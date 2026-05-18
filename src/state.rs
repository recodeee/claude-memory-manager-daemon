//! Tiny persisted state for the daemon, so things like a runtime
//! `mmctl dry-run off` survive a restart.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedState {
    /// Runtime override of the configured dry_run. None = no override
    /// (use the configured value).
    pub dry_run_override: Option<bool>,
}

pub fn load(path: &Path) -> PersistedState {
    let Ok(body) = std::fs::read_to_string(path) else {
        return PersistedState::default();
    };
    serde_json::from_str(&body).unwrap_or_default()
}

pub fn save(path: &Path, state: &PersistedState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let body = serde_json::to_string_pretty(state).context("encode state")?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}
