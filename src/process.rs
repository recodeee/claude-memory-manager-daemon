//! System process snapshot. Read-only.

use serde::Serialize;
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
    matches!(n.as_str(), "claude" | "claude-cli" | "kiro-cli" | "kiro-cli-chat")
}
