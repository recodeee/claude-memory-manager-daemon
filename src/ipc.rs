//! Lock / PID file + Unix socket status server.
//!
//! The status socket lets `mmctl` query a live snapshot (config, last tick,
//! authmux state, memory stat) without restarting the daemon.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
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

/// RAII guard for the daemon's exclusive process-singleton lock.
///
/// Two-line summary of how this defeats the old race:
///   1. The lock is acquired with `flock(LOCK_EX|LOCK_NB)` on an *fd we hold*.
///      Kernel guarantees only one fd at a time can hold the exclusive lock,
///      so two cmmd processes can't both pass startup, even if they call
///      `acquire_lock` at the same microsecond.
///   2. On crash, the kernel closes our fd and releases the lock for us. No
///      stale-lock-file false positives, no PID-recycle ambiguity.
///
/// On Drop the guard removes the lock_file and pid_file. The fd is dropped
/// last, which is when the kernel releases the actual lock.
#[derive(Debug)]
pub struct LockGuard {
    lock_file: PathBuf,
    pid_file: PathBuf,
    // Held for the lifetime of the daemon. The fd's close releases the flock.
    // Keep this *after* the path fields so Drop runs lock_file/pid_file
    // removal before the fd close — that ordering avoids a tiny window in
    // which another daemon could see the file gone and re-acquire while our
    // fd is still in close().
    _fd: std::fs::File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_file);
        let _ = std::fs::remove_file(&self.pid_file);
        // _fd dropped here -> kernel releases the flock.
    }
}

/// Acquire the daemon singleton lock.
///
/// Uses `flock(LOCK_EX|LOCK_NB)` on the lock file's fd. If the lock is held
/// by another live daemon, returns an Err that includes the holding PID
/// (read from the file body) for operator clarity. The flock is what makes
/// this race-free; the PID readback is purely cosmetic.
pub fn acquire_lock(lock_file: &Path, pid_file: &Path) -> Result<LockGuard> {
    if let Some(parent) = lock_file.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // Open-or-create with read-write so we can flock + truncate + write the
    // current pid. We do NOT use create_new(true) because that prevents
    // re-acquiring after a clean shutdown left the file behind (intentional —
    // the file is a marker, the flock is the actual lock).
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_file)
        .with_context(|| format!("open {}", lock_file.display()))?;

    let fd = file.as_raw_fd();
    // Non-blocking exclusive flock. If anyone else holds it, EWOULDBLOCK.
    let rc = unsafe { libc_flock(fd, LOCK_EX | LOCK_NB) };
    if rc != 0 {
        // Best-effort read of the previous holder's PID for the error message.
        // This is purely informational — flock already told us the truth.
        let pid_hint = std::fs::read_to_string(lock_file)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        return Err(match pid_hint {
            Some(p) => anyhow!(
                "another daemon holds {} (pid={p})",
                lock_file.display(),
            ),
            None => anyhow!(
                "another daemon holds {} (pid unknown)",
                lock_file.display(),
            ),
        });
    }

    // We hold the lock. Stamp our pid into both files.
    let me = std::process::id().to_string();
    use std::io::{Seek, SeekFrom, Write};
    let mut f = file;
    f.set_len(0)
        .with_context(|| format!("truncate {}", lock_file.display()))?;
    f.seek(SeekFrom::Start(0))
        .with_context(|| format!("seek {}", lock_file.display()))?;
    f.write_all(me.as_bytes())
        .with_context(|| format!("write {}", lock_file.display()))?;
    f.sync_all()
        .with_context(|| format!("sync {}", lock_file.display()))?;

    std::fs::write(pid_file, &me)
        .with_context(|| format!("write {}", pid_file.display()))?;

    Ok(LockGuard {
        lock_file: lock_file.to_path_buf(),
        pid_file: pid_file.to_path_buf(),
        _fd: f,
    })
}

// flock(2) is not in libc-side `nix` crate without the "fs" feature, so we
// declare the syscall directly. This is portable Linux/macOS — no surprises.
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}
unsafe fn libc_flock(fd: i32, op: i32) -> i32 {
    flock(fd, op)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("cmmd-ipc-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn second_acquire_fails_while_first_held() {
        let dir = unique_tmp("flock-double");
        let lock = dir.join("daemon.lock");
        let pid = dir.join("daemon.pid");
        let first = acquire_lock(&lock, &pid).expect("first acquire");
        let second = acquire_lock(&lock, &pid);
        assert!(second.is_err(), "second acquire must fail while first held");
        let msg = format!("{}", second.unwrap_err());
        assert!(
            msg.contains("another daemon holds"),
            "error mentions the lock collision: {msg}"
        );
        drop(first);
        // After drop the lock + pid files should be gone.
        assert!(!lock.exists(), "lock file removed on drop");
        assert!(!pid.exists(), "pid file removed on drop");
    }

    #[test]
    fn third_acquire_succeeds_after_first_dropped() {
        let dir = unique_tmp("flock-reacquire");
        let lock = dir.join("daemon.lock");
        let pid = dir.join("daemon.pid");
        {
            let _g = acquire_lock(&lock, &pid).expect("first");
        }
        // Once the first guard is dropped, a fresh acquire must work — this
        // is the "stale lock file" case that used to require a process probe.
        let _g2 = acquire_lock(&lock, &pid).expect("reacquire after drop");
    }

    #[test]
    fn pid_file_contains_current_process_id() {
        let dir = unique_tmp("flock-pid");
        let lock = dir.join("daemon.lock");
        let pid = dir.join("daemon.pid");
        let _g = acquire_lock(&lock, &pid).expect("acquire");
        let written = std::fs::read_to_string(&pid).unwrap();
        let want = std::process::id().to_string();
        assert_eq!(written.trim(), want);
    }
}
