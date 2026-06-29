//! Host-side per-instance transport record — `liveness.json`.
//!
//! Two host-owned facts live here, both connection-scoped to an attached
//! `drive`:
//!
//! - the **transport heartbeat** freshness (`lastHeartbeatAt` / `streamActive`),
//!   written by the `prompt` watchdog as `Heartbeat`s arrive — *silently*, a file
//!   write that never echoes to the orchestrator;
//! - the **turn-id counter** (`nextTurnId`), the monotonic id [`alloc_turn_id`]
//!   hands each turn. Because the file lives in the persistent state dir, a named
//!   instance's counter continues across `prompt` invocations → no id reuse,
//!   which keeps the guest's `turn_id` dedupe correct.
//!
//! Reads and writes go through the [`Host`] seam, and timestamps the host stamps
//! go through the host clock seam ([`now_rfc3339`], mirroring `start.rs`'s
//! `now_timestamp`), so the whole record is `FakeHost`-testable without a VM or a
//! real clock. The companion guest-authored `turn-state.json`
//! is a separate record read out-of-band by `status`; it is not written
//! here.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::sandbox::host::Host;

/// The `liveness.json` schema version this build writes/reads.
pub const SUPPORTED_LIVENESS_VERSION: u32 = 1;

/// The host-authored `liveness.json` record. Lives beside
/// `instance.json` at `<stateGlob>/<inst>/liveness.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Liveness {
    /// Schema version (`= SUPPORTED_LIVENESS_VERSION`).
    pub liveness_version: u32,
    /// The next turn id [`alloc_turn_id`] will hand out (monotonic, persisted).
    pub next_turn_id: u64,
    /// RFC3339 time of the last `Heartbeat` seen on the open stream; `None` until
    /// the first heartbeat (and between streams) — omitted from the JSON then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<String>,
    /// Whether a `drive` currently holds the control stream open.
    #[serde(default)]
    pub stream_active: bool,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            liveness_version: SUPPORTED_LIVENESS_VERSION,
            next_turn_id: 1,
            last_heartbeat_at: None,
            stream_active: false,
        }
    }
}

impl Liveness {
    /// Read the record through the [`Host`] seam, falling back to [`Default`]
    /// when the file is missing or unreadable/unparseable. Unlike the fail-loud
    /// `instance.json` (a cross-party contract), this is the host's own scratch
    /// record, rewritten every tick — so a corrupt read self-heals on the next
    /// write rather than failing the turn.
    pub fn load(host: &impl Host, path: &Path) -> Self {
        match host.read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Write the record through the [`Host`] seam atomically: serialize to a temp
    /// sibling, then rename over the target (rename is atomic within a
    /// filesystem). 's `status` reads this file out-of-band, so a reader must
    /// never observe a torn write, and a crash mid-write must not clobber
    /// `nextTurnId` (— same temp+rename the guest uses for
    /// turn-state.json). Rewritten at most once per second by the watchdog touch.
    pub fn store(&self, host: &impl Host, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self).context("serializing liveness.json")?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("liveness.json");
        let tmp = path.with_file_name(format!(".{name}.tmp"));
        host.write(&tmp, &bytes)
            .with_context(|| format!("writing liveness.json temp {}", tmp.display()))?;
        host.rename(&tmp, path)
            .with_context(|| format!("renaming liveness.json into {}", path.display()))?;
        Ok(())
    }
}

impl Liveness {
    /// Read the host-authored record *for `status`*: unlike
    /// [`Liveness::load`] — which self-heals a corrupt/missing file to [`Default`]
    /// for the writer — this returns `None` when the file is missing, unreadable,
    /// unparseable, or version-skewed, so `status` can tell "no transport record
    /// yet" apart from "a real, fresh stream". Advisory: a degraded read is
    /// never an error, it just drops the transport half of the liveness line.
    pub fn read(host: &impl Host, path: &Path) -> Option<Self> {
        let bytes = host.read(path).ok()?;
        let liveness: Self = serde_json::from_slice(&bytes).ok()?;
        (liveness.liveness_version == SUPPORTED_LIVENESS_VERSION).then_some(liveness)
    }
}

/// The `turn-state.json` schema version this build reads.
pub const SUPPORTED_TURN_STATE_VERSION: u32 = 1;

/// The lifecycle phase persisted to `turn-state.json`, mirroring the
/// guest's authoritative state machine: `in-flight` on inject, `ended-ok` on a
/// terminal report, `ended-unreported` when the grace window closes with no
/// terminal report (the verdict), `idle` between turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Idle,
    InFlight,
    EndedOk,
    EndedUnreported,
}

impl Phase {
    /// The wire/render token for this phase (`status`'s liveness line shows it
    /// verbatim, so it doubles as the operator-facing surfacing of 's
    /// "stopped without reporting").
    pub fn label(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::InFlight => "in-flight",
            Phase::EndedOk => "ended-ok",
            Phase::EndedUnreported => "ended-unreported",
        }
    }

    /// Whether the turn has finished (clean or unreported) — its age is measured
    /// from `endedAt`.
    pub fn is_ended(self) -> bool {
        matches!(self, Phase::EndedOk | Phase::EndedUnreported)
    }

    /// Whether a turn is still in flight — its age is measured from
    /// `lastActivityAt`, and a stale value is how the never-`Stop` hang
    /// becomes visible out-of-band.
    pub fn is_in_flight(self) -> bool {
        matches!(self, Phase::InFlight)
    }
}

/// The guest-authored, read-only `turn-state.json` record:
/// authoritative for turn/agent state and written to the share on every
/// transition, so `status` reads it out-of-band (no connection needed). The host
/// only ever *reads* it (owns the host-written [`Liveness`] writer); the
/// reader is forward-compatible (no `deny_unknown_fields`) so a newer guest can
/// add fields without breaking an older host's degraded read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnState {
    /// Schema version (`= SUPPORTED_TURN_STATE_VERSION`).
    pub turn_state_version: u32,
    /// The turn this record describes; `None` while `idle` (no turn yet).
    #[serde(default)]
    pub turn_id: Option<u64>,
    /// The lifecycle phase.
    pub phase: Phase,
    /// RFC3339 time the first activity was seen (`None` until accepted).
    #[serde(default)]
    pub accepted_at: Option<String>,
    /// RFC3339 time the turn ended (`None` until an ended phase).
    #[serde(default)]
    pub ended_at: Option<String>,
    /// RFC3339 time of the last report/hook (bumped throttled to ≤1/s); empty
    /// before any activity.
    #[serde(default)]
    pub last_activity_at: String,
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            turn_state_version: SUPPORTED_TURN_STATE_VERSION,
            turn_id: None,
            phase: Phase::Idle,
            accepted_at: None,
            ended_at: None,
            last_activity_at: String::new(),
        }
    }
}

impl TurnState {
    /// Read the guest-authored record through the [`Host`] seam, returning `None`
    /// when the file is missing, unreadable, unparseable, or version-skewed.
    /// Advisory: a degraded read is never an error — `status` simply
    /// degrades to today's connection-derived behavior.
    pub fn read(host: &impl Host, path: &Path) -> Option<Self> {
        let bytes = host.read(path).ok()?;
        let state: Self = serde_json::from_slice(&bytes).ok()?;
        (state.turn_state_version == SUPPORTED_TURN_STATE_VERSION).then_some(state)
    }
}

/// Parse the exact RFC3339 shape both records use — `YYYY-MM-DDTHH:MM:SSZ`, UTC,
/// second precision (the `date -u +%Y-%m-%dT%H:%M:%SZ` the host writes and the
/// guest's matching clock) — into Unix seconds, with **no** `chrono` dependency
/// (the sandbox has no crates.io access). Anything not matching that shape (a
/// fractional/offset form, an empty `lastActivityAt`, garbage) returns `None`, so
/// the age simply isn't rendered — advisory, never an error.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    // Strict fixed-width layout: 19 chars + 'Z'.
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |lo: usize, hi: usize| -> Option<i64> {
        let mut v: i64 = 0;
        for &c in &b[lo..hi] {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + i64::from(c - b'0');
        }
        Some(v)
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(civil_to_unix(y, mo, d) * 86_400 + h * 3_600 + mi * 60 + s)
}

/// Days since the Unix epoch for a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil`), so [`parse_rfc3339`] needs no calendar crate.
fn civil_to_unix(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A compact "how long ago" token (`4s`, `9m`, `2h`, `3d`) for `now - then`,
/// clamped at zero so a little host/guest clock skew reads as `0s` rather than a
/// negative age. The caller appends `" ago"`.
pub fn humanize_ago(now: i64, then: i64) -> String {
    let d = (now - then).max(0);
    if d < 60 {
        format!("{d}s")
    } else if d < 3_600 {
        format!("{}m", d / 60)
    } else if d < 86_400 {
        format!("{}h", d / 3_600)
    } else {
        format!("{}d", d / 86_400)
    }
}

/// The current time as Unix seconds, via the host clock seam ([`now_rfc3339`]),
/// for `status` to take liveness ages against. `None` on any clock-seam failure
/// (advisory: the line then renders phases without ages).
pub fn now_unix(host: &impl Host) -> Option<i64> {
    parse_rfc3339(&now_rfc3339(host).ok()?)
}

/// Allocate the next monotonic `turn_id`, persisting the bump in `liveness.json`
/// through the [`Host`] seam. The id handed out is the current
/// `nextTurnId` (default 1); `nextTurnId` is incremented and written back, so the
/// counter survives across `prompt` invocations. A resend reuses the *same* id
/// (which the guest dedupes), while two distinct turns can never collide.
pub fn alloc_turn_id(host: &impl Host, path: &Path) -> Result<u64> {
    let mut liveness = Liveness::load(host, path);
    let id = liveness.next_turn_id;
    liveness.next_turn_id = id.checked_add(1).context("turn-id counter overflow")?;
    liveness.store(host, path)?;
    Ok(id)
}

/// The current UTC time as an RFC3339 string, produced through the host clock
/// seam (`date -u`, mirroring `start.rs`'s `now_timestamp`) so liveness writes
/// stay `FakeHost`-testable. The silent heartbeat touch is best-
/// effort and swallows the error: a missing timestamp never fails a turn.
pub fn now_rfc3339(host: &impl Host) -> Result<String> {
    let mut cmd = Command::new("date");
    cmd.arg("-u").arg("+%Y-%m-%dT%H:%M:%SZ");
    let out = host
        .run(&cmd)
        .context("running `date` for the liveness timestamp")?;
    let stamp = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stamp.is_empty() {
        bail!("`date` produced no timestamp");
    }
    Ok(stamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host::{Call, FakeHost};
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::{ExitStatus, Output};

    const LIVENESS: &str = "/state/cdata/katsuobushi/inst-1/liveness.json";

    fn path() -> PathBuf {
        PathBuf::from(LIVENESS)
    }

    /// A canned successful `run` output with `stdout` set (for the clock seam).
    fn run_output(stdout: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    /// Pull the `Liveness` out of the n-th recorded `Write`.
    fn nth_write(host: &FakeHost, n: usize) -> Liveness {
        match &host.calls()[n] {
            Call::Write(_, bytes) => serde_json::from_slice(bytes).expect("a Liveness was written"),
            other => panic!("call {n} was not a Write: {other:?}"),
        }
    }

    #[test]
    fn it_starts_the_turn_id_counter_at_one_when_the_file_is_missing() {
        // No scripted read → FakeHost::read returns NotFound → default (start 1),
        // and the bumped counter (2) is written back.
        let host = FakeHost::new();
        let id = alloc_turn_id(&host, &path()).expect("alloc should succeed");
        assert_eq!(id, 1, "first id is 1 with no prior file");
        assert_eq!(nth_write(&host, 1).next_turn_id, 2, "counter bumped to 2");
    }

    #[test]
    fn it_resumes_the_turn_id_counter_from_the_persisted_file() {
        // A persisted nextTurnId of 4 → hands out 4, persists 5 (no reuse across
        // invocations, the guarantee for a named instance).
        let mut host = FakeHost::new();
        host.push_read(Ok(serde_json::to_vec(&Liveness {
            next_turn_id: 4,
            ..Liveness::default()
        })
        .unwrap()));
        let id = alloc_turn_id(&host, &path()).expect("alloc should succeed");
        assert_eq!(id, 4, "hands out the persisted nextTurnId");
        assert_eq!(nth_write(&host, 1).next_turn_id, 5, "and persists +1");
    }

    #[test]
    fn it_preserves_heartbeat_fields_across_a_turn_id_alloc() {
        // Allocating an id must not clobber a prior heartbeat record.
        let mut host = FakeHost::new();
        host.push_read(Ok(serde_json::to_vec(&Liveness {
            next_turn_id: 2,
            last_heartbeat_at: Some("2026-06-28T00:00:00Z".to_string()),
            stream_active: true,
            ..Liveness::default()
        })
        .unwrap()));
        alloc_turn_id(&host, &path()).expect("alloc should succeed");
        let written = nth_write(&host, 1);
        assert_eq!(
            written.last_heartbeat_at.as_deref(),
            Some("2026-06-28T00:00:00Z")
        );
        assert!(written.stream_active);
    }

    #[test]
    fn it_loads_a_default_when_the_record_is_unparseable() {
        let mut host = FakeHost::new();
        host.push_read(Ok(b"{ not json".to_vec()));
        let liveness = Liveness::load(&host, &path());
        assert_eq!(liveness, Liveness::default());
    }

    #[test]
    fn it_round_trips_a_heartbeat_touch_through_the_seam() {
        // Load → stamp lastHeartbeatAt via the clock seam → store: the written
        // record carries the timestamp and streamActive, exactly the watchdog
        // touch path, all without a VM or a real clock.
        let mut host = FakeHost::new();
        host.push_read(Ok(serde_json::to_vec(&Liveness {
            next_turn_id: 3,
            ..Liveness::default()
        })
        .unwrap()))
            .push_run(Ok(run_output("2026-06-28T12:34:56Z\n")));

        let mut liveness = Liveness::load(&host, &path());
        liveness.last_heartbeat_at = Some(now_rfc3339(&host).expect("clock seam"));
        liveness.stream_active = true;
        liveness
            .store(&host, &path())
            .expect("store should succeed");

        let written = nth_write(&host, 2); // [0]=Read, [1]=Run(date), [2]=Write
        assert_eq!(written.next_turn_id, 3, "counter preserved by a touch");
        assert_eq!(
            written.last_heartbeat_at.as_deref(),
            Some("2026-06-28T12:34:56Z")
        );
        assert!(written.stream_active);
    }

    #[test]
    fn it_writes_liveness_atomically_via_temp_then_rename() {
        // The record must land atomically — write a temp sibling, then rename
        // it over the target — so an out-of-band reader never sees a torn file.
        let host = FakeHost::new();
        Liveness::default()
            .store(&host, &path())
            .expect("store should succeed");
        let calls = host.calls();
        match (&calls[0], &calls[1]) {
            (Call::Write(tmp, _), Call::Rename(from, to)) => {
                assert_eq!(
                    tmp, from,
                    "rename moves the same temp file that was written"
                );
                assert_eq!(to, &path(), "rename targets the final liveness.json");
                assert_ne!(
                    tmp,
                    &path(),
                    "the write goes to a temp sibling, not the target"
                );
            }
            other => panic!("expected Write then Rename, got {other:?}"),
        }
    }

    #[test]
    fn it_serializes_liveness_with_camel_case_keys() {
        let json = serde_json::to_string(&Liveness {
            next_turn_id: 4,
            last_heartbeat_at: Some("2026-06-28T12:00:00Z".to_string()),
            stream_active: true,
            ..Liveness::default()
        })
        .expect("serialize");
        assert!(json.contains("\"livenessVersion\":1"), "json: {json}");
        assert!(json.contains("\"nextTurnId\":4"), "json: {json}");
        assert!(json.contains("\"lastHeartbeatAt\""), "json: {json}");
        assert!(json.contains("\"streamActive\":true"), "json: {json}");
    }

    #[test]
    fn it_omits_the_heartbeat_timestamp_before_the_first_beat() {
        // The alloc-only record has no heartbeat yet → the key is absent.
        let json = serde_json::to_string(&Liveness::default()).expect("serialize");
        assert!(!json.contains("lastHeartbeatAt"), "json: {json}");
    }

    #[test]
    fn it_errors_when_the_clock_seam_produces_no_timestamp() {
        // An empty `date` stdout is a hard error for the (fallible) seam itself;
        // the watchdog's best-effort touch is what swallows it in production.
        let host = FakeHost::new(); // default run → empty stdout
        let err = now_rfc3339(&host).expect_err("empty timestamp must error");
        assert!(format!("{err:#}").contains("no timestamp"), "{err:#}");
    }

    // ---- the guest-authored turn-state reader ----

    const TURN_STATE: &str = "/state/cdata/katsuobushi/inst-1/turn-state.json";

    fn turn_state_path() -> PathBuf {
        PathBuf::from(TURN_STATE)
    }

    /// A guest-shaped `turn-state.json` payload — exactly the bytes the guest's
    /// server writes, so the reader is tested against the real wire shape.
    fn turn_state_bytes(phase: &str, extra: &str) -> Vec<u8> {
        format!(
            r#"{{"turnStateVersion":1,"turnId":3,"phase":"{phase}",{extra}"lastActivityAt":"2026-06-28T12:00:00Z"}}"#
        )
        .into_bytes()
    }

    #[test]
    fn it_reads_an_in_flight_turn_state_through_the_seam() {
        let mut host = FakeHost::new();
        host.push_read(Ok(turn_state_bytes(
            "in-flight",
            r#""acceptedAt":"2026-06-28T11:50:00Z","endedAt":null,"#,
        )));
        let ts = TurnState::read(&host, &turn_state_path()).expect("a present record reads");
        assert_eq!(ts.phase, Phase::InFlight);
        assert_eq!(ts.turn_id, Some(3));
        assert_eq!(ts.accepted_at.as_deref(), Some("2026-06-28T11:50:00Z"));
        assert_eq!(ts.last_activity_at, "2026-06-28T12:00:00Z");
        // The read goes through the seam at the requested path.
        assert!(matches!(&host.calls()[0], Call::Read(p) if p == &turn_state_path()));
    }

    #[test]
    fn it_reads_the_ended_unreported_phase() {
        let mut host = FakeHost::new();
        host.push_read(Ok(turn_state_bytes(
            "ended-unreported",
            r#""acceptedAt":"2026-06-28T11:50:00Z","endedAt":"2026-06-28T11:55:00Z","#,
        )));
        let ts = TurnState::read(&host, &turn_state_path()).expect("reads");
        assert_eq!(ts.phase, Phase::EndedUnreported);
        assert_eq!(ts.ended_at.as_deref(), Some("2026-06-28T11:55:00Z"));
    }

    #[test]
    fn it_returns_none_for_a_missing_turn_state_file() {
        // No scripted read → NotFound → advisory None (degrade, never an error).
        let host = FakeHost::new();
        assert!(TurnState::read(&host, &turn_state_path()).is_none());
    }

    #[test]
    fn it_returns_none_for_an_unparseable_turn_state_file() {
        let mut host = FakeHost::new();
        host.push_read(Ok(b"{ not json".to_vec()));
        assert!(TurnState::read(&host, &turn_state_path()).is_none());
    }

    #[test]
    fn it_returns_none_for_a_version_skewed_turn_state_file() {
        // A newer/older schema version degrades rather than mis-parsing.
        let mut host = FakeHost::new();
        host.push_read(Ok(
            br#"{"turnStateVersion":2,"phase":"idle","lastActivityAt":""}"#.to_vec(),
        ));
        assert!(TurnState::read(&host, &turn_state_path()).is_none());
    }

    #[test]
    fn it_tolerates_unknown_forward_compatible_fields() {
        // A newer guest may add fields; an older host must still read what it knows.
        let mut host = FakeHost::new();
        host.push_read(Ok(
            br#"{"turnStateVersion":1,"phase":"idle","lastActivityAt":"","futureField":7}"#
                .to_vec(),
        ));
        let ts = TurnState::read(&host, &turn_state_path()).expect("forward-compatible read");
        assert_eq!(ts.phase, Phase::Idle);
    }

    // ---- the status-facing liveness reader ----

    #[test]
    fn it_reads_liveness_for_status_when_fresh() {
        let mut host = FakeHost::new();
        host.push_read(Ok(serde_json::to_vec(&Liveness {
            next_turn_id: 4,
            last_heartbeat_at: Some("2026-06-28T12:00:00Z".to_string()),
            stream_active: true,
            ..Liveness::default()
        })
        .unwrap()));
        let lv = Liveness::read(&host, &path()).expect("a present record reads");
        assert!(lv.stream_active);
        assert_eq!(
            lv.last_heartbeat_at.as_deref(),
            Some("2026-06-28T12:00:00Z")
        );
    }

    #[test]
    fn it_returns_none_reading_a_missing_or_corrupt_liveness_for_status() {
        // Unlike `load` (self-heals to default for the writer), `read` degrades to
        // None so `status` can distinguish "no transport record" from a live one.
        let host = FakeHost::new();
        assert!(Liveness::read(&host, &path()).is_none(), "missing → None");
        let mut host = FakeHost::new();
        host.push_read(Ok(b"{ not json".to_vec()));
        assert!(Liveness::read(&host, &path()).is_none(), "corrupt → None");
    }

    // ---- RFC3339 parse + age formatting (pure) ----

    #[test]
    fn it_parses_the_rfc3339_shape_both_records_use() {
        // 2026-06-28T12:00:00Z — cross-checked against a known Unix instant.
        assert_eq!(parse_rfc3339("2026-06-28T12:00:00Z"), Some(1_782_648_000));
        // The epoch itself.
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn it_rejects_malformed_or_empty_timestamps() {
        for bad in [
            "",                       // empty lastActivityAt before any activity
            "2026-06-28",             // date only
            "2026-06-28T12:00:00",    // missing Z
            "2026-06-28T12:00:00.5Z", // fractional seconds (different shape)
            "2026-13-28T12:00:00Z",   // month out of range
            "not-a-time",
        ] {
            assert!(parse_rfc3339(bad).is_none(), "should reject {bad:?}");
        }
    }

    #[test]
    fn it_humanizes_an_age_into_a_compact_token() {
        let now = parse_rfc3339("2026-06-28T12:00:00Z").unwrap();
        assert_eq!(humanize_ago(now, now - 4), "4s");
        assert_eq!(humanize_ago(now, now - 9 * 60), "9m");
        assert_eq!(humanize_ago(now, now - 2 * 3_600), "2h");
        assert_eq!(humanize_ago(now, now - 3 * 86_400), "3d");
        // Clock skew (then > now) clamps to 0s rather than a negative age.
        assert_eq!(humanize_ago(now, now + 30), "0s");
    }

    #[test]
    fn it_derives_now_unix_through_the_clock_seam() {
        let mut host = FakeHost::new();
        host.push_run(Ok(run_output("2026-06-28T12:00:00Z\n")));
        assert_eq!(now_unix(&host), Some(1_782_648_000));
        // A failed clock seam degrades to None (no age rendered), never an error.
        let host = FakeHost::new();
        assert!(now_unix(&host).is_none());
    }
}
