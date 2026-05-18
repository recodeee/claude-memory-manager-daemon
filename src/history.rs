//! Append-only tick history (JSONL) + git-based undo for MEMORY_ROOT.
//!
//! Two halves:
//!   - `tick-report.jsonl` records one line per tick (skipped or run) so
//!     `mmctl history` can show what happened over time.
//!   - Before any mutating tick, we ensure MEMORY_ROOT is a git repo and
//!     commit its current state with a `[cmmd] pre-tick <ts>` message.
//!     That makes every tick reversible via `git -C MEMORY_ROOT reset --hard <sha>`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickRecord {
    pub tick_id: String,
    pub started_at_unix: u64,
    pub finished_at_unix: u64,
    pub dry_run: bool,
    pub memory_root: String,
    pub ran: bool,
    pub reason_skipped: Option<String>,
    pub exit_code: Option<i32>,
    pub audit_total_issues: usize,
    pub pre_tick_sha: Option<String>,
}

/// Append one line of JSON to `path`. Creates parents as needed.
pub fn append(path: &Path, rec: &TickRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut line = serde_json::to_string(rec).context("encode tick record")?;
    line.push('\n');
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("append {}", path.display()))?;
    Ok(())
}

/// Rotate `path` if it has more than `max_lines` lines: keeps the newest
/// `max_lines` in place and moves the previous file to `path.1` (overwriting
/// any older rotation). Returns true if rotation happened. 0 = disabled.
///
/// This is what keeps `history.jsonl` from growing unbounded: at one tick per
/// minute × N memory roots, the file otherwise accumulates forever.
pub fn rotate_if_oversize(path: &Path, max_lines: u64) -> Result<bool> {
    if max_lines == 0 {
        return Ok(false);
    }
    let Ok(body) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let lines: Vec<&str> = body.lines().collect();
    let n = lines.len() as u64;
    if n <= max_lines {
        return Ok(false);
    }
    // Keep newest `max_lines` in place.
    let keep_from = lines.len().saturating_sub(max_lines as usize);
    let mut kept = lines[keep_from..].join("\n");
    if !kept.is_empty() {
        kept.push('\n');
    }
    // Move pre-rotation copy to `.1` (a single rotation slot keeps disk
    // bounded; older history is intentionally discarded).
    let backup = path.with_extension(
        path.extension()
            .map(|e| format!("{}.1", e.to_string_lossy()))
            .unwrap_or_else(|| "1".to_string()),
    );
    let tmp = path.with_extension("rot.tmp");
    std::fs::write(&tmp, kept).with_context(|| format!("write {}", tmp.display()))?;
    let _ = std::fs::rename(path, &backup);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename to {}", path.display()))?;
    Ok(true)
}

/// Delete `/tmp/cmmd-tick-*.log` files older than `ttl_days`. Returns the
/// number of files removed. 0 = disabled. This is the *biggest* disk eater
/// of the three growth sources because per-tick agent transcripts are large.
pub fn sweep_tick_logs(ttl_days: u64) -> usize {
    if ttl_days == 0 {
        return 0;
    }
    let dir = std::path::Path::new("/tmp");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let cutoff = SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(ttl_days * 86_400))
        .unwrap_or(UNIX_EPOCH);
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        if !name_s.starts_with("cmmd-tick-") || !name_s.ends_with(".log") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        if mtime < cutoff && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Run `git gc --prune=now --aggressive` over `memory_root` if it's a git
/// repo and the marker file says we haven't gc'd in `interval_days`. Best
/// effort — failures are logged by the caller, never fatal. The marker lives
/// inside `.git/cmmd-last-gc` so it travels with the repo.
pub fn git_gc_if_due(memory_root: &Path, interval_days: u64) -> Result<bool> {
    if interval_days == 0 {
        return Ok(false);
    }
    let git_dir = memory_root.join(".git");
    if !git_dir.exists() {
        return Ok(false);
    }
    let marker = git_dir.join("cmmd-last-gc");
    let now = now_unix();
    let interval = interval_days.saturating_mul(86_400);
    if let Ok(prev) = std::fs::read_to_string(&marker) {
        if let Ok(prev_unix) = prev.trim().parse::<u64>() {
            if now.saturating_sub(prev_unix) < interval {
                return Ok(false);
            }
        }
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(memory_root)
        .arg("gc")
        .arg("--prune=now")
        .arg("--quiet")
        .status()
        .context("git gc")?;
    if !status.success() {
        return Err(anyhow::anyhow!("git gc failed"));
    }
    let _ = std::fs::write(&marker, now.to_string());
    Ok(true)
}

/// Count files in `memory_root`'s working tree that differ from HEAD. Used
/// to enforce MAX_FIXES_PER_TICK: if the agent edited more files than the
/// allowed cap, the caller can `restore(pre_sha)` to roll the lot back.
pub fn count_dirty_files(memory_root: &Path) -> Result<usize> {
    let out = Command::new("git")
        .arg("-C")
        .arg(memory_root)
        .arg("status")
        .arg("--porcelain")
        .output()
        .context("git status --porcelain")?;
    if !out.status.success() {
        return Err(anyhow::anyhow!("git status failed"));
    }
    let body = String::from_utf8_lossy(&out.stdout);
    Ok(body.lines().filter(|l| !l.trim().is_empty()).count())
}

/// Read the last `n` records (newest first). Cheap for typical n; we read the
/// whole file and slice — tick-report.jsonl is rarely huge.
pub fn tail(path: &Path, n: usize) -> Vec<TickRecord> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let mut recs: Vec<TickRecord> = body
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    recs.reverse();
    recs.truncate(n);
    recs
}

/// Make sure MEMORY_ROOT is a git repo. Initializes if missing. Best-effort:
/// errors are logged and ignored — undo is a nice-to-have, not load-bearing.
pub fn ensure_git_repo(memory_root: &Path) -> Result<()> {
    if memory_root.join(".git").exists() {
        return Ok(());
    }
    if !memory_root.is_dir() {
        return Ok(());
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(memory_root)
        .arg("init")
        .arg("--quiet")
        .status()
        .context("git init")?;
    if !status.success() {
        return Err(anyhow::anyhow!("git init failed"));
    }
    // Identity for the commits made inside MEMORY_ROOT. We use a synthetic one
    // so it doesn't show up under the user's regular author identity.
    for (k, v) in &[
        ("user.email", "cmmd@local"),
        ("user.name", "claude-memory-manager-daemon"),
    ] {
        let _ = Command::new("git")
            .arg("-C")
            .arg(memory_root)
            .arg("config")
            .arg(k)
            .arg(v)
            .status();
    }
    // First snapshot — captures the pre-existing state.
    commit_snapshot(memory_root, "initial cmmd snapshot").ok();
    Ok(())
}

/// Stage everything and commit. Returns the new commit SHA, or None if there
/// was nothing to commit (clean tree).
pub fn commit_snapshot(memory_root: &Path, message: &str) -> Result<Option<String>> {
    let add = Command::new("git")
        .arg("-C")
        .arg(memory_root)
        .arg("add")
        .arg("-A")
        .status()
        .context("git add")?;
    if !add.success() {
        return Err(anyhow::anyhow!("git add failed"));
    }
    let diff = Command::new("git")
        .arg("-C")
        .arg(memory_root)
        .arg("diff")
        .arg("--cached")
        .arg("--quiet")
        .status()
        .context("git diff --cached")?;
    if diff.success() {
        // No changes staged.
        return Ok(None);
    }
    let full_msg = format!("[cmmd] {}", message);
    let commit = Command::new("git")
        .arg("-C")
        .arg(memory_root)
        .arg("commit")
        .arg("--quiet")
        .arg("-m")
        .arg(&full_msg)
        .status()
        .context("git commit")?;
    if !commit.success() {
        return Err(anyhow::anyhow!("git commit failed"));
    }
    head_sha(memory_root)
}

pub fn head_sha(memory_root: &Path) -> Result<Option<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(memory_root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .context("git rev-parse HEAD")?;
    if !out.status.success() {
        return Ok(None);
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        Ok(None)
    } else {
        Ok(Some(sha))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub sha: String,
    pub subject: String,
    pub date_iso: String,
}

pub fn log_entries(memory_root: &Path, n: usize) -> Result<Vec<LogEntry>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(memory_root)
        .arg("log")
        .arg(format!("-n{n}"))
        .arg("--pretty=format:%H%x09%cI%x09%s")
        .output()
        .context("git log")?;
    if !out.status.success() {
        return Ok(vec![]);
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let mut entries = Vec::new();
    for line in body.lines() {
        let mut it = line.splitn(3, '\t');
        let sha = it.next().unwrap_or("").to_string();
        let date_iso = it.next().unwrap_or("").to_string();
        let subject = it.next().unwrap_or("").to_string();
        if !sha.is_empty() {
            entries.push(LogEntry {
                sha,
                subject,
                date_iso,
            });
        }
    }
    Ok(entries)
}

pub fn restore(memory_root: &Path, sha: &str) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(memory_root)
        .arg("reset")
        .arg("--hard")
        .arg(sha)
        .status()
        .context("git reset --hard")?;
    if !status.success() {
        return Err(anyhow::anyhow!("git reset --hard {sha} failed"));
    }
    Ok(())
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn new_tick_id() -> String {
    let unix = now_unix();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{unix:x}{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("cmmd-hist-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn sample_rec(id: &str) -> TickRecord {
        TickRecord {
            tick_id: id.to_string(),
            started_at_unix: 1700000000,
            finished_at_unix: 1700000060,
            dry_run: true,
            memory_root: "/tmp/x".to_string(),
            ran: false,
            reason_skipped: Some("test".to_string()),
            exit_code: None,
            audit_total_issues: 0,
            pre_tick_sha: None,
        }
    }

    #[test]
    fn append_then_tail_round_trips() {
        let dir = unique_tmp("append");
        let log = dir.join("ticks.jsonl");
        append(&log, &sample_rec("a")).unwrap();
        append(&log, &sample_rec("b")).unwrap();
        append(&log, &sample_rec("c")).unwrap();
        let recs = tail(&log, 2);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].tick_id, "c", "tail returns newest first");
        assert_eq!(recs[1].tick_id, "b");
    }

    #[test]
    fn tail_on_missing_file_is_empty() {
        let recs = tail(Path::new("/tmp/cmmd-nonexistent-history-xyz.jsonl"), 10);
        assert!(recs.is_empty());
    }

    #[test]
    fn new_tick_id_is_unique_per_call() {
        let a = new_tick_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_tick_id();
        assert_ne!(a, b);
    }

    #[test]
    fn rotate_keeps_newest_lines_and_writes_backup() {
        let dir = unique_tmp("rotate");
        let log = dir.join("ticks.jsonl");
        for i in 0..10 {
            append(&log, &sample_rec(&format!("t{i}"))).unwrap();
        }
        let rotated = rotate_if_oversize(&log, 4).unwrap();
        assert!(rotated, "should have rotated");
        // The newest 4 must remain.
        let after = std::fs::read_to_string(&log).unwrap();
        let kept_lines: Vec<&str> = after.lines().collect();
        assert_eq!(kept_lines.len(), 4);
        assert!(kept_lines[3].contains("\"t9\""), "newest line preserved");
        // Backup exists.
        let backup = dir.join("ticks.jsonl.1");
        assert!(backup.exists(), "backup file written");
    }

    #[test]
    fn rotate_below_threshold_is_a_noop() {
        let dir = unique_tmp("rotate-below");
        let log = dir.join("ticks.jsonl");
        append(&log, &sample_rec("only")).unwrap();
        let rotated = rotate_if_oversize(&log, 10).unwrap();
        assert!(!rotated);
    }

    #[test]
    fn rotate_disabled_with_zero_max() {
        let dir = unique_tmp("rotate-zero");
        let log = dir.join("ticks.jsonl");
        for i in 0..5 {
            append(&log, &sample_rec(&format!("z{i}"))).unwrap();
        }
        assert!(!rotate_if_oversize(&log, 0).unwrap());
    }

    #[test]
    fn sweep_tick_logs_ignores_recent_files() {
        // Create a fresh file in /tmp matching the pattern; sweeper must
        // not touch it because mtime is "now".
        let p = std::env::temp_dir()
            .join(format!("cmmd-tick-roundtrip-{}.log", std::process::id()));
        std::fs::write(&p, "x").unwrap();
        let removed = sweep_tick_logs(7);
        assert!(p.exists(), "fresh transcript must survive sweep");
        let _ = std::fs::remove_file(&p);
        let _ = removed;
    }

    #[test]
    fn sweep_tick_logs_disabled_with_zero_ttl() {
        assert_eq!(sweep_tick_logs(0), 0);
    }
}
