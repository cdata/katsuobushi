//! The wall-clock seam and a library-free RFC3339 formatter.
//!
//! `new` stamps `created:` on a card. To keep that deterministic under test the
//! clock is injected: production uses [`SystemClock`], tests a [`FixedClock`].
//! `created` is metadata only — it no longer sorts anything (priority is board
//! position) — but a readable date is worth the small civil-date helper below.
//!
//! No `chrono` is vendored (see `sandbox/screenshot.rs`), so
//! [`format_rfc3339`] converts Unix seconds to a UTC calendar date with
//! Howard Hinnant's well-known `civil_from_days` algorithm, unit-tested against
//! known instants.

use std::time::{SystemTime, UNIX_EPOCH};

/// A source of wall-clock time as whole Unix seconds. The seam that makes
/// `created` stamps deterministic in tests.
pub trait Clock {
    fn now_unix(&self) -> i64;
}

/// Production clock: the real system time.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        // Pre-epoch clocks are absurd on a dev host; clamp to 0 rather than
        // panic, mirroring `screenshot.rs`'s defensive `unwrap_or`.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// Test clock: returns a fixed instant.
#[cfg(test)]
pub struct FixedClock(pub i64);

#[cfg(test)]
impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        self.0
    }
}

/// Format Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC, RFC3339). Pure and
/// total over all `i64` (negative = pre-epoch, handled by Euclidean division).
pub fn format_rfc3339(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (h, mi, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Parse an RFC3339 `YYYY-MM-DDTHH:MM:SSZ` stamp back to Unix seconds — the
/// inverse of [`format_rfc3339`], for comparing a `disposition_at` against a
/// window. Deliberately strict: it accepts only the exact shape this domain
/// emits (UTC `Z`, no fractional seconds, no offset). `None` on any deviation.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut dp = date.split('-');
    let (y, mo, d) = (
        parse_i64(dp.next()?)?,
        parse_u32(dp.next()?)?,
        parse_u32(dp.next()?)?,
    );
    if dp.next().is_some() {
        return None;
    }
    let mut tp = time.split(':');
    let (h, mi, sec) = (
        parse_i64(tp.next()?)?,
        parse_i64(tp.next()?)?,
        parse_i64(tp.next()?)?,
    );
    if tp.next().is_some() {
        return None;
    }
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Range-check the clock fields too, so a hand-corrupted-but-numeric stamp
    // (hour 99, minute 61) is rejected rather than silently arithmetic'd into a
    // bogus instant — it should fall through to `hidden`, not parse.
    if !(0..=23).contains(&h) || !(0..=59).contains(&mi) || !(0..=59).contains(&sec) {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec)
}

fn parse_i64(s: &str) -> Option<i64> {
    s.parse().ok()
}
fn parse_u32(s: &str) -> Option<u32> {
    s.parse().ok()
}

/// `(year, month, day)` → days since the Unix epoch (1970-01-01). The inverse of
/// [`civil_from_days`]; Howard Hinnant's `days_from_civil`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp as i64 + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Days since the Unix epoch (1970-01-01) → `(year, month, day)`.
/// Howard Hinnant, "chrono-Compatible Low-Level Date Algorithms"
/// (<http://howardhinnant.github.io/date_algorithms.html#civil_from_days>).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_instants_format_correctly() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        // 2026-07-17T18:22:04Z — the example stamp from design/project.md.
        assert_eq!(format_rfc3339(1_784_312_524), "2026-07-17T18:22:04Z");
        // A leap day.
        assert_eq!(format_rfc3339(1_582_934_400), "2020-02-29T00:00:00Z");
        // End-of-year boundary.
        assert_eq!(format_rfc3339(1_609_459_199), "2020-12-31T23:59:59Z");
    }

    #[test]
    fn fixed_clock_feeds_the_formatter() {
        let clock = FixedClock(1_784_312_524);
        assert_eq!(format_rfc3339(clock.now_unix()), "2026-07-17T18:22:04Z");
    }

    #[test]
    fn pre_epoch_is_total() {
        // 1969-12-31T23:59:59Z — one second before the epoch.
        assert_eq!(format_rfc3339(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn parse_round_trips_the_formatter() {
        for secs in [0_i64, 1_784_312_524, 1_582_934_400, 1_609_459_199, -1] {
            assert_eq!(parse_rfc3339(&format_rfc3339(secs)), Some(secs), "{secs}");
        }
    }

    #[test]
    fn parse_rejects_malformed_stamps() {
        assert_eq!(parse_rfc3339("2026-07-19"), None); // no time
        assert_eq!(parse_rfc3339("2026-07-19T00:00:00"), None); // no Z
        assert_eq!(parse_rfc3339("2026-07-19T00:00:00+01:00"), None); // offset
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None); // month 13
        assert_eq!(parse_rfc3339("2026-07-19T24:00:00Z"), None); // hour 24
        assert_eq!(parse_rfc3339("2026-07-19T99:00:00Z"), None); // hour 99
        assert_eq!(parse_rfc3339("2026-07-19T00:61:00Z"), None); // minute 61
        assert_eq!(parse_rfc3339("2026-07-19T00:00:99Z"), None); // second 99
        assert_eq!(parse_rfc3339("not-a-date"), None);
        assert_eq!(parse_rfc3339(""), None);
    }
}
