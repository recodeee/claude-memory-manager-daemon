//! claude-memory-manager-daemon — `cmmd`
//!
//! Long-running daemon. Each tick:
//!   1. refresh authmux snapshot (always shows who's logged in)
//!   2. stat MEMORY_ROOT
//!   3. abort if another live claude session exists OR memory was touched recently
//!   4. otherwise spawn `claude -p ...` against the memory-manager subagent
//!
//! A Unix socket at $STATUS_SOCK lets `mmctl status` query the running daemon.

use claude_memory_manager_daemon::{authmux, config, ipc, memory, process, tick};

use anyhow::Result;
use clap::{Parser, Subcommand};
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
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    let cfg = config::Config::load()?;

    match cli.cmd.unwrap_or(Cmd::Run { once: false }) {
        Cmd::Doctor => run_doctor(cfg).await,
        Cmd::Run { once } => run_daemon(cfg, once).await,
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

    let started_at = now_unix();
    let status = ipc::DaemonStatus {
        pid: std::process::id(),
        started_at_unix: started_at,
        dry_run: cfg.dry_run,
        model: cfg.model.clone(),
        memory_root: cfg.memory_root.display().to_string(),
        last_tick: None,
        authmux: serde_json::Value::Null,
        memory: serde_json::Value::Null,
        claude_account_dirs: serde_json::Value::Null,
        config: serde_json::to_value(&cfg)?,
    };
    let state = Arc::new(RwLock::new(status));

    // Status socket.
    {
        let sock = cfg.status_sock.clone();
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = ipc::serve_status(sock, st).await {
                error!("status server died: {e}");
            }
        });
    }

    // Signal handling — clean shutdown on SIGTERM / SIGINT.
    let shutdown = tokio::spawn(install_signal_handler());

    let cfg_arc = Arc::new(cfg);
    let result = main_loop(cfg_arc.clone(), state.clone(), once, shutdown).await;

    ipc::release_lock(&cfg_arc.lock_file, &cfg_arc.pid_file);
    info!("daemon down");
    result
}

async fn main_loop(
    cfg: Arc<config::Config>,
    state: Arc<RwLock<ipc::DaemonStatus>>,
    once: bool,
    mut shutdown: tokio::task::JoinHandle<()>,
) -> Result<()> {
    loop {
        let tick_started = now_unix();

        // Always refresh authmux + memory + account dirs first.
        // This is what makes "who is logged in" visible at any time via `mmctl status`.
        let am = authmux::snapshot(&cfg.authmux_bin).await;
        let mem = memory::stat(&cfg.memory_root);
        let accts = authmux::claude_account_dirs(&cfg.claude_accounts_dir);
        {
            let mut s = state.write().await;
            s.authmux = serde_json::to_value(&am)?;
            s.memory = serde_json::to_value(&mem)?;
            s.claude_account_dirs = serde_json::to_value(&accts)?;
        }
        log_login_summary(&am, &accts);

        let outcome = match tick::run(&cfg, &mem).await {
            Ok(o) => o,
            Err(e) => {
                error!("tick error: {e:#}");
                tick::TickOutcome { ran: false, reason_skipped: Some(format!("{e}")), exit_code: None }
            }
        };

        let rec = ipc::TickRecord {
            started_at_unix: tick_started,
            finished_at_unix: now_unix(),
            ran: outcome.ran,
            reason_skipped: outcome.reason_skipped.clone(),
            exit_code: outcome.exit_code,
        };
        if let Some(r) = &outcome.reason_skipped {
            info!(reason = %r, "tick skipped");
        } else {
            info!(exit = ?outcome.exit_code, "tick complete");
        }
        state.write().await.last_tick = Some(rec);

        if once {
            return Ok(());
        }

        tokio::select! {
            _ = tokio::time::sleep(cfg.tick_interval) => {}
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
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
