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
    audit, authmux, config, history, ipc, janitor, memory, metrics, process, state, tick,
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
    }
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
        tokio::spawn(async move { metrics::serve(bind, m).await });
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

            let outcome = match tick::run(&cfg, root, dry_run_now, &mem).await {
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
            };

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
                tick_id: history::new_tick_id(),
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
