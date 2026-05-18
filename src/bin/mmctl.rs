//! mmctl — companion CLI for the running `cmmd` daemon.
//!
//! Talks to the daemon over its Unix domain socket. The daemon must be up;
//! otherwise mmctl prints a clear error and exits non-zero.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mmctl", version, about = "Control + inspect the claude-memory-manager-daemon")]
struct Cli {
    /// Override the daemon's Unix socket path.
    #[arg(long, env = "STATUS_SOCK", default_value = "/tmp/claude-memory-manager.sock")]
    sock: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Full daemon status as pretty JSON.
    Status,
    /// Just the authmux block: who is logged in, who is active, usage % per row.
    Accounts,
    /// Memory-root stats only (file count, idle seconds, MEMORY.md line count).
    Memory,
    /// Last tick record only.
    LastTick,
    /// Liveness probe.
    Ping,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Mod path: this is a separate binary but lives in the same crate, so
    // we re-import the daemon's ipc module via the crate name.
    let status = claude_memory_manager_daemon::ipc::query_status(&cli.sock)
        .await
        .with_context(|| format!("daemon not reachable on {}", cli.sock.display()))?;

    match cli.cmd {
        Cmd::Status => println!("{}", serde_json::to_string_pretty(&status)?),
        Cmd::Accounts => println!("{}", serde_json::to_string_pretty(&status.authmux)?),
        Cmd::Memory => println!("{}", serde_json::to_string_pretty(&status.memory)?),
        Cmd::LastTick => println!("{}", serde_json::to_string_pretty(&status.last_tick)?),
        Cmd::Ping => println!("ok"),
    }
    Ok(())
}
