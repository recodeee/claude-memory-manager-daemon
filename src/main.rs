//! claude-memory-manager-daemon — `cmmd`
//!
//! Long-running daemon. Each tick:
//!   1. refresh authmux snapshot (always shows who's logged in)
//!   2. stat MEMORY_ROOT
//!   3. abort if another live claude session exists OR memory was touched recently
//!   4. otherwise spawn `claude -p ...` against the memory-manager subagent
//!
//! A Unix socket at $STATUS_SOCK lets `mmctl status` query the running daemon.

use claude_memory_manager_daemon::{
    audit, authmux, config, history, ipc, janitor, memory, metrics, orphan_node, pressure, process,
    state, tick, tmux_janitor, webhook,
};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

#[derive(Parser)]
#[command(name = "cmmd", version, about = "Claude Memory Manager Daemon")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon (default).
    Run {
        /// Run exactly one tick then exit.
        #[arg(long)]
        once: bool,
    },
    /// Print resolved config + a one-shot authmux+memory snapshot, then exit.
    Doctor,
    /// Deterministic Rust-side audit of MEMORY_ROOT. No claude spawn, no
    /// token cost. Use to see what would trigger the next tick.
    Audit {
        /// Override MEMORY_ROOT for this run.
        #[arg(long)]
        memory_root: Option<std::path::PathBuf>,
        /// Emit JSON instead of summary line.
        #[arg(long)]
        json: bool,
    },
    /// Process janitor: list / clean up stale Claude / Codex / Kiro sessions.
    Janitor {
        #[command(subcommand)]
        action: JanitorAction,
    },
    /// Show tick history (tail of $HISTORY_FILE, newest first).
    History {
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
        #[arg(long)]
        json: bool,
    },
    /// `git -C MEMORY_ROOT log` — recent git snapshots of memory.
    GitLog {
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
    /// Reset MEMORY_ROOT to a prior git snapshot. Destructive.
    Restore {
        /// SHA from `cmmd git-log`. Must be a real commit on the MEMORY_ROOT git.
        sha: String,
    },
    /// `git -C MEMORY_ROOT diff <sha>` — preview what `restore <sha>` would change.
    Diff { sha: String },
    /// Orphaned Node process reaper (mcpvault-stdio-keepalive, mcp-server.cjs, worker-service.cjs).
    OrphanNode {
        #[command(subcommand)]
        action: OrphanNodeAction,
    },
    /// Tmux unattached `term-*` session cleanup.
    TmuxJanitor {
        #[command(subcommand)]
        action: TmuxAction,
    },
    /// RAM pressure check + escalating response.
    Pressure {
        /// Run check + escalation (default = report only).
        #[arg(long)]
        respond: bool,
        #[arg(long)]
        json: bool,
    },
    /// Prune old tick transcripts + truncate history.jsonl + rotate main log.
    Vacuum {
        /// Drop transcripts older than this many days.
        #[arg(long, default_value_t = 14)]
        keep_days: u64,
        /// Keep at most this many records in history.jsonl.
        #[arg(long, default_value_t = 10_000)]
        keep_history: usize,
        /// Preview only — don't actually delete or truncate.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print just the agent's `files=N issues=N applied=N deferred=N — narrative`
    /// line from the most recent tick's transcript.
    LastTickSummary,
}

#[derive(Subcommand)]
enum OrphanNodeAction {
    /// List orphans without killing.
    List {
        #[arg(long)]
        json: bool,
    },
    /// SIGTERM all detected orphans.
    Reap {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum TmuxAction {
    /// Cleanup unattached `term-*` sessions.
    Cleanup {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum JanitorAction {
    /// List stale candidates. Read-only; never kills.
    List(JanitorOpts),
    /// Apply: send SIGTERM (then SIGKILL after 10s) to up to --max candidates.
    /// Refuses to act unless --no-dry-run is passed explicitly.
    Apply {
        #[command(flatten)]
        opts: JanitorOpts,
        /// Actually kill processes. Without this flag, apply is a no-op preview.
        #[arg(long)]
        no_dry_run: bool,
    },
}

#[derive(Args, Clone)]
struct JanitorOpts {
    /// Minimum age in hours before a process is even considered.
    /// Default is conservative-enough for typical idle CLI sessions to fall
    /// out (6h) without snagging a quietly-running interactive session.
    #[arg(long, default_value_t = 6.0)]
    min_age_hours: f64,
    /// Max CPU% over the last sample. Default keeps active sessions safe.
    #[arg(long, default_value_t = 0.5)]
    max_cpu_pct: f32,
    /// Soft cap on kills per invocation (hard ceiling is 20).
    #[arg(long, default_value_t = 5)]
    max: usize,
    /// Only consider processes with no controlling TTY (orphaned).
    /// Strongly recommended on. Disable with --no-require-no-tty.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    require_no_tty: bool,
    /// Emit JSON instead of pretty text.
    #[arg(long)]
    json: bool,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    let cfg = config::Config::load()?;

    match cli.cmd.unwrap_or(Cmd::Run { once: false }) {
        Cmd::Doctor => run_doctor(cfg).await,
        Cmd::Run { once } => run_daemon(cfg, once).await,
        Cmd::Audit { memory_root, json } => run_audit_cmd(cfg, memory_root, json),
        Cmd::Janitor { action } => run_janitor(action),
        Cmd::History { lines, json } => run_history(cfg, lines, json),
        Cmd::GitLog { lines } => run_git_log(cfg, lines),
        Cmd::Restore { sha } => run_restore(cfg, sha),
        Cmd::Diff { sha } => run_diff(cfg, sha),
        Cmd::OrphanNode { action } => run_orphan_node(action),
        Cmd::TmuxJanitor { action } => run_tmux_janitor(action),
        Cmd::Pressure { respond, json } => run_pressure(respond, json),
        Cmd::Vacuum {
            keep_days,
            keep_history,
            dry_run,
        } => run_vacuum(cfg, keep_days, keep_history, dry_run),
        Cmd::LastTickSummary => run_last_tick_summary(cfg),
    }
}

fn run_orphan_node(action: OrphanNodeAction) -> Result<()> {
    match action {
        OrphanNodeAction::List { json } => {
            let orphans = orphan_node::find_orphans();
            if json {
                println!("{}", serde_json::to_string_pretty(&orphans)?);
            } else if orphans.is_empty() {
                println!("(no orphan node processes)");
            } else {
                println!("found {} orphan node process(es):", orphans.len());
                for o in &orphans {
                    println!(
                        "  pid={} ppid={} rss_kb={} cmd={}",
                        o.pid,
                        o.ppid,
                        o.rss_kb,
                        o.cmdline.chars().take(80).collect::<String>()
                    );
                }
            }
            Ok(())
        }
        OrphanNodeAction::Reap { json } => {
            let result = orphan_node::reap_orphans();
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("scanned={} reaped={}", result.scanned, result.reaped.len());
                for o in &result.reaped {
                    println!(
                        "  killed pid={} ({})",
                        o.pid,
                        o.cmdline.chars().take(60).collect::<String>()
                    );
                }
            }
            Ok(())
        }
    }
}

fn run_tmux_janitor(action: TmuxAction) -> Result<()> {
    match action {
        TmuxAction::Cleanup { json } => {
            let result = tmux_janitor::cleanup_unattached();
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("scanned={} killed={}", result.scanned, result.killed.len());
                for name in &result.killed {
                    println!("  killed session {name}");
                }
            }
            Ok(())
        }
    }
}

fn run_pressure(respond: bool, json: bool) -> Result<()> {
    let result = if respond {
        pressure::check_and_respond()
    } else {
        pressure::PressureResponse {
            mem: pressure::read_meminfo(),
            actions_taken: vec![],
            threshold_exceeded: false,
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "ram: total={} MB available={} MB used={}%{}",
            result.mem.total_kb / 1024,
            result.mem.available_kb / 1024,
            result.mem.used_pct,
            if result.threshold_exceeded {
                " (PRESSURE)"
            } else {
                ""
            }
        );
        for a in &result.actions_taken {
            println!("  action: {a}");
        }
    }
    Ok(())
}

fn run_vacuum(
    cfg: config::Config,
    keep_days: u64,
    keep_history: usize,
    dry_run: bool,
) -> Result<()> {
    use std::time::{Duration, SystemTime};
    let cutoff = SystemTime::now() - Duration::from_secs(keep_days * 86400);

    let mut transcripts_removed = 0;
    let mut transcripts_kept = 0;
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for e in entries.flatten() {
            let name = e.file_name();
            let n = name.to_string_lossy();
            if !n.starts_with("cmmd-tick-") || !n.ends_with(".log") {
                continue;
            }
            let meta = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if mtime < cutoff {
                if !dry_run {
                    let _ = std::fs::remove_file(e.path());
                }
                transcripts_removed += 1;
            } else {
                transcripts_kept += 1;
            }
        }
    }

    let mut history_before: usize = 0;
    let mut history_after: usize = 0;
    if cfg.history_file.is_file() {
        let body = std::fs::read_to_string(&cfg.history_file).unwrap_or_default();
        let lines: Vec<&str> = body.lines().collect();
        history_before = lines.len();
        if lines.len() > keep_history {
            history_after = keep_history;
            if !dry_run {
                let kept = lines[lines.len() - keep_history..].join("\n");
                let mut out = kept;
                out.push('\n');
                let tmp = cfg.history_file.with_extension("tmp");
                std::fs::write(&tmp, out)?;
                std::fs::rename(&tmp, &cfg.history_file)?;
            }
        } else {
            history_after = lines.len();
        }
    }

    println!(
        "{}vacuum: transcripts_removed={} transcripts_kept={} history_before={} history_after={} keep_days={} keep_history={}",
        if dry_run { "[dry-run] " } else { "" },
        transcripts_removed, transcripts_kept, history_before, history_after, keep_days, keep_history
    );
    Ok(())
}

fn run_last_tick_summary(cfg: config::Config) -> Result<()> {
    let recs = history::tail(&cfg.history_file, 1);
    let rec = match recs.first() {
        Some(r) => r,
        None => {
            println!("(no ticks recorded)");
            return Ok(());
        }
    };
    let transcript = std::path::PathBuf::from(format!("/tmp/cmmd-tick-{}.log", rec.tick_id));
    let body = match std::fs::read_to_string(&transcript) {
        Ok(s) => s,
        Err(_) => {
            println!(
                "(no transcript at {}; tick may have been skipped — reason: {:?})",
                transcript.display(),
                rec.reason_skipped
            );
            return Ok(());
        }
    };
    // Walk the transcript backwards and find the last line that looks like the
    // agent's structured summary.
    let summary = body
        .lines()
        .rev()
        .find(|l| l.contains("files=") && l.contains("issues=") && l.contains("applied="))
        .unwrap_or("(no `files=... issues=... applied=...` line found in transcript)");
    println!("tick_id    : {}", rec.tick_id);
    println!("started_at : {}", rec.started_at_unix);
    println!(
        "duration   : {}s",
        rec.finished_at_unix.saturating_sub(rec.started_at_unix)
    );
    println!("ran        : {}  exit={:?}", rec.ran, rec.exit_code);
    println!("audit_issues: {}", rec.audit_total_issues);
    println!("summary    : {}", summary.trim());
    Ok(())
}

fn run_diff(cfg: config::Config, sha: String) -> Result<()> {
    if !cfg.memory_root.join(".git").exists() {
        return Err(anyhow::anyhow!(
            "{} is not a git repo — nothing to diff",
            cfg.memory_root.display()
        ));
    }
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&cfg.memory_root)
        .arg("diff")
        .arg(&sha)
        .status()?;
    if !status.success() {
        return Err(anyhow::anyhow!("git diff exited non-zero"));
    }
    Ok(())
}

fn run_history(cfg: config::Config, n: usize, json: bool) -> Result<()> {
    let recs = history::tail(&cfg.history_file, n);
    if json {
        println!("{}", serde_json::to_string_pretty(&recs)?);
        return Ok(());
    }
    if recs.is_empty() {
        println!("(no history at {})", cfg.history_file.display());
        return Ok(());
    }
    for r in &recs {
        let when = chrono::DateTime::<chrono::Utc>::from_timestamp(r.started_at_unix as i64, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| r.started_at_unix.to_string());
        let dur = r.finished_at_unix.saturating_sub(r.started_at_unix);
        let status = if r.ran {
            format!("ran exit={:?} issues={}", r.exit_code, r.audit_total_issues)
        } else {
            format!(
                "skipped: {}",
                r.reason_skipped.as_deref().unwrap_or("(no reason)")
            )
        };
        let sha = r.pre_tick_sha.as_deref().unwrap_or("-");
        println!(
            "{when}  +{dur:>3}s  dry={}  sha={:.10}  {status}",
            r.dry_run, sha
        );
    }
    Ok(())
}

fn run_git_log(cfg: config::Config, n: usize) -> Result<()> {
    let entries = history::log_entries(&cfg.memory_root, n)?;
    if entries.is_empty() {
        println!(
            "(no git history at {}; either git_track is off or MEMORY_ROOT isn't a git repo yet)",
            cfg.memory_root.display()
        );
        return Ok(());
    }
    for e in &entries {
        println!("{:.10}  {}  {}", e.sha, e.date_iso, e.subject);
    }
    Ok(())
}

fn run_restore(cfg: config::Config, sha: String) -> Result<()> {
    if !cfg.memory_root.join(".git").exists() {
        return Err(anyhow::anyhow!(
            "{} is not a git repo — nothing to restore",
            cfg.memory_root.display()
        ));
    }
    history::restore(&cfg.memory_root, &sha)?;
    println!(
        "restored {} to {}",
        cfg.memory_root.display(),
        &sha[..sha.len().min(10)]
    );
    Ok(())
}

fn run_audit_cmd(
    cfg: config::Config,
    memory_root: Option<std::path::PathBuf>,
    json: bool,
) -> Result<()> {
    let root = memory_root.unwrap_or(cfg.memory_root);
    let report = audit::run_audit(&root);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", report.summary());
        if report.total_issues() == 0 {
            return Ok(());
        }
        if report.memory_md_oversize {
            println!(
                "  ⚠ MEMORY.md is {} lines (>200 budget)",
                report.memory_md_lines
            );
        }
        for f in &report.missing_frontmatter {
            println!("  ✗ missing frontmatter: {f}");
        }
        for t in &report.invalid_type {
            println!("  ✗ invalid type='{}': {}", t.got, t.file);
        }
        for d in &report.dangling_index_entries {
            println!("  ✗ MEMORY.md points at missing file: {d}");
        }
        for u in &report.missing_from_index {
            println!("  ✗ file not in MEMORY.md: {u}");
        }
        for w in &report.broken_wikilinks {
            println!("  ✗ broken [[{}]] in {}", w.to_slug, w.from);
        }
        for m in &report.missing_why_or_how {
            println!("  ✗ feedback/project missing Why/How: {m}");
        }
        for p in &report.duplicate_candidates {
            println!("  ⚠ likely dupes: {} / {} ({})", p.a, p.b, p.reason);
        }
    }
    Ok(())
}

fn run_janitor(action: JanitorAction) -> Result<()> {
    match action {
        JanitorAction::List(opts) => {
            let jopts = janitor::Opts::new(
                opts.min_age_hours,
                opts.max_cpu_pct,
                opts.max,
                true,
                opts.require_no_tty,
            );
            let stale = janitor::list_stale(&jopts);
            if opts.json {
                println!("{}", serde_json::to_string_pretty(&stale)?);
            } else {
                pretty_print_stale(&stale, &jopts);
            }
        }
        JanitorAction::Apply { opts, no_dry_run } => {
            let jopts = janitor::Opts::new(
                opts.min_age_hours,
                opts.max_cpu_pct,
                opts.max,
                !no_dry_run,
                opts.require_no_tty,
            );
            let outcome = janitor::apply(&jopts)?;
            if opts.json {
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                pretty_print_apply(&outcome);
            }
        }
    }
    Ok(())
}

fn pretty_print_stale(stale: &[janitor::StaleProc], opts: &janitor::Opts) {
    println!(
        "janitor list — min-age={}s max-cpu={}% (allowlist: {:?})",
        opts.min_age_sec,
        opts.max_cpu_pct,
        janitor::ALLOWLIST
    );
    println!(
        "{:>8} {:>8} {:<18} {:>10} {:>8} {:>10}",
        "pid", "ppid", "name", "age", "cpu%", "rss_kb"
    );
    for p in stale {
        let h = p.age_sec / 3600;
        let m = (p.age_sec % 3600) / 60;
        println!(
            "{:>8} {:>8} {:<18} {:>7}h{:02}m {:>7.1} {:>10}",
            p.pid,
            p.ppid.map(|x| x.to_string()).unwrap_or_else(|| "-".into()),
            truncate(&p.name, 18),
            h,
            m,
            p.cpu_pct,
            p.rss_kb
        );
    }
    if stale.is_empty() {
        println!("  (no stale processes match the rules)");
    }
}

fn pretty_print_apply(out: &janitor::ApplyOutcome) {
    println!(
        "janitor apply — considered={} attempted={} dry_run={}",
        out.considered,
        out.attempted.len(),
        out.dry_run
    );
    for r in &out.attempted {
        if out.dry_run {
            println!(
                "  would kill pid={} name={} age_sec={}",
                r.pid, r.name, r.age_sec
            );
        } else {
            println!(
                "  pid={} name={} sigterm_ok={} survived_grace={} sigkill={:?}",
                r.pid, r.name, r.sigterm_ok, r.still_alive_after_grace, r.sigkill_ok
            );
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n - 1])
    }
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("CMMD_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

async fn run_doctor(cfg: config::Config) -> Result<()> {
    let am = authmux::snapshot(&cfg.authmux_bin).await;
    let mem = memory::stat(&cfg.memory_root);
    let acct = authmux::claude_account_dirs(&cfg.claude_accounts_dir);
    let claude_ok = tick::claude_bin_available(&cfg.claude_bin);

    let report = serde_json::json!({
        "config": &cfg,
        "claude_bin_available": claude_ok,
        "authmux": &am,
        "memory": &mem,
        "claude_account_dirs": &acct,
        "live_claude_sessions": process::find_claude_sessions(),
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn run_daemon(cfg: config::Config, once: bool) -> Result<()> {
    ipc::acquire_lock(&cfg.lock_file, &cfg.pid_file)?;
    info!(
        pid = std::process::id(),
        memory = %cfg.memory_root.display(),
        dry_run = cfg.dry_run,
        "daemon up"
    );

    if !tick::claude_bin_available(&cfg.claude_bin) {
        warn!(
            claude_bin = %cfg.claude_bin,
            "claude binary not found on $PATH — ticks will fail. Set CLAUDE_BIN in .env."
        );
    }

    // Apply any persisted runtime override (e.g. `mmctl dry-run off` from a
    // previous run). Logged so the operator can see the override kicked in.
    let persisted = state::load(&cfg.state_file);
    let initial_dry_run = persisted.dry_run_override.unwrap_or(cfg.dry_run);
    if persisted.dry_run_override.is_some() {
        info!(persisted = ?persisted, configured = cfg.dry_run, effective = initial_dry_run,
              "applied persisted dry_run override");
    }

    let started_at = now_unix();
    let status = ipc::DaemonStatus {
        pid: std::process::id(),
        started_at_unix: started_at,
        dry_run: initial_dry_run,
        model: cfg.model.clone(),
        memory_root: cfg.memory_root.display().to_string(),
        last_tick: None,
        authmux: serde_json::Value::Null,
        memory: serde_json::Value::Null,
        claude_account_dirs: serde_json::Value::Null,
        config: serde_json::to_value(&cfg)?,
    };
    let state = Arc::new(RwLock::new(status));
    let tick_now = Arc::new(tokio::sync::Notify::new());
    let dry_run_runtime = Arc::new(tokio::sync::Mutex::new(initial_dry_run));
    let metrics_handle = Arc::new(metrics::Metrics::default());

    if !cfg.metrics_bind.is_empty() {
        let bind = cfg.metrics_bind.clone();
        let m = metrics_handle.clone();
        let interval_sec = cfg.tick_interval.as_secs();
        tokio::spawn(async move { metrics::serve(bind, m, interval_sec).await });
    }

    // Status socket.
    {
        let sock = cfg.status_sock.clone();
        let handles = ipc::DaemonHandles {
            state: state.clone(),
            tick_now: tick_now.clone(),
            dry_run: dry_run_runtime.clone(),
            state_file: cfg.state_file.clone(),
        };
        tokio::spawn(async move {
            if let Err(e) = ipc::serve_status(sock, handles).await {
                error!("status server died: {e}");
            }
        });
    }

    // Signal handling — clean shutdown on SIGTERM / SIGINT.
    let shutdown = tokio::spawn(install_signal_handler());

    let cfg_arc = Arc::new(cfg);
    let result = main_loop(
        cfg_arc.clone(),
        state.clone(),
        tick_now.clone(),
        dry_run_runtime.clone(),
        metrics_handle.clone(),
        once,
        shutdown,
    )
    .await;

    ipc::release_lock(&cfg_arc.lock_file, &cfg_arc.pid_file);
    info!("daemon down");
    result
}

async fn main_loop(
    cfg: Arc<config::Config>,
    state: Arc<RwLock<ipc::DaemonStatus>>,
    tick_now: Arc<tokio::sync::Notify>,
    dry_run_runtime: Arc<tokio::sync::Mutex<bool>>,
    metrics_handle: Arc<metrics::Metrics>,
    once: bool,
    mut shutdown: tokio::task::JoinHandle<()>,
) -> Result<()> {
    loop {
        let tick_started = now_unix();

        // --- Growth control ---
        // 1. Rotate history.jsonl if oversize. The file accumulates one row
        //    per tick per root forever otherwise.
        // 2. Sweep per-tick agent transcripts (/tmp/cmmd-tick-*.log) older
        //    than the configured TTL — these are the real disk eater.
        // Both run once per iteration before any per-root work so a slow
        // tick can't postpone cleanup.
        match history::rotate_if_oversize(&cfg.history_file, cfg.history_max_lines) {
            Ok(true) => {
                info!(file = %cfg.history_file.display(), "history rotated");
                metrics_handle.record_history_rotation();
            }
            Ok(false) => {}
            Err(e) => warn!("history rotate failed: {e}"),
        }
        let swept = history::sweep_tick_logs(cfg.tick_log_ttl_days);
        if swept > 0 {
            info!(count = swept, "swept stale tick transcripts");
            metrics_handle.record_tick_logs_swept(swept as u64);
        }

        // --- Daily-tick budget ---
        // Roll the per-day counter if UTC date has changed; persist so the
        // ceiling survives a restart within the same day.
        let today = state::utc_date_string(tick_started);
        let mut persisted = state::load(&cfg.state_file);
        state::bump_day_if_needed(&mut persisted, &today);
        metrics_handle.set_day_ticks_ran(persisted.day_ticks_ran);
        let daily_cap_reached = cfg.max_ticks_per_day > 0
            && persisted.day_ticks_ran >= cfg.max_ticks_per_day;
        if daily_cap_reached {
            warn!(
                ran = persisted.day_ticks_ran,
                cap = cfg.max_ticks_per_day,
                date = %today,
                "MAX_TICKS_PER_DAY reached — agent will not be spawned this iteration"
            );
        }
        if let Err(e) = state::save(&cfg.state_file, &persisted) {
            warn!("persist state failed: {e}");
        }

        // Always refresh authmux + account dirs first (memory stat updates
        // per-root below). This is what makes "who is logged in" visible at
        // any time via `mmctl status`.
        let am = authmux::snapshot(&cfg.authmux_bin).await;
        let accts = authmux::claude_account_dirs(&cfg.claude_accounts_dir);
        {
            let mut s = state.write().await;
            s.authmux = serde_json::to_value(&am)?;
            s.claude_account_dirs = serde_json::to_value(&accts)?;
        }
        log_login_summary(&am, &accts);

        // --- Housekeeping: run every tick regardless of memory-manager logic ---
        // 1. Kill orphaned tmux sessions
        let tmux_result = tmux_janitor::cleanup_unattached();
        if !tmux_result.killed.is_empty() {
            info!(killed = ?tmux_result.killed, "tmux janitor");
        }
        // 2. Reap orphaned node processes (mcpvault, mcp-server, worker-service)
        let orphan_result = orphan_node::reap_orphans();
        if !orphan_result.reaped.is_empty() {
            info!(count = orphan_result.reaped.len(), "orphan node reaper");
        }
        // 3. RAM pressure response
        let pressure_result = pressure::check_and_respond();
        if pressure_result.threshold_exceeded {
            info!(
                used_pct = pressure_result.mem.used_pct,
                actions = ?pressure_result.actions_taken,
                "pressure response"
            );
        }

        let dry_run_now = *dry_run_runtime.lock().await;
        state.write().await.dry_run = dry_run_now;

        // Iterate every configured MEMORY_ROOT this tick. Each gets its own
        // lsof + audit + (optional) spawn. The published `memory` field on
        // status reflects the PRIMARY root for backward compatibility; per-
        // root details live in history.jsonl.
        let mut roots = cfg.all_memory_roots();
        for (i, root) in roots.iter_mut().enumerate() {
            let mem = memory::stat(root);
            if i == 0 {
                state.write().await.memory = serde_json::to_value(&mem)?;
            }
            info!(root = %root.display(), "tending memory root");
            let tick_id = history::new_tick_id();

            let outcome = if daily_cap_reached {
                // Cheap path: refuse to spawn, but still record the tick so
                // history/metrics reflect that we hit the cap.
                metrics_handle.record_daily_cap_block();
                tick::TickOutcome {
                    ran: false,
                    reason_skipped: Some(format!(
                        "MAX_TICKS_PER_DAY={} reached for {}",
                        cfg.max_ticks_per_day, today
                    )),
                    exit_code: None,
                    audit_total_issues: 0,
                    pre_tick_sha: None,
                }
            } else {
                match tick::run(&cfg, root, dry_run_now, &mem, &tick_id).await {
                    Ok(o) => o,
                    Err(e) => {
                        error!(root = %root.display(), "tick error: {e:#}");
                        tick::TickOutcome {
                            ran: false,
                            reason_skipped: Some(format!("{e}")),
                            exit_code: None,
                            audit_total_issues: 0,
                            pre_tick_sha: None,
                        }
                    }
                }
            };

            // --- Enforce MAX_FIXES_PER_TICK ---
            // The "≤3 fixes per tick" rule used to live only in the agent
            // prompt. If the agent ignored it (or hallucinated more edits),
            // there was no daemon-side guard. Now: after a non-dry-run tick
            // that produced changes, count dirty files vs the pre-tick SHA
            // and roll the whole tick back if it exceeded the cap.
            if outcome.ran
                && !dry_run_now
                && cfg.max_fixes_per_tick > 0
                && cfg.git_track
            {
                if let Some(pre_sha) = outcome.pre_tick_sha.as_ref() {
                    match history::count_dirty_files(root) {
                        Ok(n) if (n as u64) > cfg.max_fixes_per_tick => {
                            warn!(
                                changed = n,
                                cap = cfg.max_fixes_per_tick,
                                "agent exceeded MAX_FIXES_PER_TICK — reverting to pre-tick snapshot"
                            );
                            if let Err(e) = history::restore(root, pre_sha) {
                                error!("revert to pre-tick SHA failed: {e}");
                            } else {
                                metrics_handle.record_fix_cap_revert();
                            }
                        }
                        Ok(_) => {}
                        Err(e) => warn!("could not count dirty files: {e}"),
                    }
                }
            }

            // --- Persisted daily-tick counter ---
            // Only count *ran* ticks against the daily cap — skipped ticks
            // (lsof guard, audit-clean) cost nothing. Re-read state to avoid
            // clobbering any concurrent mmctl mutation.
            if outcome.ran {
                let mut p = state::load(&cfg.state_file);
                state::bump_day_if_needed(&mut p, &today);
                p.day_ticks_ran = p.day_ticks_ran.saturating_add(1);
                metrics_handle.set_day_ticks_ran(p.day_ticks_ran);
                if let Err(e) = state::save(&cfg.state_file, &p) {
                    warn!("persist day_ticks_ran failed: {e}");
                }
            }

            // --- Periodic git gc on this root ---
            // Without this the memory dir's .git/objects grows forever.
            // Best-effort — failures are logged, never fatal.
            match history::git_gc_if_due(root, cfg.git_gc_interval_days) {
                Ok(true) => {
                    info!(root = %root.display(), "git gc complete");
                    metrics_handle.record_git_gc();
                }
                Ok(false) => {}
                Err(e) => warn!(root = %root.display(), "git gc failed: {e}"),
            }

            let now = now_unix();
            if let Some(r) = &outcome.reason_skipped {
                info!(root = %root.display(), reason = %r, "tick skipped");
            } else {
                info!(root = %root.display(), exit = ?outcome.exit_code, "tick complete");
            }

            // Only the primary root drives state.last_tick (kept for mmctl
            // last-tick / status JSON compatibility).
            if i == 0 {
                let rec = ipc::TickRecord {
                    started_at_unix: tick_started,
                    finished_at_unix: now,
                    ran: outcome.ran,
                    reason_skipped: outcome.reason_skipped.clone(),
                    exit_code: outcome.exit_code,
                };
                state.write().await.last_tick = Some(rec);
            }

            let hist_rec = history::TickRecord {
                tick_id: tick_id.clone(),
                started_at_unix: tick_started,
                finished_at_unix: now,
                dry_run: dry_run_now,
                memory_root: root.display().to_string(),
                ran: outcome.ran,
                reason_skipped: outcome.reason_skipped.clone(),
                exit_code: outcome.exit_code,
                audit_total_issues: outcome.audit_total_issues,
                pre_tick_sha: outcome.pre_tick_sha.clone(),
            };
            match history::append(&cfg.history_file, &hist_rec) {
                Ok(()) => metrics_handle.record_history_append(),
                Err(e) => warn!("history append failed: {e}"),
            }
            metrics_handle.record_tick(
                outcome.ran,
                now.saturating_sub(tick_started),
                outcome.audit_total_issues as u64,
                outcome.exit_code,
            );

            // Webhook delivery — fire-and-forget. Spawned so the loop never
            // blocks on a slow upstream.
            if !cfg.webhook_url.is_empty() {
                let url = cfg.webhook_url.clone();
                let payload = webhook::TickWebhookPayload {
                    tick_id: hist_rec.tick_id.clone(),
                    started_at_unix: hist_rec.started_at_unix,
                    finished_at_unix: hist_rec.finished_at_unix,
                    memory_root: hist_rec.memory_root.clone(),
                    dry_run: hist_rec.dry_run,
                    ran: hist_rec.ran,
                    reason_skipped: hist_rec.reason_skipped.clone(),
                    exit_code: hist_rec.exit_code,
                    audit_total_issues: hist_rec.audit_total_issues,
                    pre_tick_sha: hist_rec.pre_tick_sha.clone(),
                };
                tokio::spawn(async move {
                    webhook::post(&url, &payload).await;
                });
            }
        }

        if once {
            return Ok(());
        }

        // Sleep until the next scheduled tick, OR until someone pokes us via
        // `mmctl tick`, OR until we get SIGTERM.
        tokio::select! {
            _ = tokio::time::sleep(cfg.tick_interval) => {}
            _ = tick_now.notified() => { info!("tick requested via socket"); }
            _ = &mut shutdown => {
                info!("shutdown signal received");
                return Ok(());
            }
        }
    }
}

fn log_login_summary(am: &authmux::AuthmuxSnapshot, accts: &[authmux::ClaudeAccountDir]) {
    let active = am.current.as_deref().unwrap_or("<none>");
    let n_authmux = am.accounts.len();
    let n_dirs = accts.iter().filter(|a| a.has_credentials).count();
    info!(
        authmux_available = am.available,
        authmux_active = active,
        authmux_total = n_authmux,
        claude_account_dirs_with_creds = n_dirs,
        "login snapshot"
    );
}

async fn install_signal_handler() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM");
    let mut intr = signal(SignalKind::interrupt()).expect("install SIGINT");
    tokio::select! {
        _ = term.recv() => {}
        _ = intr.recv() => {}
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
