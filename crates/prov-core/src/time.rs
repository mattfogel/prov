//! Tiny no-dep ISO-8601 timestamp helpers shared by capture (CLI hook) and
//! storage (staging, notes) code paths.
//!
//! Avoids pulling `chrono` / `time` into the workspace for what amounts to
//! two functions. Pure integer arithmetic — Howard Hinnant's civil-from-days.

use std::time::{SystemTime, UNIX_EPOCH};

/// Return the current UTC time formatted as `YYYY-MM-DDThh:mm:ssZ`.
///
/// Uses `SystemTime::now()`. If the system clock is broken (pre-epoch), the
/// fallback is `1970-01-01T00:00:00Z` rather than panicking.
#[must_use]
pub fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (year, month, day, hour, minute, second) = epoch_to_civil(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert a UNIX-epoch second count to civil `(year, month, day, hour,
/// minute, second)`.
///
/// Howard Hinnant's civil-from-days. Pure integer arithmetic. Variable names
/// (`z`, `era`, `doe`, `yoe`, `doy`, `mp`) follow the canonical paper so the
/// algorithm is recognisable; the names are intentionally short and similar.
#[allow(clippy::similar_names, clippy::many_single_char_names)]
#[must_use]
pub fn epoch_to_civil(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let day_secs = 86_400_u64;
    let z = i64::try_from(secs / day_secs).unwrap_or(0) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i32::try_from(yoe).unwrap_or(0) + i32::try_from(era).unwrap_or(0) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let month = u32::try_from(m).unwrap_or(0);
    let day = u32::try_from(d).unwrap_or(0);
    let day_secs_offset = secs % day_secs;
    let hour = u32::try_from(day_secs_offset / 3600).unwrap_or(0);
    let minute = u32::try_from((day_secs_offset % 3600) / 60).unwrap_or(0);
    let second = u32::try_from(day_secs_offset % 60).unwrap_or(0);
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_format_is_parseable() {
        let s = now_iso8601();
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), 20);
    }

    #[test]
    fn epoch_to_civil_handles_known_values() {
        // 1970-01-01T00:00:00Z
        assert_eq!(epoch_to_civil(0), (1970, 1, 1, 0, 0, 0));
        // 2024-01-01T00:00:00Z = 1_704_067_200
        assert_eq!(epoch_to_civil(1_704_067_200), (2024, 1, 1, 0, 0, 0));
        // 2026-04-28T12:34:56Z = 1_777_379_696
        assert_eq!(epoch_to_civil(1_777_379_696), (2026, 4, 28, 12, 34, 56));
    }
}
