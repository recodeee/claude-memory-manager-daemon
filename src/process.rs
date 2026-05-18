//! System process snapshot. Read-only.

use serde::Serialize;
use std::path::Path;
use std::process::Command;
use sysinfo::{ProcessRefreshKind, RefreshKind, System};

#[derive(Debug, Clone, Serialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub cmd: String,
    pub rss_kb: u64,
    pub cpu_pct: f32,
}

pub fn snapshot_top(limit: usize) -> Vec<ProcInfo> {
    let sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::everything()),
    );
    let mut rows: Vec<ProcInfo> = sys
        .processes()
        .iter()
        .map(|(pid, p)| ProcInfo {
            pid: pid.as_u32(),
            name: p.name().to_string(),
            cmd: p.cmd().join(" "),
            rss_kb: p.memory() / 1024,
            cpu_pct: p.cpu_usage(),
        })
        .collect();
    rows.sort_by(|a, b| b.rss_kb.cmp(&a.rss_kb));
    rows.truncate(limit);
    rows
}

/// Returns every claude / claude-cli / kiro-cli process — used to detect when
/// another live session is editing memory so the daemon can skip a tick.
pub fn find_claude_sessions() -> Vec<ProcInfo> {
    let sys = System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::everything()),
    );
    sys.processes()
        .iter()
        .filter(|(_, p)| is_claude_proc(p.name(), &p.cmd().join(" ")))
        .map(|(pid, p)| ProcInfo {
            pid: pid.as_u32(),
            name: p.name().to_string(),
            cmd: p.cmd().join(" "),
            rss_kb: p.memory() / 1024,
            cpu_pct: p.cpu_usage(),
        })
        .collect()
}

fn is_claude_proc(name: &str, _cmd: &str) -> bool {
    // Name-only on purpose. Bun/Node worker threads (HeapHelper, "Bun Pool N",
    // "HTTP Client", fs.watch) inherit the parent claude command line, so a
    // cmd-substring match catches dozens of false positives per session.
    let n = name.to_lowercase();
    matches!(
        n.as_str(),
        "claude" | "claude-cli" | "kiro-cli" | "kiro-cli-chat"
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct MemoryHolder {
    pub pid: u32,
    pub name: String,
}

/// Detect processes (other than the daemon) that currently hold a file under
/// `memory_root` open. This is a much sharper "is somebody touching memory
/// right now" signal than a blanket `find_claude_sessions()` — it only fires
/// when a real fd points at a real file in the lane.
///
/// Implementation: shells out to `lsof +D <memory_root>`. If lsof is missing
/// or fails, returns `Err` so the caller can decide whether to fall back to
/// the conservative process-name guard.
pub fn memory_holders(memory_root: &Path) -> Result<Vec<MemoryHolder>, String> {
    let me = std::process::id();
    let out = Command::new("lsof")
        .arg("-F")
        .arg("pcn") // machine-readable: p=pid c=cmd n=name
        .arg("+D")
        .arg(memory_root) // recursive enumerate
        .output()
        .map_err(|e| format!("lsof spawn failed: {e}"))?;
    // lsof returns 1 when there are no matching files — that's success-with-zero-results.
    let stdout = String::from_utf8_lossy(&out.stdout);

    let mut holders: Vec<MemoryHolder> = Vec::new();
    let mut cur_pid: Option<u32> = None;
    let mut cur_name: Option<String> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            // Flush previous record if it pointed at a real file.
            cur_pid = rest.parse().ok();
            cur_name = None;
        } else if let Some(rest) = line.strip_prefix('c') {
            cur_name = Some(rest.to_string());
        } else if line.starts_with('n') {
            // A file record — emit the (pid, name) pair if it's not us.
            if let (Some(pid), Some(name)) = (cur_pid, cur_name.clone()) {
                if pid != me && !holders.iter().any(|h| h.pid == pid) {
                    holders.push(MemoryHolder { pid, name });
                }
            }
        }
    }
    Ok(holders)
}
