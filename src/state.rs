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

    /// Date (UTC, "YYYY-MM-DD") that `day_ticks_ran` is counting against.
    /// When the daemon notices the date has rolled over, it resets the
    /// counter. Persisted so the cap survives restarts within the same day.
    #[serde(default)]
    pub day_ticks_ran_date: String,
    /// Count of *ran* ticks (not skipped) recorded against
    /// `day_ticks_ran_date`. The MAX_TICKS_PER_DAY ceiling is checked
    /// against this before each agent spawn.
    #[serde(default)]
    pub day_ticks_ran: u64,
}

/// Reset the daily counter if the UTC date has rolled. Returns the current
/// counter value (post-reset).
pub fn bump_day_if_needed(state: &mut PersistedState, today_utc: &str) -> u64 {
    if state.day_ticks_ran_date != today_utc {
        state.day_ticks_ran_date = today_utc.to_string();
        state.day_ticks_ran = 0;
    }
    state.day_ticks_ran
}

/// UTC-date string ("YYYY-MM-DD") for an absolute unix timestamp. Avoids a
/// chrono dependency — the daemon doesn't need any other date math.
pub fn utc_date_string(unix: u64) -> String {
    // Days since 1970-01-01 (Thursday).
    let days = (unix / 86_400) as i64;
    // Civil-from-days algorithm (Howard Hinnant's date.h, public domain).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_date_known_anchors() {
        // 1970-01-01 00:00:00 UTC = unix 0
        assert_eq!(utc_date_string(0), "1970-01-01");
        // 2026-05-18 00:00:00 UTC = 1_779_062_400 (20591 days × 86400)
        assert_eq!(utc_date_string(1_779_062_400), "2026-05-18");
        // 2026-05-18 23:59:59 UTC stays on the same day
        assert_eq!(utc_date_string(1_779_148_799), "2026-05-18");
        // 2026-05-19 00:00:00 UTC rolls over
        assert_eq!(utc_date_string(1_779_148_800), "2026-05-19");
        // Leap-year edge: 2024-02-29
        assert_eq!(utc_date_string(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn bump_day_resets_counter_on_date_change() {
        let mut s = PersistedState {
            day_ticks_ran_date: "2026-05-17".to_string(),
            day_ticks_ran: 42,
            ..Default::default()
        };
        bump_day_if_needed(&mut s, "2026-05-18");
        assert_eq!(s.day_ticks_ran, 0);
        assert_eq!(s.day_ticks_ran_date, "2026-05-18");
    }

    #[test]
    fn bump_day_preserves_counter_within_same_day() {
        let mut s = PersistedState {
            day_ticks_ran_date: "2026-05-18".to_string(),
            day_ticks_ran: 7,
            ..Default::default()
        };
        bump_day_if_needed(&mut s, "2026-05-18");
        assert_eq!(s.day_ticks_ran, 7);
    }
}
