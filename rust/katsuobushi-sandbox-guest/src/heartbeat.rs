//! Heartbeat definitions: parse `.katsuobushi/heartbeats/*.yaml` files into
//! typed [`Heartbeat`] values.
//!
//! Parsing is separated from I/O so the parser is testable with inline strings;
//! [`load_heartbeats`] handles the filesystem walk and collects both successes
//! and errors without aborting on the first failure.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Transport cadence default (10 s) — also the default for an absent `interval`.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

/// A successfully parsed heartbeat definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    pub label: String,
    pub timeout: Duration,
    /// Polling cadence; defaults to [`DEFAULT_INTERVAL`] when absent in the file.
    pub interval: Duration,
    /// Shell body run to check liveness.
    pub check: String,
    /// Optional shell body whose stdout enriches the status line.
    pub detail: Option<String>,
}

impl fmt::Display for Heartbeat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{} ({})", self.label, detail.trim()),
            None => write!(f, "{}", self.label),
        }
    }
}

/// A file that could not be loaded as a heartbeat.
#[derive(Debug)]
pub struct HeartbeatError {
    pub file: PathBuf,
    pub reason: String,
}

impl fmt::Display for HeartbeatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.file.display(), self.reason)
    }
}

impl std::error::Error for HeartbeatError {}

// ── Duration parsing ──────────────────────────────────────────────────────────

/// Parse a duration string in the `<n>s` or `<n>m` form (e.g. `10s`, `45m`).
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('s') {
        let secs: u64 = n
            .trim()
            .parse()
            .map_err(|_| format!("invalid duration {s:?}: expected a whole number before 's'"))?;
        return Ok(Duration::from_secs(secs));
    }
    if let Some(n) = s.strip_suffix('m') {
        let mins: u64 = n
            .trim()
            .parse()
            .map_err(|_| format!("invalid duration {s:?}: expected a whole number before 'm'"))?;
        let secs = mins.checked_mul(60).ok_or_else(|| {
            format!("invalid duration {s:?}: value overflows when converted to seconds")
        })?;
        return Ok(Duration::from_secs(secs));
    }
    Err(format!(
        "invalid duration {s:?}: expected a suffix of 's' (seconds) or 'm' (minutes)"
    ))
}

// ── YAML deserialization ──────────────────────────────────────────────────────

/// Raw serde target; all fields optional so we can emit field-specific errors.
#[derive(Deserialize)]
struct HeartbeatRaw {
    label: Option<String>,
    timeout: Option<String>,
    interval: Option<String>,
    check: Option<String>,
    detail: Option<String>,
}

/// Parse a heartbeat from its YAML text. `path` is used only for error messages.
pub fn parse_heartbeat(text: &str, path: &Path) -> Result<Heartbeat, HeartbeatError> {
    let err = |reason: String| HeartbeatError {
        file: path.to_owned(),
        reason,
    };

    let raw: HeartbeatRaw =
        serde_yaml_ng::from_str(text).map_err(|e| err(format!("YAML parse error: {e}")))?;

    let label = raw
        .label
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err("missing required field `label`".into()))?;

    let timeout_str = raw
        .timeout
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err("missing required field `timeout`".into()))?;
    let timeout = parse_duration(&timeout_str).map_err(&err)?;

    let check = raw
        .check
        .filter(|s| !s.is_empty())
        .ok_or_else(|| err("missing required field `check`".into()))?;

    let interval = raw
        .interval
        .filter(|s| !s.is_empty())
        .map(|s| parse_duration(&s).map_err(err))
        .transpose()?
        .unwrap_or(DEFAULT_INTERVAL);

    let detail = raw.detail.filter(|s| !s.is_empty());

    Ok(Heartbeat {
        label,
        timeout,
        interval,
        check,
        detail,
    })
}

/// Load all `.yaml` and `.yml` files from `dir` as heartbeat definitions.
///
/// Returns `(heartbeats, errors)`. A file that fails to parse produces one
/// [`HeartbeatError`] and does not stop the remaining files from loading.
pub fn load_heartbeats(dir: &Path) -> (Vec<Heartbeat>, Vec<HeartbeatError>) {
    let mut ok = Vec::new();
    let mut errs = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            errs.push(HeartbeatError {
                file: dir.to_owned(),
                reason: format!("cannot read directory: {e}"),
            });
            return (ok, errs);
        }
    };

    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        match entry {
            Ok(e) => {
                let p = e.path();
                if matches!(
                    p.extension().and_then(|x| x.to_str()),
                    Some("yaml") | Some("yml")
                ) {
                    paths.push(p);
                }
            }
            Err(e) => errs.push(HeartbeatError {
                file: dir.to_owned(),
                reason: format!("cannot enumerate directory entry: {e}"),
            }),
        }
    }
    paths.sort();

    for path in paths {
        match std::fs::read_to_string(&path) {
            Err(e) => errs.push(HeartbeatError {
                file: path,
                reason: format!("cannot read file: {e}"),
            }),
            Ok(text) => match parse_heartbeat(&text, &path) {
                Ok(h) => ok.push(h),
                Err(e) => errs.push(e),
            },
        }
    }

    (ok, errs)
}

// ── Pure beat-state core ──────────────────────────────────────────────────────

/// Per-heartbeat state across check intervals.
///
/// Stored per-heartbeat in the runner; contains only what the pure transition
/// function needs to continue an unbroken beat run across ticks.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BeatState {
    /// Unix-epoch seconds of the first beat in the current unbroken run.
    /// `None` if the heartbeat has never beaten or the last check was a miss.
    pub beat_started: Option<u64>,
}

// ── Turn heartbeat ────────────────────────────────────────────────────────────

/// The in-flight turn heartbeat: beats for exactly as long as a turn is in
/// flight. Armed on `TurnAccepted`, silenced on `TurnEnded`. Joins the same
/// set as file heartbeats and uses the same [`apply_check`] machinery.
///
/// The timeout (60 minutes) names a turn that has been running unreasonably
/// long — a fault the operator should act on.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TurnBeat {
    state: BeatState,
    /// Whether a turn is currently in flight.
    pub armed: bool,
}

impl TurnBeat {
    /// Heartbeat interval — matches the file-heartbeat default.
    pub const INTERVAL: Duration = DEFAULT_INTERVAL;
    /// Timeout: a turn in flight longer than this is late.
    pub const TIMEOUT_SECS: u64 = 60 * 60;

    /// Arm the heartbeat when a turn is accepted. Records the arm time as
    /// `beat_started` so duration is measured from the turn's first activity.
    pub fn arm(&mut self, now_secs: u64) -> BeatStatus {
        self.armed = true;
        self.tick(now_secs)
    }

    /// Silence the heartbeat when the stop hook fires — the turn ended.
    pub fn silence(&mut self, now_secs: u64) -> BeatStatus {
        self.armed = false;
        self.tick(now_secs)
    }

    /// Tick on the heartbeat interval: emits `Beat` when armed, `Miss` when
    /// not. The first `Beat` after an `arm` sets `beat_started`; subsequent
    /// beats grow duration from that anchor; `silence` clears it.
    pub fn tick(&mut self, now_secs: u64) -> BeatStatus {
        let outcome = if self.armed {
            CheckOutcome::Beat { detail: None }
        } else {
            CheckOutcome::Miss
        };
        let (new_state, status) =
            apply_check(self.state.clone(), outcome, now_secs, Self::TIMEOUT_SECS);
        self.state = new_state;
        status
    }
}

/// The outcome of one check interval, produced by the runner and consumed by
/// [`apply_check`].
#[derive(Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The check exited zero; `detail` is the first trimmed line of the detail
    /// body's stdout, if a detail body ran and succeeded.
    Beat { detail: Option<String> },
    /// The check exited non-zero, timed out, or could not be spawned.
    Miss,
}

/// The status returned by [`apply_check`] after one tick. A later card uses
/// this to compute the aggregated work state.
#[derive(Debug, PartialEq, Eq)]
pub struct BeatStatus {
    /// Whether this heartbeat is currently beating.
    pub beating: bool,
    /// Seconds since the start of the current unbroken beat run; `0` when not
    /// beating (the beat-started clock is unset).
    pub duration_secs: u64,
    /// Whether `duration_secs ≥ timeout_secs`: this heartbeat has beaten past
    /// its own declared bound. A later card acts on this flag.
    pub is_late: bool,
    /// First line of the `detail` body's output, when available.
    pub narration: Option<String>,
}

/// Apply one check outcome to `state` and return `(new_state, status)`.
///
/// `now_secs` is the current Unix-epoch time in seconds, injected so the core
/// stays clock-free and unit-testable without timers. `timeout_secs` is the
/// heartbeat's `timeout` field converted to seconds.
///
/// - A [`CheckOutcome::Beat`] either starts a new run (`beat_started = now_secs`)
///   or continues the existing one, with duration growing from `beat_started`.
/// - A [`CheckOutcome::Miss`] clears `beat_started`, ending the run. The next
///   beat starts a fresh run with duration reset to zero.
pub fn apply_check(
    mut state: BeatState,
    outcome: CheckOutcome,
    now_secs: u64,
    timeout_secs: u64,
) -> (BeatState, BeatStatus) {
    match outcome {
        CheckOutcome::Beat { detail } => {
            let started = state.beat_started.unwrap_or(now_secs);
            let duration_secs = now_secs.saturating_sub(started);
            let is_late = duration_secs >= timeout_secs;
            state.beat_started = Some(started);
            (
                state,
                BeatStatus {
                    beating: true,
                    duration_secs,
                    is_late,
                    narration: detail,
                },
            )
        }
        CheckOutcome::Miss => {
            state.beat_started = None;
            (
                state,
                BeatStatus {
                    beating: false,
                    duration_secs: 0,
                    is_late: false,
                    narration: None,
                },
            )
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> &'static Path {
        Path::new("test.yaml")
    }

    fn parse(text: &str) -> Result<Heartbeat, HeartbeatError> {
        parse_heartbeat(text, path())
    }

    #[test]
    fn it_parses_a_complete_heartbeat_with_all_fields() {
        let yaml = "\
label: Compiling
timeout: 45m
interval: 10s
check: |
  pgrep -f 'rustc|cargo' >/dev/null
detail: |
  echo units
";
        let hb = parse(yaml).unwrap();
        assert_eq!(hb.label, "Compiling");
        assert_eq!(hb.timeout, Duration::from_secs(45 * 60));
        assert_eq!(hb.interval, Duration::from_secs(10));
        assert!(hb.check.contains("pgrep"));
        assert!(hb.detail.as_deref().unwrap().contains("echo"));
    }

    #[test]
    fn it_defaults_the_interval_to_ten_seconds() {
        let yaml = "label: Running\ntimeout: 5m\ncheck: true\n";
        let hb = parse(yaml).unwrap();
        assert_eq!(hb.interval, DEFAULT_INTERVAL);
    }

    #[test]
    fn it_parses_with_no_detail_and_shows_label_alone() {
        let yaml = "label: Running\ntimeout: 5m\ncheck: true\n";
        let hb = parse(yaml).unwrap();
        assert!(hb.detail.is_none());
        assert_eq!(hb.to_string(), "Running");
    }

    #[test]
    fn it_rejects_a_file_with_no_label() {
        let yaml = "timeout: 10s\ncheck: true\n";
        let err = parse(yaml).unwrap_err();
        assert!(err.reason.contains("label"), "{}", err.reason);
        assert_eq!(err.file, path());
    }

    #[test]
    fn it_rejects_a_file_with_no_timeout() {
        let yaml = "label: X\ncheck: true\n";
        let err = parse(yaml).unwrap_err();
        assert!(err.reason.contains("timeout"), "{}", err.reason);
        assert_eq!(err.file, path());
    }

    #[test]
    fn it_rejects_a_file_with_no_check() {
        let yaml = "label: X\ntimeout: 10s\n";
        let err = parse(yaml).unwrap_err();
        assert!(err.reason.contains("check"), "{}", err.reason);
        assert_eq!(err.file, path());
    }

    #[test]
    fn it_rejects_invalid_yaml_with_a_named_error() {
        let yaml = "label: [\nnot closed";
        let err = parse(yaml).unwrap_err();
        assert!(err.reason.contains("YAML"), "{}", err.reason);
        assert_eq!(err.file, path());
    }

    #[test]
    fn it_parses_duration_in_seconds() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn it_parses_duration_in_minutes() {
        assert_eq!(parse_duration("45m").unwrap(), Duration::from_secs(45 * 60));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn it_rejects_a_duration_with_an_unknown_suffix() {
        assert!(parse_duration("10h").is_err());
        assert!(parse_duration("10").is_err());
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn it_rejects_an_empty_or_whitespace_only_duration() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("   ").is_err());
    }

    #[test]
    fn it_rejects_a_duration_that_overflows_on_conversion_to_seconds() {
        // 307_445_734_561_825_861 * 60 overflows u64; must error, not panic.
        assert!(parse_duration("307445734561825861m").is_err());
    }

    #[test]
    fn it_names_the_file_in_every_error() {
        let p = Path::new("some/dir/mine.yaml");
        let err = parse_heartbeat("not: yaml: here: [", p).unwrap_err();
        assert_eq!(err.file, p);
    }

    #[test]
    fn it_reads_block_scalar_check_verbatim() {
        let yaml = "\
label: Test
timeout: 5m
check: |
  pgrep -f 'rustc|cargo' >/dev/null
";
        let hb = parse(yaml).unwrap();
        assert_eq!(hb.check, "pgrep -f 'rustc|cargo' >/dev/null\n");
    }

    #[test]
    fn it_displays_label_and_trimmed_detail_when_detail_is_present() {
        let yaml = "\
label: Building
timeout: 30m
check: true
detail: |
  echo 42 units
";
        let hb = parse(yaml).unwrap();
        let disp = hb.to_string();
        assert!(disp.starts_with("Building"), "{disp}");
        assert!(disp.contains("echo 42 units"), "{disp}");
    }

    // ── load_heartbeats tests ─────────────────────────────────────────────────

    /// RAII temp directory backed by std only (no external dep).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p =
                std::env::temp_dir().join(format!("katsuobushi-hb-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn it_loads_valid_files_and_collects_errors_for_invalid_ones() {
        let dir = TempDir::new("partial");
        std::fs::write(
            dir.path().join("good.yaml"),
            "label: Running\ntimeout: 10s\ncheck: true\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("bad.yaml"), "label: [\nnot closed").unwrap();

        let (heartbeats, errors) = load_heartbeats(dir.path());
        assert_eq!(heartbeats.len(), 1, "expected one valid heartbeat");
        assert_eq!(errors.len(), 1, "expected one error for the bad file");
        assert_eq!(heartbeats[0].label, "Running");
        assert!(errors[0].file.ends_with("bad.yaml"), "{:?}", errors[0].file);
    }

    #[test]
    fn it_returns_an_error_when_the_directory_does_not_exist() {
        let (heartbeats, errors) = load_heartbeats(Path::new("/nonexistent/path/for/test"));
        assert!(heartbeats.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].reason.contains("cannot read directory"),
            "{}",
            errors[0].reason
        );
    }

    #[test]
    fn it_ignores_non_yaml_files_in_the_heartbeat_directory() {
        let dir = TempDir::new("filter");
        std::fs::write(dir.path().join("config.json"), r#"{"key": "val"}"#).unwrap();
        std::fs::write(
            dir.path().join("hb.yaml"),
            "label: OK\ntimeout: 5s\ncheck: true\n",
        )
        .unwrap();

        let (heartbeats, errors) = load_heartbeats(dir.path());
        assert_eq!(heartbeats.len(), 1);
        assert!(errors.is_empty());
    }
}

// ── TurnBeat tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod turn_beat_tests {
    use super::*;

    const T: u64 = 1_000_000;

    #[test]
    fn it_beats_while_a_turn_is_in_flight() {
        let mut beat = TurnBeat::default();
        let status = beat.arm(T);
        assert!(status.beating, "should beat immediately on arm");
        let status = beat.tick(T + 10);
        assert!(status.beating, "should keep beating while armed");
    }

    #[test]
    fn it_goes_late_after_sixty_minutes() {
        let mut beat = TurnBeat::default();
        beat.arm(T);
        // One second before the boundary: not late yet.
        let status = beat.tick(T + TurnBeat::TIMEOUT_SECS - 1);
        assert!(!status.is_late, "one second before the timeout: not late");
        // At the 60-minute boundary: late.
        let status = beat.tick(T + TurnBeat::TIMEOUT_SECS);
        assert!(status.is_late, "at 60 minutes: late flag raised");
    }

    #[test]
    fn it_stops_beating_when_a_turn_ends() {
        let mut beat = TurnBeat::default();
        beat.arm(T);
        let status = beat.silence(T + 30);
        assert!(!status.beating, "should stop beating on silence");
    }

    #[test]
    fn it_is_idle_before_any_turn_starts() {
        let mut beat = TurnBeat::default();
        let status = beat.tick(T);
        assert!(!status.beating, "no beat before a turn is armed");
        assert!(!status.is_late);
    }

    #[test]
    fn it_resets_duration_when_rearmed_after_silence() {
        let mut beat = TurnBeat::default();
        beat.arm(T);
        beat.silence(T + 100);
        // A new turn arms fresh; duration starts at zero again.
        let status = beat.arm(T + 200);
        assert_eq!(status.duration_secs, 0, "rearm resets the turn clock");
    }
}

// ── Pure beat-state core tests ─────────────────────────────────────────────────

#[cfg(test)]
mod beat_tests {
    use super::*;

    const NOW: u64 = 1_000_000;
    const TIMEOUT: u64 = 300;

    fn beat(detail: Option<&str>) -> CheckOutcome {
        CheckOutcome::Beat {
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn it_beats_on_exit_zero() {
        let (_, status) = apply_check(BeatState::default(), beat(None), NOW, TIMEOUT);
        assert!(status.beating);
    }

    #[test]
    fn it_misses_on_nonzero_exit() {
        let (_, status) = apply_check(BeatState::default(), CheckOutcome::Miss, NOW, TIMEOUT);
        assert!(!status.beating);
    }

    #[test]
    fn it_starts_the_duration_at_zero_on_the_first_beat() {
        let (_, status) = apply_check(BeatState::default(), beat(None), NOW, TIMEOUT);
        assert_eq!(status.duration_secs, 0, "first beat: duration starts at 0");
    }

    #[test]
    fn it_counts_duration_from_the_first_beat_of_an_unbroken_run() {
        let (state, _) = apply_check(BeatState::default(), beat(None), NOW, TIMEOUT);
        let (_, status) = apply_check(state, beat(None), NOW + 30, TIMEOUT);
        assert_eq!(status.duration_secs, 30);
    }

    #[test]
    fn it_resets_the_duration_after_a_failed_check() {
        let (state, _) = apply_check(BeatState::default(), beat(None), NOW, TIMEOUT);
        let (state, _) = apply_check(state, CheckOutcome::Miss, NOW + 10, TIMEOUT);
        let (_, status) = apply_check(state, beat(None), NOW + 20, TIMEOUT);
        assert!(status.beating);
        assert_eq!(status.duration_secs, 0, "fresh run: duration resets to 0");
    }

    #[test]
    fn it_flags_a_heartbeat_past_its_own_timeout() {
        let (state, _) = apply_check(BeatState::default(), beat(None), NOW, TIMEOUT);
        let (_, status) = apply_check(state, beat(None), NOW + TIMEOUT, TIMEOUT);
        assert!(status.is_late, "at the timeout boundary, is_late is set");
    }

    #[test]
    fn it_is_not_late_below_the_timeout() {
        let (state, _) = apply_check(BeatState::default(), beat(None), NOW, TIMEOUT);
        let (_, status) = apply_check(state, beat(None), NOW + TIMEOUT - 1, TIMEOUT);
        assert!(!status.is_late, "one second before the timeout: not late");
    }

    #[test]
    fn it_carries_no_late_flag_on_a_miss() {
        let (_, status) = apply_check(
            BeatState::default(),
            CheckOutcome::Miss,
            NOW + 9999,
            TIMEOUT,
        );
        assert!(!status.is_late, "a miss is never late");
    }

    #[test]
    fn it_attaches_detail_narration_to_a_beat() {
        let (_, status) = apply_check(BeatState::default(), beat(Some("42 units")), NOW, TIMEOUT);
        assert_eq!(status.narration.as_deref(), Some("42 units"));
    }

    #[test]
    fn it_carries_no_narration_on_a_miss() {
        let (_, status) = apply_check(BeatState::default(), CheckOutcome::Miss, NOW, TIMEOUT);
        assert!(status.narration.is_none());
    }

    #[test]
    fn it_carries_no_narration_when_the_detail_body_did_not_run() {
        let (_, status) = apply_check(BeatState::default(), beat(None), NOW, TIMEOUT);
        assert!(status.narration.is_none());
    }
}
