//! Lock / PID file + Unix socket status server.
//!
//! The status socket lets `mmctl` query a live snapshot (config, last tick,
//! authmux state, memory stat) without restarting the daemon.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DaemonStatus {
    pub pid: u32,
    pub started_at_unix: u64,
    pub dry_run: bool,
    pub model: String,
    pub memory_root: String,
    pub last_tick: Option<TickRecord>,
    pub authmux: serde_json::Value,
    pub memory: serde_json::Value,
    pub claude_account_dirs: serde_json::Value,
    pub config: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TickRecord {
    pub started_at_unix: u64,
    pub finished_at_unix: u64,
    pub ran: bool,
    pub reason_skipped: Option<String>,
    pub exit_code: Option<i32>,
}

pub fn acquire_lock(lock_file: &Path, pid_file: &Path) -> Result<()> {
    if lock_file.exists() {
        let pid_str = std::fs::read_to_string(lock_file).unwrap_or_default();
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            // signal 0 = existence probe
            if unsafe { libc_kill(pid, 0) } == 0 {
                return Err(anyhow!("another daemon holds {} (pid={})", lock_file.display(), pid));
            }
        }
        let _ = std::fs::remove_file(lock_file);
    }
    let me = std::process::id();
    std::fs::write(lock_file, me.to_string()).with_context(|| format!("write {}", lock_file.display()))?;
    std::fs::write(pid_file, me.to_string()).with_context(|| format!("write {}", pid_file.display()))?;
    Ok(())
}

pub fn release_lock(lock_file: &Path, pid_file: &Path) {
    let _ = std::fs::remove_file(lock_file);
    let _ = std::fs::remove_file(pid_file);
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 { kill(pid, sig) }

pub async fn serve_status(
    sock_path: std::path::PathBuf,
    state: std::sync::Arc<tokio::sync::RwLock<DaemonStatus>>,
) -> Result<()> {
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("bind {}", sock_path.display()))?;
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => { tracing::warn!("accept err: {e}"); continue; }
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _ = handle_client(&mut stream, state).await;
        });
    }
}

async fn handle_client(
    stream: &mut UnixStream,
    state: std::sync::Arc<tokio::sync::RwLock<DaemonStatus>>,
) -> Result<()> {
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).await.unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    match req.as_str() {
        "" | "status" => {
            let s = state.read().await;
            let body = serde_json::to_string(&*s)?;
            stream.write_all(body.as_bytes()).await?;
        }
        "ping" => {
            stream.write_all(b"pong\n").await?;
        }
        other => {
            stream.write_all(format!("unknown:{other}\n").as_bytes()).await?;
        }
    }
    Ok(())
}

pub async fn query_status(sock_path: &Path) -> Result<DaemonStatus> {
    let mut stream = UnixStream::connect(sock_path).await
        .with_context(|| format!("connect {}", sock_path.display()))?;
    stream.write_all(b"status").await?;
    stream.shutdown().await.ok();
    let mut body = String::new();
    stream.read_to_string(&mut body).await?;
    let status: DaemonStatus = serde_json::from_str(&body).context("decode status response")?;
    Ok(status)
}
