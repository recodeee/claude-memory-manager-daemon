//! mmctl — companion CLI for the running `cmmd` daemon.
//!
//! Some commands hit the daemon over its Unix socket (status, accounts,
//! memory, last-tick, ping, tick, dry-run-on/off). Others shell out:
//!
//!   - `logs` tails $LOG_FILE
//!   - `janitor list/apply` shells out to `cmmd janitor`
//!   - `accounts switch` shells out to `authmux switch`
//!   - `plugins` manages folders under .claude/skills/

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use claude_memory_manager_daemon::ipc;

#[derive(Parser)]
#[command(
    name = "mmctl",
    version,
    about = "Control + inspect the claude-memory-manager-daemon"
)]
struct Cli {
    /// Daemon's Unix socket path.
    #[arg(
        long,
        env = "STATUS_SOCK",
        default_value = "/tmp/claude-memory-manager.sock"
    )]
    sock: PathBuf,
    /// Daemon's log file (used by `logs`).
    #[arg(
        long,
        env = "LOG_FILE",
        default_value = "/tmp/claude-memory-manager.log"
    )]
    log_file: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Full daemon status as pretty JSON.
    Status,
    /// authmux block: who is logged in, who is active, usage % per row.
    Accounts(AccountsArgs),
    /// Memory-root stats only.
    Memory,
    /// Last tick record only.
    LastTick,
    /// Liveness probe.
    Ping,
    /// Trigger an immediate tick (bypasses the inter-tick sleep).
    Tick {
        /// Block until the daemon reports a finished tick newer than
        /// the one in state at request time. Max-wait is bounded by --timeout.
        #[arg(long)]
        wait: bool,
        /// Wait timeout in seconds when --wait is set.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Toggle runtime dry-run mode on the running daemon.
    DryRun {
        /// "on" or "off".
        state: String,
    },
    /// Tail the daemon log file.
    Logs {
        /// Number of trailing lines.
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Follow new output (Ctrl-C to exit).
        #[arg(short, long)]
        follow: bool,
    },
    /// Deterministic Rust-side audit of MEMORY_ROOT (no claude spawn).
    Audit {
        #[arg(long)]
        memory_root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Tail the tick history (JSONL append-only log).
    History {
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
        #[arg(long)]
        json: bool,
    },
    /// git -C MEMORY_ROOT log — recent pre-tick snapshots.
    GitLog {
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
    /// git diff MEMORY_ROOT against a snapshot SHA. Preview before restore.
    Diff { sha: String },
    /// Reset MEMORY_ROOT to a prior snapshot (destructive).
    Restore { sha: String },
    /// Print the full agent transcript for a tick (from /tmp/cmmd-tick-&lt;id&gt;.log).
    TickLog { tick_id: String },
    /// Process janitor: list / clean stale claude/codex/kiro sessions.
    Janitor {
        #[command(subcommand)]
        action: JanitorCmd,
    },
    /// Skill plugin manager (folders under .claude/skills/).
    Plugins {
        #[command(subcommand)]
        action: PluginsCmd,
    },
    /// Show just the agent's final `files=N issues=N applied=N ...` line.
    LastTickSummary,
    /// Prune old tick transcripts + truncate history.jsonl.
    Vacuum {
        #[arg(long, default_value_t = 14)]
        keep_days: u64,
        #[arg(long, default_value_t = 10_000)]
        keep_history: usize,
        #[arg(long)]
        dry_run: bool,
    },
    /// Orphan Node process reaper (mcpvault-stdio-keepalive, mcp-server.cjs, worker-service.cjs).
    OrphanNode {
        #[command(subcommand)]
        action: OrphanNodeCmd,
    },
    /// Tmux unattached term-* session cleanup.
    TmuxJanitor {
        #[arg(long)]
        json: bool,
    },
    /// RAM pressure check + optional escalating response.
    Pressure {
        #[arg(long)]
        respond: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum OrphanNodeCmd {
    List {
        #[arg(long)]
        json: bool,
    },
    Reap {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
struct AccountsArgs {
    /// "switch" to a specific authmux account by email fragment.
    #[arg(long)]
    switch: Option<String>,
}

#[derive(Subcommand)]
enum JanitorCmd {
    /// List stale candidates (read-only).
    List {
        #[arg(long, default_value_t = 24.0)]
        min_age_hours: f64,
        #[arg(long)]
        json: bool,
    },
    /// Apply: preview by default, or `--no-dry-run` to actually kill.
    Apply {
        #[arg(long, default_value_t = 24.0)]
        min_age_hours: f64,
        #[arg(long, default_value_t = 5)]
        max: usize,
        #[arg(long)]
        no_dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum PluginsCmd {
    /// List installed skills under .claude/skills/ (enabled + disabled).
    List,
    /// Install a skill from a local folder or a git URL.
    /// Local: mmctl plugins install /path/to/my-skill
    /// Git:   mmctl plugins install https://github.com/foo/bar
    Install { source: String },
    /// Remove a skill folder (and any disabled mirror).
    Remove { name: String },
    /// Move skill into .claude/skills-disabled/.
    Disable { name: String },
    /// Move skill back into .claude/skills/.
    Enable { name: String },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Status => cmd_status(&cli.sock).await,
        Cmd::Accounts(args) => cmd_accounts(&cli.sock, args).await,
        Cmd::Memory => cmd_memory(&cli.sock).await,
        Cmd::LastTick => cmd_last_tick(&cli.sock).await,
        Cmd::Ping => cmd_ping(&cli.sock).await,
        Cmd::Tick { wait, timeout } => cmd_tick(&cli.sock, wait, timeout).await,
        Cmd::DryRun { state } => cmd_dry_run(&cli.sock, &state).await,
        Cmd::Logs { lines, follow } => cmd_logs(&cli.log_file, lines, follow),
        Cmd::Audit { memory_root, json } => cmd_audit(memory_root, json),
        Cmd::History { lines, json } => cmd_proxy_history(lines, json),
        Cmd::GitLog { lines } => cmd_proxy_git_log(lines),
        Cmd::Diff { sha } => cmd_proxy_diff(sha),
        Cmd::Restore { sha } => cmd_proxy_restore(sha),
        Cmd::TickLog { tick_id } => cmd_tick_log(tick_id),
        Cmd::Janitor { action } => cmd_janitor(action),
        Cmd::Plugins { action } => cmd_plugins(action),
        Cmd::LastTickSummary => cmd_proxy_last_summary(),
        Cmd::Vacuum {
            keep_days,
            keep_history,
            dry_run,
        } => cmd_proxy_vacuum(keep_days, keep_history, dry_run),
        Cmd::OrphanNode { action } => cmd_proxy_orphan_node(action),
        Cmd::TmuxJanitor { json } => cmd_proxy_tmux_janitor(json),
        Cmd::Pressure { respond, json } => cmd_proxy_pressure(respond, json),
    }
}

fn cmd_proxy_last_summary() -> Result<()> {
    let cmmd = locate_cmmd()?;
    let mut c = Command::new(&cmmd);
    c.arg("last-tick-summary");
    run_inherit(c)
}

fn cmd_proxy_vacuum(keep_days: u64, keep_history: usize, dry_run: bool) -> Result<()> {
    let cmmd = locate_cmmd()?;
    let mut c = Command::new(&cmmd);
    c.arg("vacuum")
        .arg("--keep-days")
        .arg(keep_days.to_string())
        .arg("--keep-history")
        .arg(keep_history.to_string());
    if dry_run {
        c.arg("--dry-run");
    }
    run_inherit(c)
}

fn cmd_proxy_orphan_node(action: OrphanNodeCmd) -> Result<()> {
    let cmmd = locate_cmmd()?;
    let mut c = Command::new(&cmmd);
    c.arg("orphan-node");
    match action {
        OrphanNodeCmd::List { json } => {
            c.arg("list");
            if json {
                c.arg("--json");
            }
        }
        OrphanNodeCmd::Reap { json } => {
            c.arg("reap");
            if json {
                c.arg("--json");
            }
        }
    }
    run_inherit(c)
}

fn cmd_proxy_tmux_janitor(json: bool) -> Result<()> {
    let cmmd = locate_cmmd()?;
    let mut c = Command::new(&cmmd);
    c.arg("tmux-janitor").arg("cleanup");
    if json {
        c.arg("--json");
    }
    run_inherit(c)
}

fn cmd_proxy_pressure(respond: bool, json: bool) -> Result<()> {
    let cmmd = locate_cmmd()?;
    let mut c = Command::new(&cmmd);
    c.arg("pressure");
    if respond {
        c.arg("--respond");
    }
    if json {
        c.arg("--json");
    }
    run_inherit(c)
}

fn cmd_proxy_history(n: usize, json: bool) -> Result<()> {
    let cmmd = locate_cmmd()?;
    let mut c = Command::new(&cmmd);
    c.arg("history").arg("-n").arg(n.to_string());
    if json {
        c.arg("--json");
    }
    run_inherit(c)
}

fn cmd_proxy_git_log(n: usize) -> Result<()> {
    let cmmd = locate_cmmd()?;
    let mut c = Command::new(&cmmd);
    c.arg("git-log").arg("-n").arg(n.to_string());
    run_inherit(c)
}

fn cmd_proxy_restore(sha: String) -> Result<()> {
    let cmmd = locate_cmmd()?;
    let mut c = Command::new(&cmmd);
    c.arg("restore").arg(sha);
    run_inherit(c)
}

fn cmd_audit(memory_root: Option<PathBuf>, json: bool) -> Result<()> {
    let cmmd = locate_cmmd()?;
    let mut c = Command::new(&cmmd);
    c.arg("audit");
    if let Some(p) = memory_root {
        c.arg("--memory-root").arg(p);
    }
    if json {
        c.arg("--json");
    }
    run_inherit(c)
}

// ---------- daemon-socket commands ----------

async fn fetch_status(sock: &Path) -> Result<ipc::DaemonStatus> {
    ipc::query_status(sock)
        .await
        .with_context(|| format!("daemon not reachable on {}", sock.display()))
}

async fn cmd_status(sock: &Path) -> Result<()> {
    let s = fetch_status(sock).await?;
    println!("{}", serde_json::to_string_pretty(&s)?);
    Ok(())
}

async fn cmd_accounts(sock: &Path, args: AccountsArgs) -> Result<()> {
    if let Some(query) = args.switch {
        // Switch via authmux directly — does not need the daemon.
        let status = Command::new("authmux").arg("switch").arg(&query).status()?;
        if !status.success() {
            return Err(anyhow!("authmux switch failed"));
        }
        return Ok(());
    }
    let s = fetch_status(sock).await?;
    println!("{}", serde_json::to_string_pretty(&s.authmux)?);
    Ok(())
}

async fn cmd_memory(sock: &Path) -> Result<()> {
    let s = fetch_status(sock).await?;
    println!("{}", serde_json::to_string_pretty(&s.memory)?);
    Ok(())
}

async fn cmd_last_tick(sock: &Path) -> Result<()> {
    let s = fetch_status(sock).await?;
    println!("{}", serde_json::to_string_pretty(&s.last_tick)?);
    Ok(())
}

async fn cmd_ping(sock: &Path) -> Result<()> {
    let reply = ipc::send_command(sock, "ping")
        .await
        .with_context(|| format!("daemon not reachable on {}", sock.display()))?;
    print!("{reply}");
    Ok(())
}

async fn cmd_tick(sock: &Path, wait: bool, timeout_sec: u64) -> Result<()> {
    // Snapshot the current last-tick stamp so we know what to wait past.
    let before = ipc::query_status(sock)
        .await
        .with_context(|| format!("daemon not reachable on {}", sock.display()))?;
    let before_finished = before
        .last_tick
        .as_ref()
        .map(|t| t.finished_at_unix)
        .unwrap_or(0);

    let reply = ipc::send_command(sock, "tick")
        .await
        .with_context(|| format!("daemon not reachable on {}", sock.display()))?;
    print!("{reply}");

    if !wait {
        return Ok(());
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_sec);
    let mut poll = std::time::Duration::from_millis(250);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "--wait timed out after {timeout_sec}s without a fresh tick"
            ));
        }
        tokio::time::sleep(poll).await;
        // Exponential-ish backoff capped at 2s so we don't hammer the socket.
        poll = (poll * 2).min(std::time::Duration::from_secs(2));

        let now_status = match ipc::query_status(sock).await {
            Ok(s) => s,
            Err(_) => continue, // transient
        };
        let after_finished = now_status
            .last_tick
            .as_ref()
            .map(|t| t.finished_at_unix)
            .unwrap_or(0);
        if after_finished > before_finished {
            println!("{}", serde_json::to_string_pretty(&now_status.last_tick)?);
            return Ok(());
        }
    }
}

fn cmd_proxy_diff(sha: String) -> Result<()> {
    let cmmd = locate_cmmd()?;
    let mut c = Command::new(&cmmd);
    c.arg("diff").arg(sha);
    run_inherit(c)
}

fn cmd_tick_log(tick_id: String) -> Result<()> {
    let path = std::path::PathBuf::from(format!("/tmp/cmmd-tick-{tick_id}.log"));
    if !path.exists() {
        return Err(anyhow!("no transcript at {}", path.display()));
    }
    let status = Command::new("cat").arg(&path).status().context("cat")?;
    if !status.success() {
        return Err(anyhow!("cat exited non-zero"));
    }
    Ok(())
}

async fn cmd_dry_run(sock: &Path, state: &str) -> Result<()> {
    let cmd = match state.to_lowercase().as_str() {
        "on" | "true" | "1" => "dry-run-on",
        "off" | "false" | "0" => "dry-run-off",
        other => return Err(anyhow!("unknown state '{other}', want on|off")),
    };
    let reply = ipc::send_command(sock, cmd).await?;
    print!("{reply}");
    Ok(())
}

// ---------- log tail ----------

fn cmd_logs(log_file: &Path, lines: usize, follow: bool) -> Result<()> {
    let mut c = Command::new("tail");
    c.arg("-n").arg(lines.to_string());
    if follow {
        c.arg("-f");
    }
    c.arg(log_file);
    let status = c
        .status()
        .with_context(|| format!("spawn tail {}", log_file.display()))?;
    if !status.success() {
        return Err(anyhow!("tail exited non-zero"));
    }
    Ok(())
}

// ---------- janitor (shells out to cmmd janitor) ----------

fn cmd_janitor(action: JanitorCmd) -> Result<()> {
    let cmmd = locate_cmmd()?;
    match action {
        JanitorCmd::List {
            min_age_hours,
            json,
        } => {
            let mut c = Command::new(&cmmd);
            c.arg("janitor")
                .arg("list")
                .arg("--min-age-hours")
                .arg(min_age_hours.to_string());
            if json {
                c.arg("--json");
            }
            run_inherit(c)
        }
        JanitorCmd::Apply {
            min_age_hours,
            max,
            no_dry_run,
            json,
        } => {
            let mut c = Command::new(&cmmd);
            c.arg("janitor")
                .arg("apply")
                .arg("--min-age-hours")
                .arg(min_age_hours.to_string())
                .arg("--max")
                .arg(max.to_string());
            if no_dry_run {
                c.arg("--no-dry-run");
            }
            if json {
                c.arg("--json");
            }
            run_inherit(c)
        }
    }
}

/// Find the cmmd binary: sibling of mmctl first, then $PATH.
fn locate_cmmd() -> Result<PathBuf> {
    if let Ok(me) = std::env::current_exe() {
        if let Some(parent) = me.parent() {
            let sib = parent.join("cmmd");
            if sib.is_file() {
                return Ok(sib);
            }
        }
    }
    Ok(PathBuf::from("cmmd"))
}

fn run_inherit(mut c: Command) -> Result<()> {
    let status = c
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("spawn {:?}", c.get_program()))?;
    if !status.success() {
        return Err(anyhow!("subprocess exited {:?}", status.code()));
    }
    Ok(())
}

// ---------- plugins (skills directory management) ----------

fn skills_root() -> PathBuf {
    PathBuf::from(std::env::var("CMMD_REPO").unwrap_or_else(|_| ".".to_string()))
        .join(".claude/skills")
}
fn disabled_root() -> PathBuf {
    PathBuf::from(std::env::var("CMMD_REPO").unwrap_or_else(|_| ".".to_string()))
        .join(".claude/skills-disabled")
}

fn cmd_plugins(action: PluginsCmd) -> Result<()> {
    match action {
        PluginsCmd::List => {
            let enabled = list_dirs(&skills_root())?;
            let disabled = list_dirs(&disabled_root()).unwrap_or_default();
            println!("enabled skills ({}):", enabled.len());
            for d in &enabled {
                println!("  - {d}");
            }
            println!("\ndisabled skills ({}):", disabled.len());
            for d in &disabled {
                println!("  - {d}");
            }
            Ok(())
        }
        PluginsCmd::Install { source } => plugin_install(&source),
        PluginsCmd::Remove { name } => plugin_remove(&name),
        PluginsCmd::Disable { name } => plugin_move(&name, &skills_root(), &disabled_root()),
        PluginsCmd::Enable { name } => plugin_move(&name, &disabled_root(), &skills_root()),
    }
}

fn list_dirs(root: &Path) -> Result<Vec<String>> {
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for e in std::fs::read_dir(root)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            if let Some(s) = e.file_name().to_str() {
                out.push(s.to_string());
            }
        }
    }
    out.sort();
    Ok(out)
}

fn plugin_install(source: &str) -> Result<()> {
    let dst_root = skills_root();
    std::fs::create_dir_all(&dst_root)?;
    if source.starts_with("http") || source.starts_with("git@") {
        let tmp = std::env::temp_dir().join(format!("cmmd-plugin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let status = Command::new("git")
            .arg("clone")
            .arg("--depth")
            .arg("1")
            .arg(source)
            .arg(&tmp)
            .status()
            .context("git clone")?;
        if !status.success() {
            return Err(anyhow!("git clone failed"));
        }
        let name = tmp
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("plugin")
            .to_string();
        let dst = dst_root.join(&name);
        if dst.exists() {
            return Err(anyhow!("plugin already installed at {}", dst.display()));
        }
        std::fs::rename(&tmp, &dst)?;
        println!("installed → {}", dst.display());
        Ok(())
    } else {
        let src = PathBuf::from(source);
        if !src.is_dir() {
            return Err(anyhow!("not a directory: {source}"));
        }
        let name = src
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("bad source name"))?;
        let dst = dst_root.join(name);
        if dst.exists() {
            return Err(anyhow!("plugin already installed at {}", dst.display()));
        }
        copy_dir(&src, &dst)?;
        println!("installed → {}", dst.display());
        Ok(())
    }
}

fn plugin_remove(name: &str) -> Result<()> {
    let mut removed = 0;
    for root in [skills_root(), disabled_root()] {
        let p = root.join(name);
        if p.exists() {
            std::fs::remove_dir_all(&p).with_context(|| format!("remove {}", p.display()))?;
            println!("removed {}", p.display());
            removed += 1;
        }
    }
    if removed == 0 {
        return Err(anyhow!("no skill named '{name}'"));
    }
    Ok(())
}

fn plugin_move(name: &str, from: &Path, to: &Path) -> Result<()> {
    let src = from.join(name);
    if !src.exists() {
        return Err(anyhow!("not found: {}", src.display()));
    }
    std::fs::create_dir_all(to)?;
    let dst = to.join(name);
    if dst.exists() {
        return Err(anyhow!("already at {}", dst.display()));
    }
    std::fs::rename(&src, &dst)?;
    println!("{} → {}", src.display(), dst.display());
    Ok(())
}

fn copy_dir(src: &PathBuf, dst: &PathBuf) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let from = e.path();
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
