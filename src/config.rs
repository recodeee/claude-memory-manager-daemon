use anyhow::{Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub memory_root: PathBuf,
    /// Extra MEMORY_ROOT dirs to rotate over per tick. Parsed from
    /// MEMORY_ROOTS (colon-separated). The primary `memory_root` is
    /// always tended; these are additional dirs.
    pub additional_memory_roots: Vec<PathBuf>,
    pub tick_interval: Duration,
    pub min_idle: Duration,
    pub max_tick: Duration,
    pub dry_run: bool,
    pub state_file: PathBuf,
    pub history_file: PathBuf,
    /// Whether to keep MEMORY_ROOT under git for tick-undo.
    pub git_track: bool,
    /// Where the Prometheus exporter binds. Empty string = disabled.
    pub metrics_bind: String,
    /// Optional WEBHOOK_URL (http://host[:port][/path]). Empty = disabled.
    pub webhook_url: String,
    pub model: String,
    pub max_turns: u32,
    /// Hard cap on number of file modifications the agent is allowed to apply
    /// per tick. Enforced post-spawn by counting `git status --porcelain`
    /// entries against the pre-tick snapshot. 0 = unlimited.
    pub max_fixes_per_tick: u64,
    /// Daily ceiling on the number of *ran* ticks (skipped ticks don't count).
    /// The expensive path won't spawn `claude -p` once this is hit. Resets
    /// at UTC midnight. 0 = unlimited.
    pub max_ticks_per_day: u64,
    /// Rotate $HISTORY_FILE when it exceeds this many lines. 0 = disabled.
    pub history_max_lines: u64,
    /// TTL in days for `/tmp/cmmd-tick-<id>.log` files. 0 = disabled.
    pub tick_log_ttl_days: u64,
    /// Cadence for `git gc --prune=now --aggressive` over each MEMORY_ROOT.
    /// 0 = disabled.
    pub git_gc_interval_days: u64,
    /// Wall-clock cap for the `lsof +D <memory_root>` probe used to detect
    /// live editors. A slow filesystem (sshfs, network mount) could hang the
    /// blocking lsof and wedge the whole tick; this kills it instead.
    /// 0 = disabled (use the old unbounded blocking call).
    pub lsof_timeout_sec: u64,
    /// Run housekeepers (tmux_janitor, orphan_node, pressure) in
    /// report-only mode — log what *would* be killed without actually
    /// sending signals. Same intent as DRY_RUN but scoped to housekeepers
    /// so the memory-manager loop can still mutate while housekeepers stay
    /// silent.
    pub housekeeper_dry_run: bool,
    pub claude_bin: String,
    pub authmux_bin: String,
    pub claude_accounts_dir: PathBuf,
    pub log_file: PathBuf,
    pub pid_file: PathBuf,
    pub lock_file: PathBuf,
    pub status_sock: PathBuf,
    pub claude_config_dir: Option<PathBuf>,
}

impl Config {
    /// Every memory root the daemon should tend, primary first.
    pub fn all_memory_roots(&self) -> Vec<PathBuf> {
        let mut v = vec![self.memory_root.clone()];
        v.extend(self.additional_memory_roots.iter().cloned());
        v
    }

    pub fn load() -> Result<Self> {
        // Best-effort .env load; ignored if missing.
        let _ = dotenvy::dotenv();

        let home = std::env::var("HOME").context("$HOME not set")?;

        let memory_root = env_path(
            "MEMORY_ROOT",
            format!("{home}/.claude/projects/-home-deadpool/memory"),
        );

        // MEMORY_ROOTS: colon-separated extras. Skipped if empty / unset.
        let extras: Vec<PathBuf> = std::env::var("MEMORY_ROOTS")
            .unwrap_or_default()
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .filter(|p| *p != memory_root)
            .collect();

        Ok(Self {
            memory_root,
            additional_memory_roots: extras,
            tick_interval: Duration::from_secs(env_u64("TICK_INTERVAL_SEC", 900)),
            min_idle: Duration::from_secs(env_u64("MIN_IDLE_SEC", 300)),
            max_tick: Duration::from_secs(env_u64("MAX_TICK_SECONDS", 600)),
            dry_run: env_bool("DRY_RUN", true),
            state_file: env_path("STATE_FILE", "/tmp/cmmd-state.json".to_string()),
            history_file: env_path("HISTORY_FILE", "/tmp/cmmd-history.jsonl".to_string()),
            git_track: env_bool("GIT_TRACK_MEMORY", true),
            metrics_bind: env_str("METRICS_BIND", "127.0.0.1:9601"),
            webhook_url: env_str("WEBHOOK_URL", ""),
            model: env_str("MODEL", "claude-haiku-4-5-20251001"),
            max_turns: env_u64("MAX_TURNS", 12) as u32,
            max_fixes_per_tick: env_u64("MAX_FIXES_PER_TICK", 3),
            max_ticks_per_day: env_u64("MAX_TICKS_PER_DAY", 100),
            history_max_lines: env_u64("HISTORY_MAX_LINES", 10_000),
            tick_log_ttl_days: env_u64("TICK_LOG_TTL_DAYS", 7),
            git_gc_interval_days: env_u64("GIT_GC_INTERVAL_DAYS", 7),
            lsof_timeout_sec: env_u64("LSOF_TIMEOUT_SEC", 5),
            housekeeper_dry_run: env_bool("HOUSEKEEPER_DRY_RUN", false),
            claude_bin: env_str("CLAUDE_BIN", "claude"),
            authmux_bin: env_str("AUTHMUX_BIN", "authmux"),
            claude_accounts_dir: env_path(
                "CLAUDE_ACCOUNTS_DIR",
                format!("{home}/.claude-accounts"),
            ),
            log_file: env_path("LOG_FILE", "/tmp/claude-memory-manager.log".to_string()),
            pid_file: env_path("PID_FILE", "/tmp/claude-memory-manager.pid".to_string()),
            lock_file: env_path("LOCK_FILE", "/tmp/claude-memory-manager.lock".to_string()),
            status_sock: env_path("STATUS_SOCK", "/tmp/claude-memory-manager.sock".to_string()),
            claude_config_dir: std::env::var("CLAUDE_CONFIG_DIR").ok().map(PathBuf::from),
        })
    }
}

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
fn env_path(key: &str, default: String) -> PathBuf {
    PathBuf::from(std::env::var(key).unwrap_or(default))
}
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(s) => !matches!(s.to_lowercase().as_str(), "false" | "0" | "no" | "off"),
        Err(_) => default,
    }
}
