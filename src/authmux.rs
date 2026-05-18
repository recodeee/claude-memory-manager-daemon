//! Talks to the local `authmux` CLI to surface logged-in Claude / Codex accounts.
//!
//! `authmux list` output (one line per account):
//!   `  email@host  type=...  5h=99%  weekly=84%`
//! The active row is prefixed with `*`. We parse loosely — authmux output
//! is not a stable interface, so anything we don't recognize is dropped, not errored.

use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Default)]
pub struct AuthmuxSnapshot {
    pub binary: String,
    pub available: bool,
    pub current: Option<String>,
    pub accounts: Vec<Account>,
    pub auto_switch: Option<String>,
    pub service_state: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub email: String,
    pub kind: Option<String>,
    pub five_h_pct: Option<u8>,
    pub weekly_pct: Option<u8>,
    pub active: bool,
}

pub async fn snapshot(authmux_bin: &str) -> AuthmuxSnapshot {
    let mut snap = AuthmuxSnapshot {
        binary: authmux_bin.to_string(),
        ..Default::default()
    };

    // Existence probe — `which` is cheap and avoids spawning if absent.
    if which(authmux_bin).await.is_none() {
        return snap;
    }
    snap.available = true;

    snap.current = run_capture(authmux_bin, &["current"]).await.ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    if let Ok(out) = run_capture(authmux_bin, &["list"]).await {
        snap.accounts = parse_list(&out);
    }
    if let Ok(out) = run_capture(authmux_bin, &["status"]).await {
        for line in out.lines() {
            if let Some(v) = line.strip_prefix("auto-switch:") {
                snap.auto_switch = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("service:") {
                snap.service_state = Some(v.trim().to_string());
            }
        }
    }

    snap
}

fn parse_list(out: &str) -> Vec<Account> {
    let mut acc = Vec::new();
    for raw in out.lines() {
        let active = raw.starts_with('*');
        let line = raw.trim_start_matches('*').trim();
        if line.is_empty() {
            continue;
        }
        // First whitespace-delimited token = email (or alias).
        let mut parts = line.split_whitespace();
        let email = match parts.next() {
            Some(e) if e.contains('@') => e.to_string(),
            _ => continue,
        };
        let mut entry = Account { email, kind: None, five_h_pct: None, weekly_pct: None, active };
        // Greedy K=V scan over the rest.
        let rest: Vec<&str> = parts.collect();
        let mut joined = rest.join(" ");
        // type= can contain spaces ("ChatGPT seat (Business)") — capture until the next K= pair.
        if let Some(idx) = joined.find("type=") {
            let after = &joined[idx + 5..];
            let end = ["5h=", "weekly=", "  "]
                .iter()
                .filter_map(|n| after.find(n))
                .min()
                .unwrap_or(after.len());
            entry.kind = Some(after[..end].trim().to_string());
            joined.replace_range(idx..idx + 5 + end, "");
        }
        for tok in joined.split_whitespace() {
            if let Some(v) = tok.strip_prefix("5h=").and_then(parse_pct) {
                entry.five_h_pct = Some(v);
            } else if let Some(v) = tok.strip_prefix("weekly=").and_then(parse_pct) {
                entry.weekly_pct = Some(v);
            }
        }
        acc.push(entry);
    }
    acc
}

fn parse_pct(s: &str) -> Option<u8> {
    s.trim_end_matches('%').parse().ok()
}

async fn run_capture(bin: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(bin).args(args).output().await?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn which(bin: &str) -> Option<String> {
    if Path::new(bin).is_absolute() {
        return Some(bin.to_string());
    }
    let out = Command::new("which").arg(bin).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// Enumerate parallel Claude Code accounts under $CLAUDE_ACCOUNTS_DIR.
/// Each subdir with a `.credentials.json` counts as a logged-in account.
pub fn claude_account_dirs(root: &Path) -> Vec<ClaudeAccountDir> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else { return out };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let creds = path.join(".credentials.json");
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
        out.push(ClaudeAccountDir {
            name,
            has_credentials: creds.exists(),
            path: path.clone(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeAccountDir {
    pub name: String,
    pub has_credentials: bool,
    pub path: std::path::PathBuf,
}
