//! RAM pressure response: monitors memory usage and takes escalating actions
//! when thresholds are exceeded.

use serde::Serialize;
use std::fs;
use std::process::Command;
use tracing::{info, warn};

const PRESSURE_THRESHOLD_PCT: u64 = 80;
const CRITICAL_THRESHOLD_PCT: u64 = 90;

#[derive(Debug, Clone, Serialize)]
pub struct MemInfo {
    pub total_kb: u64,
    pub available_kb: u64,
    pub used_pct: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PressureResponse {
    pub mem: MemInfo,
    pub actions_taken: Vec<String>,
    pub threshold_exceeded: bool,
}

/// Read /proc/meminfo and compute usage percentage.
pub fn read_meminfo() -> MemInfo {
    let content = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut total: u64 = 0;
    let mut available: u64 = 0;

    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            total = parse_kb(line);
        } else if line.starts_with("MemAvailable:") {
            available = parse_kb(line);
        }
    }

    let used_pct = if total > 0 {
        ((total - available) * 100) / total
    } else {
        0
    };

    MemInfo {
        total_kb: total,
        available_kb: available,
        used_pct,
    }
}

fn parse_kb(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Check RAM pressure and take actions if thresholds exceeded.
pub fn check_and_respond() -> PressureResponse {
    let mem = read_meminfo();
    let mut actions = Vec::new();

    if mem.used_pct < PRESSURE_THRESHOLD_PCT {
        return PressureResponse {
            mem,
            actions_taken: actions,
            threshold_exceeded: false,
        };
    }

    info!(used_pct = mem.used_pct, "RAM pressure detected");

    // Level 1: Prune stopped docker containers
    if run_cmd("docker", &["container", "prune", "-f"]) {
        actions.push("docker container prune".into());
    }

    // Level 2: Drop page cache (safe — kernel rebuilds as needed)
    if mem.used_pct >= CRITICAL_THRESHOLD_PCT {
        // sync first to flush dirty pages
        let _ = Command::new("sync").status();
        if fs::write("/proc/sys/vm/drop_caches", "1").is_ok() {
            actions.push("drop_caches=1".into());
        } else {
            // Try via sudo
            if run_cmd("sudo", &["sh", "-c", "echo 1 > /proc/sys/vm/drop_caches"]) {
                actions.push("drop_caches=1 (sudo)".into());
            }
        }
    }

    // Level 3: Enforce swap tuning (in case it was reset)
    enforce_swap_tuning(&mut actions);

    if !actions.is_empty() {
        info!(actions = ?actions, "pressure response complete");
    }

    PressureResponse {
        mem,
        actions_taken: actions,
        threshold_exceeded: true,
    }
}

/// Ensure optimal swap settings are applied.
fn enforce_swap_tuning(actions: &mut Vec<String>) {
    let tunings: &[(&str, &str)] = &[
        ("/proc/sys/vm/swappiness", "60"),
        ("/proc/sys/vm/page-cluster", "0"),
    ];

    for (path, desired) in tunings {
        let current = fs::read_to_string(path)
            .unwrap_or_default()
            .trim()
            .to_string();
        if current != *desired {
            if fs::write(path, desired).is_ok() {
                actions.push(format!("{path}={desired} (was {current})"));
            } else {
                // Non-root — try sudo
                let val = format!("echo {desired} > {path}");
                if run_cmd("sudo", &["sh", "-c", &val]) {
                    actions.push(format!("{path}={desired} (sudo, was {current})"));
                } else {
                    warn!(path, desired, current, "failed to enforce swap tuning");
                }
            }
        }
    }
}

fn run_cmd(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_meminfo_returns_nonzero() {
        let mem = read_meminfo();
        assert!(mem.total_kb > 0);
        assert!(mem.used_pct <= 100);
    }

    #[test]
    fn parse_kb_works() {
        assert_eq!(parse_kb("MemTotal:       32456789 kB"), 32456789);
        assert_eq!(parse_kb("garbage"), 0);
    }
}
