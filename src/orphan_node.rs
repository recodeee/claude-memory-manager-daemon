//! Orphan node process reaper: kills mcpvault-stdio-keepalive, mcp-server.cjs,
//! and worker-service.cjs processes whose parent process is dead (ppid=1 or
//! parent no longer exists).

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde::Serialize;
use std::fs;
use tracing::info;

/// Patterns in the cmdline that identify reapable orphan node processes.
const ORPHAN_PATTERNS: &[&str] = &[
    "mcpvault-stdio-keepalive",
    "mcp-server.cjs",
    "worker-service.cjs",
];

#[derive(Debug, Clone, Serialize)]
pub struct OrphanProc {
    pub pid: u32,
    pub ppid: u32,
    pub cmdline: String,
    pub rss_kb: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReapResult {
    pub scanned: usize,
    pub reaped: Vec<OrphanProc>,
}

/// Scan /proc for node processes matching ORPHAN_PATTERNS whose parent is
/// dead (ppid == 1, meaning they were reparented to init).
pub fn find_orphans() -> Vec<OrphanProc> {
    let my_uid = unsafe { nix::libc::getuid() };
    let mut orphans = Vec::new();

    let entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return orphans,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();
        let pid: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Check ownership
        let status_path = format!("/proc/{pid}/status");
        let status = match fs::read_to_string(&status_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let uid = status
            .lines()
            .find(|l| l.starts_with("Uid:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(u32::MAX);
        if uid != my_uid {
            continue;
        }

        let ppid = status
            .lines()
            .find(|l| l.starts_with("PPid:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);

        let rss_kb = status
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        // Only consider processes reparented to init (ppid=1)
        if ppid != 1 {
            continue;
        }

        // Check cmdline matches our patterns
        let cmdline_path = format!("/proc/{pid}/cmdline");
        let cmdline = match fs::read_to_string(&cmdline_path) {
            Ok(c) => c.replace('\0', " "),
            Err(_) => continue,
        };

        if ORPHAN_PATTERNS.iter().any(|pat| cmdline.contains(pat)) {
            orphans.push(OrphanProc {
                pid,
                ppid,
                cmdline: cmdline.trim().to_string(),
                rss_kb,
            });
        }
    }

    orphans
}

/// Kill orphaned node processes. Returns what was reaped.
pub fn reap_orphans() -> ReapResult {
    let orphans = find_orphans();
    let scanned = orphans.len();
    let mut reaped = Vec::new();

    for proc in orphans {
        let pid = Pid::from_raw(proc.pid as i32);
        if kill(pid, Signal::SIGTERM).is_ok() {
            info!(
                pid = proc.pid,
                rss_kb = proc.rss_kb,
                cmd = %proc.cmdline.chars().take(80).collect::<String>(),
                "reaped orphan node process"
            );
            reaped.push(proc);
        }
    }

    ReapResult { scanned, reaped }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_orphans_does_not_panic() {
        let _ = find_orphans();
    }

    #[test]
    fn patterns_cover_known_orphans() {
        assert!(ORPHAN_PATTERNS.contains(&"mcpvault-stdio-keepalive"));
        assert!(ORPHAN_PATTERNS.contains(&"mcp-server.cjs"));
        assert!(ORPHAN_PATTERNS.contains(&"worker-service.cjs"));
    }
}
