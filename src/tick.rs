//! Per-tick audit: spawn `claude -p ...` against the memory-manager subagent.
//!
//! No SDK dependency — we shell out to the installed `claude` CLI. The CLI
//! handles auth (subscription session via $CLAUDE_CONFIG_DIR), agents in
//! `.claude/agents/`, skills in `.claude/skills/`, and MCP wiring. That keeps
//! the daemon tiny and lets the user upgrade `claude` independently.

use crate::config::Config;
use crate::memory::MemoryStat;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{info, warn};

pub struct TickOutcome {
    pub ran: bool,
    pub reason_skipped: Option<String>,
    pub exit_code: Option<i32>,
    pub audit_total_issues: usize,
    pub pre_tick_sha: Option<String>,
}

impl TickOutcome {
    fn skipped(reason: String, audit_issues: usize) -> Self {
        Self {
            ran: false,
            reason_skipped: Some(reason),
            exit_code: None,
            audit_total_issues: audit_issues,
            pre_tick_sha: None,
        }
    }
}

pub async fn run(
    cfg: &Config,
    memory_root: &Path,
    dry_run: bool,
    mem: &MemoryStat,
) -> Result<TickOutcome> {
    // Coordination guards — never race a live session.
    //
    // Strategy: ask lsof which (foreign) PIDs currently hold a file under
    // memory_root open. If lsof is unavailable, fall back to the conservative
    // process-name guard (any other claude/kiro process aborts the tick).
    match crate::process::memory_holders(memory_root) {
        Ok(holders) if !holders.is_empty() => {
            let names: Vec<String> = holders
                .iter()
                .map(|h| format!("{}({})", h.name, h.pid))
                .collect();
            return Ok(TickOutcome::skipped(
                format!("memory files open by: {}", names.join(", ")),
                0,
            ));
        }
        Ok(_) => { /* nobody holds memory open — proceed */ }
        Err(e) => {
            warn!("lsof unavailable ({e}); falling back to process-name guard");
            let live = crate::process::find_claude_sessions();
            if !live.is_empty() {
                return Ok(TickOutcome::skipped(
                    format!(
                        "{} live claude session(s) detected (lsof fallback)",
                        live.len()
                    ),
                    0,
                ));
            }
        }
    }

    if mem.idle_sec < cfg.min_idle.as_secs() {
        return Ok(TickOutcome::skipped(
            format!(
                "memory dir mutated {}s ago (< MIN_IDLE_SEC={})",
                mem.idle_sec,
                cfg.min_idle.as_secs()
            ),
            0,
        ));
    }

    // Cheap deterministic audit — if zero issues, no need to pay for a claude
    // tick. This is the single biggest cost optimization in the daemon.
    let audit = crate::audit::run_audit(memory_root);
    info!(root = %memory_root.display(), audit = %audit.summary(), "rust-side audit");
    if audit.total_issues() == 0 {
        return Ok(TickOutcome::skipped(
            format!("audit clean — {}", audit.summary()),
            0,
        ));
    }

    // Pre-tick git snapshot — only if memory_root is meant to be mutated.
    let mut pre_tick_sha = None;
    if !dry_run && cfg.git_track {
        if let Err(e) = crate::history::ensure_git_repo(memory_root) {
            warn!("git_track enabled but ensure_git_repo failed: {e}");
        } else {
            match crate::history::commit_snapshot(
                memory_root,
                &format!("pre-tick snapshot at unix={}", crate::history::now_unix()),
            ) {
                Ok(Some(sha)) => {
                    info!(sha = %sha, "pre-tick snapshot committed");
                    pre_tick_sha = Some(sha);
                }
                Ok(None) => {
                    // No changes since last commit — use HEAD as the restore target.
                    pre_tick_sha = crate::history::head_sha(memory_root).ok().flatten();
                }
                Err(e) => warn!("pre-tick snapshot failed: {e}"),
            }
        }
    }

    let prompt = build_prompt(memory_root, dry_run, &audit);
    info!(dry_run, model = %cfg.model, "spawning claude CLI for tick");

    let mut cmd = Command::new(&cfg.claude_bin);
    cmd.arg("-p")
        .arg(&prompt)
        .arg("--model")
        .arg(&cfg.model)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--max-turns")
        .arg(cfg.max_turns.to_string())
        .arg("--verbose")
        // Working directory = repo root so .claude/agents and .claude/skills resolve.
        .current_dir(cwd_or_repo()?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    // Tools — when DRY_RUN, deny mutators.
    let allow = if dry_run {
        "Read,Glob,Grep,Bash(find:*),Bash(ps:*)"
    } else {
        "Read,Write,Edit,Glob,Grep,Bash(find:*),Bash(ps:*)"
    };
    cmd.arg("--allowed-tools").arg(allow);

    if let Some(dir) = &cfg.claude_config_dir {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", cfg.claude_bin))?;
    let stdout = child.stdout.take().context("claude stdout missing")?;
    let stderr = child.stderr.take().context("claude stderr missing")?;

    let stdout_task = tokio::spawn(stream_lines("stdout", stdout, true));
    let stderr_task = tokio::spawn(stream_lines("stderr", stderr, false));

    // Tick timeout: if the claude child hasn't finished by `cfg.max_tick`,
    // send SIGTERM, give it 10s, then SIGKILL. Surfaces a hang in the log
    // instead of stalling every subsequent tick request.
    let status = match timeout(cfg.max_tick, child.wait()).await {
        Ok(res) => res?,
        Err(_elapsed) => {
            warn!(
                secs = cfg.max_tick.as_secs(),
                "tick exceeded max_tick — sending SIGTERM"
            );
            if let Some(pid) = child.id() {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGTERM,
                );
            }
            match timeout(std::time::Duration::from_secs(10), child.wait()).await {
                Ok(res) => res?,
                Err(_) => {
                    warn!("child still alive after SIGTERM grace — SIGKILL");
                    let _ = child.start_kill();
                    child.wait().await?
                }
            }
        }
    };
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    if !status.success() {
        warn!(code = ?status.code(), "claude exited non-zero");
    }
    Ok(TickOutcome {
        ran: true,
        reason_skipped: None,
        exit_code: status.code(),
        audit_total_issues: audit.total_issues(),
        pre_tick_sha,
    })
}

fn build_prompt(memory_root: &Path, dry_run: bool, audit: &crate::audit::AuditReport) -> String {
    // Pre-attach the Rust-side audit report so the agent does not redo the
    // boring inventory work. It can focus on judgment calls (merging dupes,
    // adding Why/How lines, deciding whether to prune).
    let pre = serde_json::to_string_pretty(audit).unwrap_or_else(|_| "{}".to_string());
    format!(
        "You are the memory-manager daemon tick agent.

MEMORY_ROOT: {memory_root}
DRY_RUN: {dry_run}

A deterministic Rust-side audit already ran. Findings (JSON):

{pre}

Procedure:
1. Trust the audit report above — do NOT re-scan the whole directory.
2. For each issue, decide whether the FIX is mechanical (broken frontmatter,
   missing index entry, dangling line) or requires judgment (which duplicate
   to keep, what Why/How text to write, prune vs preserve).
3. If DRY_RUN=true, REPORT proposed changes only. Do not Edit or Write.
   If DRY_RUN=false, apply at most THREE targeted Edits this tick. Defer
   the rest to the next tick.
4. End with exactly: 'files=N issues=N applied=N deferred=N — <one-line narrative>'.

Hard rules:
- Never write outside MEMORY_ROOT.
- Never delete files; empty their body and prefix description with '[pruned]'.
- If anything looks like a secret, stop and report. Do not log the value.",
        memory_root = memory_root.display(),
        dry_run = dry_run,
        pre = pre,
    )
}

fn cwd_or_repo() -> Result<std::path::PathBuf> {
    // Daemon is launched from the repo root by start.sh / systemd.
    Ok(std::env::current_dir()?)
}

async fn stream_lines<R: tokio::io::AsyncRead + Unpin>(
    tag: &'static str,
    reader: R,
    parse_json: bool,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if parse_json {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(role) = v.get("type").and_then(|x| x.as_str()) {
                    let preview = v
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.get(0))
                        .and_then(|b| b.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    let snippet: String = preview.chars().take(220).collect();
                    if !snippet.is_empty() {
                        info!(stream = tag, role, "{}", snippet);
                    }
                    continue;
                }
            }
        }
        info!(stream = tag, "{}", line);
    }
}

/// Static check — used by `cmmd doctor`.
pub fn claude_bin_available(claude_bin: &str) -> bool {
    if Path::new(claude_bin).is_absolute() {
        return Path::new(claude_bin).exists();
    }
    std::process::Command::new("which")
        .arg(claude_bin)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
