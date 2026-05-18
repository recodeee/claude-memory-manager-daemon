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
}
