//! Stat the file-based memory lane at $MEMORY_ROOT. Read-only.

use serde::Serialize;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Default)]
pub struct MemoryStat {
    pub root: String,
    pub exists: bool,
    pub file_count: u64,
    pub total_bytes: u64,
    pub newest_mtime_unix: u64,
    pub idle_sec: u64,
    pub memory_md_lines: Option<u64>,
}

pub fn stat(root: &Path) -> MemoryStat {
    let mut s = MemoryStat {
        root: root.display().to_string(),
        ..Default::default()
    };
    if !root.exists() {
        return s;
    }
    s.exists = true;

    let mut newest: u64 = 0;
    for entry in WalkDir::new(root).into_iter().flatten() {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        s.file_count += 1;
        s.total_bytes += meta.len();
        if let Ok(modified) = meta.modified() {
            if let Ok(d) = modified.duration_since(UNIX_EPOCH) {
                let secs = d.as_secs();
                if secs > newest {
                    newest = secs;
                }
            }
        }
    }
    s.newest_mtime_unix = newest;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    s.idle_sec = now.saturating_sub(newest);

    // Line count of MEMORY.md, if present.
    let mem_md = root.join("MEMORY.md");
    if let Ok(contents) = std::fs::read_to_string(&mem_md) {
        s.memory_md_lines = Some(contents.lines().count() as u64);
    }

    s
}
