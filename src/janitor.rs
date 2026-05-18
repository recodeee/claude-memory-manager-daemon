//! Process janitor: identifies and (optionally) terminates stale Claude / Codex
//! / Kiro CLI sessions. All safety invariants are enforced HERE, in compiled
//! Rust, not in the skill prompt the agent reads.
//!
//! Invariants (none of these can be relaxed via env vars):
//!   - process name must be in the allowlist
//!   - process owner uid must equal the daemon's own uid
//!   - never the daemon's own pid, never a child of the daemon, never pid ≤ 1
//!   - hard ceiling: never kill more than [`MAX_KILLS_HARD`] in one invocation
//!
//! Tunable knobs (per-invocation flags):
//!   - --min-age-hours  (default 24)
//!   - --max-cpu-pct    (default 0.5)
//!   - --max            (per-invocation soft cap, default 5)
//!   - --no-dry-run     (without this, nothing is killed)

use anyhow::Result;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde::Serialize;
use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{Pid as SysPid, ProcessRefreshKind, RefreshKind, System};

pub const ALLOWLIST: &[&str] = &[
    "claude", "claude-cli", "kiro-cli", "kiro-cli-chat", "codex", "codex-cli",
];
pub const MAX_KILLS_HARD: usize = 20;

#[derive(Debug, Clone, Serialize)]
pub struct StaleProc {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    pub cmd: String,
    pub age_sec: u64,
    pub cpu_pct: f32,
    pub rss_kb: u64,
}

#[derive(Debug, Clone)]
pub struct Opts {
    pub min_age_sec: u64,
    pub max_cpu_pct: f32,
    pub max_kills: usize,
    pub dry_run: bool,
}

impl Opts {
    pub fn new(min_age_hours: f64, max_cpu_pct: f32, max_kills: usize, dry_run: bool) -> Self {
        Self {
            min_age_sec: (min_age_hours * 3600.0) as u64,
            max_cpu_pct,
            max_kills: max_kills.min(MAX_KILLS_HARD),
            dry_run,
        }
    }
}

pub fn list_stale(opts: &Opts) -> Vec<StaleProc> {
    let sys = fresh_system();
    let now = now_unix();
    let my_uid = current_uid();
    let my_pid = std::process::id();

    let allow: HashSet<&str> = ALLOWLIST.iter().copied().collect();

    let mut out = Vec::new();
    for (pid, p) in sys.processes() {
        let raw_pid = pid.as_u32();
        if raw_pid <= 1 || raw_pid == my_pid {
            continue;
        }
        let name = p.name().to_lowercase();
        if !allow.contains(name.as_str()) {
            continue;
        }
        // Owner check.
        if let Some(uid) = p.user_id() {
            if **uid != my_uid {
                continue;
            }
        } else {
            continue; // unknown owner — be conservative
        }
        // Don't kill our own children (e.g. the `claude -p` we spawned for the memory tick).
        let ppid = p.parent().map(|pp| pp.as_u32());
        if ppid == Some(my_pid) {
            continue;
        }

        let started = p.start_time();
        let age = now.saturating_sub(started);
        if age < opts.min_age_sec {
            continue;
        }
        let cpu = p.cpu_usage();
        if cpu > opts.max_cpu_pct {
            continue;
        }

        out.push(StaleProc {
            pid: raw_pid,
            ppid,
            name: name.clone(),
            cmd: p.cmd().join(" "),
            age_sec: age,
            cpu_pct: cpu,
            rss_kb: p.memory() / 1024,
        });
    }
    out.sort_by(|a, b| b.age_sec.cmp(&a.age_sec));
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyOutcome {
    pub considered: usize,
    pub attempted: Vec<KillRecord>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct KillRecord {
    pub pid: u32,
    pub name: String,
    pub age_sec: u64,
    pub sigterm_ok: bool,
    pub still_alive_after_grace: bool,
    pub sigkill_ok: Option<bool>,
}

pub fn apply(opts: &Opts) -> Result<ApplyOutcome> {
    let mut candidates = list_stale(opts);
    let considered = candidates.len();
    candidates.truncate(opts.max_kills);

    let mut records: Vec<KillRecord> = Vec::with_capacity(candidates.len());

    for c in &candidates {
        if opts.dry_run {
            records.push(KillRecord {
                pid: c.pid,
                name: c.name.clone(),
                age_sec: c.age_sec,
                sigterm_ok: false,
                still_alive_after_grace: false,
                sigkill_ok: None,
            });
            continue;
        }

        let pid = Pid::from_raw(c.pid as i32);
        let sigterm_ok = kill(pid, Signal::SIGTERM).is_ok();

        // Grace period: up to 10s waiting for the process to exit on its own.
        let mut alive = true;
        for _ in 0..10 {
            std::thread::sleep(Duration::from_secs(1));
            if !pid_alive(c.pid) {
                alive = false;
                break;
            }
        }

        let sigkill_ok = if alive {
            Some(kill(pid, Signal::SIGKILL).is_ok())
        } else {
            None
        };

        records.push(KillRecord {
            pid: c.pid,
            name: c.name.clone(),
            age_sec: c.age_sec,
            sigterm_ok,
            still_alive_after_grace: alive,
            sigkill_ok,
        });
    }

    Ok(ApplyOutcome { considered, attempted: records, dry_run: opts.dry_run })
}

fn pid_alive(pid: u32) -> bool {
    // signal 0 = existence probe
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

fn fresh_system() -> System {
    System::new_with_specifics(
        RefreshKind::new().with_processes(ProcessRefreshKind::everything()),
    )
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn current_uid() -> u32 {
    // Safe: getuid never fails on Linux.
    unsafe { libc_getuid() }
}

extern "C" { fn getuid() -> u32; }
unsafe fn libc_getuid() -> u32 { getuid() }

// keep linter happy when SysPid is otherwise unused in this file
#[allow(dead_code)]
fn _suppress_unused(_: SysPid) {}
