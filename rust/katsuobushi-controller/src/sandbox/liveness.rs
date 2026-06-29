//! Host-side per-instance transport record — `liveness.json`
//! (design/sandbox-liveness.md §9).
//!
//! Two host-owned facts live here, both connection-scoped to an attached `drive`
//! (design §9):
//!
//! - the **transport heartbeat** freshness (`lastHeartbeatAt` / `streamActive`),
//!   written by the `prompt` watchdog as `Heartbeat`s arrive — *silently*, a file
//!   write that never echoes to the orchestrator (design §8.1);
//! - the **turn-id counter** (`nextTurnId`), the monotonic id [`alloc_turn_id`]
//!   hands each turn. Because the file lives in the persistent state dir, a named
//!   instance's counter continues across `prompt` invocations → no id reuse,
//!   which keeps the guest's `turn_id` dedupe correct (design §7.2).
//!
//! Reads and writes go through the [`Host`] seam, and timestamps the host stamps
//! go through the host clock seam ([`now_rfc3339`], mirroring `start.rs`'s
//! `now_timestamp`), so the whole record is `FakeHost`-testable without a VM or a
//! real clock (design §9, §12). The companion guest-authored `turn-state.json`
//! (§6.3) is a separate record read out-of-band by `status`; it is not written
//! here.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::sandbox::host::Host;

/// The `liveness.json` schema version this build writes/reads (design §9, §17).
pub const SUPPORTED_LIVENESS_VERSION: u32 = 1;

/// The host-authored `liveness.json` record (design §9, §17). Lives beside
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

    /// Write the record through the [`Host`] seam (compact JSON). Rewritten at
    /// most once per second by the watchdog touch; the record is tiny and
    /// `status` treats it as advisory (design §15 stale-read safety), so a whole-
    /// file write is sufficient — a torn read self-heals on the next tick.
    pub fn store(&self, host: &impl Host, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(self).context("serializing liveness.json")?;
        host.write(path, &bytes)
            .with_context(|| format!("writing liveness.json to {}", path.display()))?;
        Ok(())
    }
}

/// Allocate the next monotonic `turn_id`, persisting the bump in `liveness.json`
/// through the [`Host`] seam (design §7.2 / §9). The id handed out is the current
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
/// stay `FakeHost`-testable (design §9). The silent heartbeat touch is best-
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
        // invocations, the §7.2 guarantee for a named instance).
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
        // touch path (design §8.1/§9), all without a VM or a real clock.
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
}
