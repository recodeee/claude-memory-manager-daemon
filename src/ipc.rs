//! Lock / PID file + Unix socket status server.
//!
//! The status socket lets `mmctl` query a live snapshot (config, last tick,
//! authmux state, memory stat) without restarting the daemon.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, RwLock};

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
                return Err(anyhow!(
                    "another daemon holds {} (pid={})",
                    lock_file.display(),
                    pid
                ));
            }
        }
        let _ = std::fs::remove_file(lock_file);
    }
    let me = std::process::id();
    std::fs::write(lock_file, me.to_string())
        .with_context(|| format!("write {}", lock_file.display()))?;
    std::fs::write(pid_file, me.to_string())
        .with_context(|| format!("write {}", pid_file.display()))?;
    Ok(())
}

pub fn release_lock(lock_file: &Path, pid_file: &Path) {
    let _ = std::fs::remove_file(lock_file);
    let _ = std::fs::remove_file(pid_file);
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    kill(pid, sig)
}

/// Daemon-side handles exposed to clients via the Unix socket.
#[derive(Clone)]
pub struct DaemonHandles {
    pub state: Arc<RwLock<DaemonStatus>>,
    /// Wake the main loop now (bypasses the inter-tick sleep).
    pub tick_now: Arc<Notify>,
    /// Toggle DRY_RUN at runtime. Returns the new value.
    pub dry_run: Arc<tokio::sync::Mutex<bool>>,
    /// Where to persist runtime overrides on toggle.
    pub state_file: std::path::PathBuf,
}

pub async fn serve_status(sock_path: std::path::PathBuf, handles: DaemonHandles) -> Result<()> {
    let _ = std::fs::remove_file(&sock_path);
    let listener =
        UnixListener::bind(&sock_path).with_context(|| format!("bind {}", sock_path.display()))?;
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("accept err: {e}");
                continue;
            }
        };
        let h = handles.clone();
        tokio::spawn(async move {
            let _ = handle_client(&mut stream, h).await;
        });
    }
}

async fn handle_client(stream: &mut UnixStream, handles: DaemonHandles) -> Result<()> {
    let mut buf = [0u8; 256];
    let n = stream.read(&mut buf).await.unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]).trim().to_string();
    match req.as_str() {
        "" | "status" => {
            let s = handles.state.read().await;
            let body = serde_json::to_string(&*s)?;
            stream.write_all(body.as_bytes()).await?;
        }
        "ping" => {
            stream.write_all(b"pong\n").await?;
        }
        "tick" => {
            handles.tick_now.notify_one();
            stream
                .write_all(b"{\"ok\":true,\"action\":\"tick\"}\n")
                .await?;
        }
        "dry-run-on" => {
            *handles.dry_run.lock().await = true;
            persist_dry_run(&handles.state_file, Some(true));
            stream
                .write_all(b"{\"ok\":true,\"dry_run\":true,\"persisted\":true}\n")
                .await?;
        }
        "dry-run-off" => {
            *handles.dry_run.lock().await = false;
            persist_dry_run(&handles.state_file, Some(false));
            stream
                .write_all(b"{\"ok\":true,\"dry_run\":false,\"persisted\":true}\n")
                .await?;
        }
        other => {
            stream
                .write_all(format!("{{\"ok\":false,\"error\":\"unknown:{other}\"}}\n").as_bytes())
                .await?;
        }
    }
    Ok(())
}

fn persist_dry_run(state_file: &Path, value: Option<bool>) {
    let mut s = crate::state::load(state_file);
    s.dry_run_override = value;
    if let Err(e) = crate::state::save(state_file, &s) {
        tracing::warn!("persist dry_run failed: {e}");
    }
}

/// Send a command and read the response (raw text). Used by mmctl.
pub async fn send_command(sock_path: &Path, cmd: &str) -> Result<String> {
    let mut stream = UnixStream::connect(sock_path)
        .await
        .with_context(|| format!("connect {}", sock_path.display()))?;
    stream.write_all(cmd.as_bytes()).await?;
    stream.shutdown().await.ok();
    let mut body = String::new();
    stream.read_to_string(&mut body).await?;
    Ok(body)
}

pub async fn query_status(sock_path: &Path) -> Result<DaemonStatus> {
    let body = send_command(sock_path, "status").await?;
    let status: DaemonStatus = serde_json::from_str(&body).context("decode status response")?;
    Ok(status)
}

// Silence "unused import" if anyhow!() is not used elsewhere in this file.
#[allow(dead_code)]
fn _suppress() -> anyhow::Error {
    anyhow!("unused")
}
