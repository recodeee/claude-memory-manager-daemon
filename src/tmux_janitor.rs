//! Tmux session janitor: kills unattached `term-*` sessions that linger
//! after their terminal window was closed.

use serde::Serialize;
use std::process::Command;
use tracing::info;

#[derive(Debug, Clone, Serialize)]
pub struct TmuxSession {
    pub name: String,
    pub attached: bool,
    pub windows: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupResult {
    pub scanned: usize,
    pub killed: Vec<String>,
}

/// List all tmux sessions on the default server.
fn list_sessions() -> Vec<TmuxSession> {
    let out = Command::new("tmux")
        .args([
            "list-sessions",
            "-F",
            "#{session_name} #{session_attached} #{session_windows}",
        ])
        .output();
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                Some(TmuxSession {
                    name: parts[0].to_string(),
                    attached: parts[1] != "0",
                    windows: parts[2].parse().unwrap_or(0),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Kill unattached sessions matching the `term-*` pattern.
/// These are auto-created by .bashrc and should die when the window closes.
pub fn cleanup_unattached() -> CleanupResult {
    let sessions = list_sessions();
    let mut killed = Vec::new();

    for sess in &sessions {
        if !sess.attached && sess.name.starts_with("term-") {
            let status = Command::new("tmux")
                .args(["kill-session", "-t", &sess.name])
                .status();
            if status.map(|s| s.success()).unwrap_or(false) {
                info!(session = %sess.name, "killed unattached tmux session");
                killed.push(sess.name.clone());
            }
        }
    }

    CleanupResult {
        scanned: sessions.len(),
        killed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_sessions_does_not_panic() {
        // Just ensure it doesn't crash even if tmux isn't running
        let _ = list_sessions();
    }
}
