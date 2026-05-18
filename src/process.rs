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
///
/// Sync variant — kept for non-async callers (`cmmd doctor`, tests). The
/// hot path (`tick::run`) uses the async timeout-wrapped version below.
pub fn memory_holders(memory_root: &Path) -> Result<Vec<MemoryHolder>, String> {
    let out = Command::new("lsof")
        .arg("-F")
        .arg("pcn") // machine-readable: p=pid c=cmd n=name
        .arg("+D")
        .arg(memory_root) // recursive enumerate
        .output()
        .map_err(|e| format!("lsof spawn failed: {e}"))?;
    // lsof returns 1 when there are no matching files — that's success-with-zero-results.
    Ok(parse_lsof_pcn(&String::from_utf8_lossy(&out.stdout)))
}

/// Same as [`memory_holders`] but spawns lsof under a wall-clock timeout. If
/// lsof doesn't return within `timeout_secs`, the spawned process is killed
/// and `Err("lsof timeout")` is returned so the caller can fall back to the
/// conservative process-name guard.
///
/// This exists because the blocking lsof recursively walks `memory_root`;
/// on a slow filesystem (sshfs, network mount) it can hang for the lifetime
/// of the tick and wedge every subsequent iteration. The 0-timeout variant
/// is preserved for parity with the sync function.
pub async fn memory_holders_with_timeout(
    memory_root: &Path,
    timeout_secs: u64,
) -> Result<Vec<MemoryHolder>, String> {
    if timeout_secs == 0 {
        // Opt-out path: behave exactly like the sync version. Useful for
        // operators who can't tolerate the kill-on-deadline behavior.
        return memory_holders(memory_root);
    }
    let dur = std::time::Duration::from_secs(timeout_secs);
    let mut cmd = tokio::process::Command::new("lsof");
    cmd.arg("-F")
        .arg("pcn")
        .arg("+D")
        .arg(memory_root)
        .kill_on_drop(true);
    let fut = cmd.output();
    let out = match tokio::time::timeout(dur, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("lsof spawn failed: {e}")),
        Err(_) => return Err(format!("lsof timeout after {timeout_secs}s")),
    };
    Ok(parse_lsof_pcn(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse the `lsof -F pcn` machine-readable format. Lifted out so both the
/// sync and async variants share the same parser — easier to test, easier to
/// keep in sync if the lsof flags change.
fn parse_lsof_pcn(stdout: &str) -> Vec<MemoryHolder> {
    let me = std::process::id();
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
    holders
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lsof_skips_self_pid() {
        let me = std::process::id();
        let other = me + 1;
        let body = format!("p{me}\nccmd-self\nn/x\np{other}\nccmd-other\nn/y\n");
        let h = parse_lsof_pcn(&body);
        assert_eq!(h.len(), 1, "self pid filtered: got {:?}", h);
        assert_eq!(h[0].pid, other);
        assert_eq!(h[0].name, "cmd-other");
    }

    #[test]
    fn parse_lsof_dedupes_repeated_pid() {
        let other = std::process::id() + 99;
        let body = format!(
            "p{other}\ncfoo\nn/a\np{other}\ncfoo\nn/b\np{other}\ncfoo\nn/c\n"
        );
        let h = parse_lsof_pcn(&body);
        assert_eq!(h.len(), 1, "duplicate pid coalesced");
    }

    #[tokio::test]
    async fn memory_holders_zero_timeout_falls_back_to_sync() {
        // Smoke test: zero timeout means "behave like the sync call".
        // We pass /tmp which always exists; we don't care about the result,
        // only that the function returns without hanging.
        let _ = memory_holders_with_timeout(std::path::Path::new("/tmp"), 0).await;
    }
}
