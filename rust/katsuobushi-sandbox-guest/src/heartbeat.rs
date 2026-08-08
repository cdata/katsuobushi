//! Heartbeat definitions: parse `.katsuobushi/heartbeats/*.yaml` files into
//! typed [`Heartbeat`] values.
//!
//! Parsing is separated from I/O so the parser is testable with inline strings;
//! [`load_heartbeats`] handles the filesystem walk and collects both successes
//! and errors without aborting on the first failure.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// Transport cadence default (10 s) — also the default for an absent `interval`.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

/// A successfully parsed heartbeat definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    /// File this heartbeat was loaded from; used to track re-reads.
    pub source: PathBuf,
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
        source: path.to_owned(),
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

// ── Heartbeat-set re-read state ───────────────────────────────────────────────

/// Tracks which heartbeat files have been loaded or are known broken across
/// periodic re-reads of the heartbeat directories.
///
/// The runner re-reads the directories on a slow timer. On each read it calls
/// [`eval_scan`] to compute what changed: newly fixed or added files produce
/// new tasks; error messages are suppressed for files that are still broken
/// (once per broken path, not once per error message); deleted or broken files
/// that were running have their tasks aborted.
#[derive(Debug, Default)]
pub struct HeartbeatSetState {
    /// Paths for which a running task has already been spawned.
    loaded: HashSet<PathBuf>,
    /// Paths that produced a parse error (error already reported once).
    broken: HashSet<PathBuf>,
}

/// What changed on one re-read scan, as computed by [`eval_scan`].
pub struct HeartbeatSetDiff {
    /// Heartbeats that should have tasks spawned (newly added or fixed files).
    pub to_start: Vec<Heartbeat>,
    /// Errors that have not been reported before and should be printed once.
    pub new_errors: Vec<HeartbeatError>,
    /// Paths whose tasks should be aborted (file deleted or became unparseable).
    pub to_stop: Vec<PathBuf>,
}

/// Apply one scan result to `state` and return what changed.
///
/// Rules:
/// - A path that was previously loaded but is no longer valid (deleted or
///   unparseable) is placed in `to_stop` and removed from `state.loaded`, so
///   the caller can abort the old task and so that a recreated file starts a
///   fresh task under the normal add path.
/// - A heartbeat file that was not previously loaded gets a task spawned.
///   If it was broken before, its broken state is cleared.
/// - A file that is still loaded (task already running) is skipped.
/// - A parse error on a file not yet in the broken set is reported once
///   (once per broken path) and added to the set. A file already in the
///   broken set stays silent, even if its error message has changed.
pub fn eval_scan(
    state: &mut HeartbeatSetState,
    heartbeats: Vec<Heartbeat>,
    errors: Vec<HeartbeatError>,
) -> HeartbeatSetDiff {
    let mut to_start = Vec::new();
    let mut new_errors = Vec::new();
    let mut to_stop = Vec::new();

    // Paths that are currently valid (successfully parsed).
    let current_valid: HashSet<PathBuf> = heartbeats.iter().map(|hb| hb.source.clone()).collect();

    // A path that was running but is no longer valid (deleted or broken) must
    // have its task aborted. Remove it from loaded so a recreated file starts
    // fresh under the normal add path.
    state.loaded.retain(|path| {
        if current_valid.contains(path) {
            true
        } else {
            to_stop.push(path.clone());
            false
        }
    });

    for hb in heartbeats {
        let path = hb.source.clone();
        if !state.loaded.contains(&path) {
            state.broken.remove(&path);
            state.loaded.insert(path);
            to_start.push(hb);
        }
    }

    for err in errors {
        if !state.broken.contains(&err.file) {
            state.broken.insert(err.file.clone());
            new_errors.push(err);
        }
    }

    HeartbeatSetDiff {
        to_start,
        new_errors,
        to_stop,
    }
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
    armed: bool,
}

impl TurnBeat {
    /// Heartbeat interval — matches the file-heartbeat default.
    pub const INTERVAL: Duration = DEFAULT_INTERVAL;
    /// Timeout: a turn in flight longer than this is late.
    pub const TIMEOUT_SECS: u64 = 60 * 60;

    /// Arm the heartbeat when the runner's poll detects the turn-armed flag
    /// flipping `true`. Sets `beat_started` to `now_secs` so duration is
    /// measured from the first interval that observed the arm — up to one
    /// interval (10 s) after `TurnAccepted` fires.
    pub fn arm(&mut self, now_secs: u64) -> BeatStatus {
        self.armed = true;
        self.tick(now_secs)
    }

    /// Silence the heartbeat when the runner's poll detects the turn-armed
    /// flag flipping `false` (set by the stop hook). Clears `beat_started`
    /// and emits a `Miss`.
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

// ── Work-state combiner ───────────────────────────────────────────────────────

/// The work state derived by combining all heartbeat statuses with the turn's
/// terminal-report flag.
///
/// Precedence (high → low):
/// - `Finished` — the agent ran a terminal report; outranks every beat observation.
/// - `Active` — one or more heartbeats are beating; `is_late` is raised when any
///   beating heartbeat has exceeded its declared timeout.
/// - `Idle` — nothing beats and no terminal report was filed.
///
/// `late` is not a separate state; it is a flag on `Active`. A heartbeat past
/// its timeout is by definition still beating; one that stops is absent, not late.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    /// The agent filed a terminal report (`done` / `blocked`) for this turn.
    Finished,
    /// At least one heartbeat is beating.
    Active {
        /// `true` when any of the currently-beating heartbeats has exceeded its
        /// own declared timeout (duration ≥ timeout).
        is_late: bool,
    },
    /// No heartbeat beats and no terminal report was filed.
    Idle,
}

/// Combine a slice of [`BeatStatus`] values into one [`WorkState`].
///
/// `finished` is `true` when the current turn has received a terminal report
/// (`done` / `blocked`). `beats` holds the most-recent status from every active
/// heartbeat (file heartbeats and the turn heartbeat).
///
/// The rule is a logical OR over the beat set, read in precedence order:
/// `finished` outranks every beat observation; any beating entry wins over all
/// misses; a late entry among beating ones raises the flag.
pub fn combine_work_state(finished: bool, beats: &[BeatStatus]) -> WorkState {
    if finished {
        return WorkState::Finished;
    }
    let any_beating = beats.iter().any(|b| b.beating);
    if any_beating {
        let is_late = beats.iter().any(|b| b.beating && b.is_late);
        WorkState::Active { is_late }
    } else {
        WorkState::Idle
    }
}

// ── Staleness bound ───────────────────────────────────────────────────────────

/// Multiplier applied to a heartbeat's own interval to derive the staleness
/// bound. Three intervals clears the worst-case late tick (a slow check fills
/// the full budget, so consecutive ticks can be up to two intervals apart) while
/// still catching a dead sender well within the first user-visible status check.
pub const STALE_BEAT_MULTIPLIER: u64 = 3;

/// Returns the staleness bound in seconds for a heartbeat whose polling interval
/// is `interval_secs`.
///
/// An entry not refreshed within this many seconds is treated as absent by the
/// work-state coordinator — its `beating` flag is no longer counted toward
/// [`WorkState::Active`].
pub fn staleness_bound_secs(interval_secs: u64) -> u64 {
    STALE_BEAT_MULTIPLIER.saturating_mul(interval_secs)
}

/// Returns `true` when a beat entry is stale at `now_secs`.
///
/// Staleness is declared when the silence since `received_at_secs` exceeds
/// [`staleness_bound_secs`] for `interval_secs`. The bound uses strict `>`
/// so an entry refreshed exactly at the boundary edge is still live.
pub fn is_stale(interval_secs: u64, received_at_secs: u64, now_secs: u64) -> bool {
    now_secs.saturating_sub(received_at_secs) > staleness_bound_secs(interval_secs)
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

// ── Work-state combiner tests ─────────────────────────────────────────────────

#[cfg(test)]
mod work_state_tests {
    use super::*;

    fn beating(is_late: bool) -> BeatStatus {
        BeatStatus {
            beating: true,
            duration_secs: if is_late { 3_600 } else { 10 },
            is_late,
            narration: None,
        }
    }

    fn miss() -> BeatStatus {
        BeatStatus {
            beating: false,
            duration_secs: 0,
            is_late: false,
            narration: None,
        }
    }

    #[test]
    fn it_prefers_finished_over_a_beating_heartbeat() {
        assert_eq!(
            combine_work_state(true, &[beating(false)]),
            WorkState::Finished
        );
    }

    #[test]
    fn it_prefers_finished_over_a_late_beating_heartbeat() {
        assert_eq!(
            combine_work_state(true, &[beating(true)]),
            WorkState::Finished
        );
    }

    #[test]
    fn it_prefers_finished_with_no_beats_at_all() {
        assert_eq!(combine_work_state(true, &[]), WorkState::Finished);
    }

    #[test]
    fn it_is_active_when_a_heartbeat_beats() {
        assert_eq!(
            combine_work_state(false, &[beating(false)]),
            WorkState::Active { is_late: false }
        );
    }

    #[test]
    fn it_flags_late_without_leaving_active() {
        // `late` is a flag on `Active`, not a separate fourth state.
        let state = combine_work_state(false, &[beating(true)]);
        assert!(
            matches!(state, WorkState::Active { is_late: true }),
            "expected Active {{ is_late: true }}, got {state:?}"
        );
    }

    #[test]
    fn it_flags_late_when_any_beating_heartbeat_exceeds_its_timeout() {
        // One late + one healthy = active, late flag raised.
        let state = combine_work_state(false, &[beating(false), beating(true)]);
        assert_eq!(state, WorkState::Active { is_late: true });
    }

    #[test]
    fn it_does_not_raise_the_late_flag_for_a_healthy_beating_heartbeat() {
        assert_eq!(
            combine_work_state(false, &[beating(false)]),
            WorkState::Active { is_late: false }
        );
    }

    #[test]
    fn it_is_active_when_at_least_one_heartbeat_beats_among_misses() {
        // OR semantics: one beating entry is enough.
        let state = combine_work_state(false, &[miss(), beating(false), miss()]);
        assert_eq!(state, WorkState::Active { is_late: false });
    }

    #[test]
    fn it_falls_back_to_idle() {
        assert_eq!(combine_work_state(false, &[]), WorkState::Idle);
    }

    #[test]
    fn it_falls_back_to_idle_when_all_heartbeats_miss() {
        assert_eq!(
            combine_work_state(false, &[miss(), miss()]),
            WorkState::Idle
        );
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

// ── HeartbeatSetState / eval_scan tests ───────────────────────────────────────

#[cfg(test)]
mod heartbeat_set_tests {
    use super::*;

    fn good(path: &str) -> Heartbeat {
        Heartbeat {
            source: PathBuf::from(path),
            label: format!("hb-{path}"),
            timeout: Duration::from_secs(300),
            interval: DEFAULT_INTERVAL,
            check: "true".into(),
            detail: None,
        }
    }

    fn broken(path: &str) -> HeartbeatError {
        HeartbeatError {
            file: PathBuf::from(path),
            reason: "parse error".into(),
        }
    }

    #[test]
    fn it_starts_a_newly_added_valid_heartbeat() {
        let mut state = HeartbeatSetState::default();
        let diff = eval_scan(&mut state, vec![good("a.yaml")], vec![]);
        assert_eq!(diff.to_start.len(), 1);
        assert!(diff.new_errors.is_empty());
    }

    #[test]
    fn it_reports_a_broken_file_exactly_once() {
        let mut state = HeartbeatSetState::default();
        let diff1 = eval_scan(&mut state, vec![], vec![broken("bad.yaml")]);
        assert_eq!(diff1.new_errors.len(), 1, "first scan: error reported");
        let diff2 = eval_scan(&mut state, vec![], vec![broken("bad.yaml")]);
        assert!(diff2.new_errors.is_empty(), "second scan: error suppressed");
        let diff3 = eval_scan(&mut state, vec![], vec![broken("bad.yaml")]);
        assert!(
            diff3.new_errors.is_empty(),
            "third scan: error still suppressed"
        );
    }

    #[test]
    fn it_stops_a_loaded_heartbeat_when_its_file_is_deleted() {
        let mut state = HeartbeatSetState::default();

        // First scan: file appears and is loaded, task spawned.
        let diff = eval_scan(&mut state, vec![good("vanish.yaml")], vec![]);
        assert_eq!(diff.to_start.len(), 1);
        assert!(diff.to_stop.is_empty());

        // Second scan: file is gone (deleted). Task must be stopped and path
        // removed from loaded so a recreated file starts fresh.
        let diff = eval_scan(&mut state, vec![], vec![]);
        assert!(
            diff.to_start.is_empty(),
            "deleted file must not be re-started"
        );
        assert_eq!(
            diff.to_stop,
            vec![PathBuf::from("vanish.yaml")],
            "deleted file path must appear in to_stop"
        );
        assert!(
            !state.loaded.contains(Path::new("vanish.yaml")),
            "path must be removed from loaded after deletion"
        );

        // Third scan: file is still absent; must not re-appear in to_stop.
        let diff = eval_scan(&mut state, vec![], vec![]);
        assert!(
            diff.to_stop.is_empty(),
            "already-removed path must not re-appear"
        );

        // Fourth scan: file is recreated; must start a fresh task.
        let diff = eval_scan(&mut state, vec![good("vanish.yaml")], vec![]);
        assert_eq!(
            diff.to_start.len(),
            1,
            "recreated file must start a fresh task"
        );
        assert!(diff.to_stop.is_empty());
    }

    #[test]
    fn it_starts_a_heartbeat_when_a_broken_file_is_corrected() {
        let mut state = HeartbeatSetState::default();
        eval_scan(&mut state, vec![], vec![broken("fixed.yaml")]);
        let diff = eval_scan(&mut state, vec![good("fixed.yaml")], vec![]);
        assert_eq!(diff.to_start.len(), 1, "corrected file must start a task");
        assert!(diff.new_errors.is_empty());
    }

    #[test]
    fn it_does_not_respawn_an_already_running_heartbeat() {
        let mut state = HeartbeatSetState::default();
        eval_scan(&mut state, vec![good("ok.yaml")], vec![]);
        let diff = eval_scan(&mut state, vec![good("ok.yaml")], vec![]);
        assert!(
            diff.to_start.is_empty(),
            "already-running file must not be re-spawned"
        );
    }

    #[test]
    fn it_picks_up_a_newly_added_heartbeat_file_on_a_later_scan() {
        let mut state = HeartbeatSetState::default();
        eval_scan(&mut state, vec![], vec![]);
        let diff = eval_scan(&mut state, vec![good("new.yaml")], vec![]);
        assert_eq!(diff.to_start.len(), 1, "new file must be picked up");
    }

    #[test]
    fn it_clears_the_broken_state_when_a_file_is_corrected() {
        let mut state = HeartbeatSetState::default();
        eval_scan(&mut state, vec![], vec![broken("f.yaml")]);
        assert!(state.broken.contains(Path::new("f.yaml")));
        eval_scan(&mut state, vec![good("f.yaml")], vec![]);
        assert!(
            !state.broken.contains(Path::new("f.yaml")),
            "broken state must be cleared"
        );
    }

    #[test]
    fn it_handles_mixed_good_and_broken_files_independently() {
        let mut state = HeartbeatSetState::default();
        let diff = eval_scan(&mut state, vec![good("ok.yaml")], vec![broken("bad.yaml")]);
        assert_eq!(diff.to_start.len(), 1);
        assert_eq!(diff.new_errors.len(), 1);
    }
}

// ── Staleness bound tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod staleness_tests {
    use super::*;

    const INTERVAL: u64 = 10; // seconds — the default heartbeat cadence

    #[test]
    fn it_is_not_stale_immediately_after_a_beat_arrives() {
        // received_at == now_secs: zero silence; must never be stale.
        let now = 1_000_000u64;
        assert!(!is_stale(INTERVAL, now, now));
    }

    #[test]
    fn it_is_not_stale_within_the_bound() {
        // Silence of exactly `staleness_bound_secs` is still live (strict >).
        let now = 1_000_000u64;
        let received_at = now - staleness_bound_secs(INTERVAL);
        assert!(
            !is_stale(INTERVAL, received_at, now),
            "silence == bound is still live"
        );
    }

    #[test]
    fn it_becomes_stale_when_silence_exceeds_the_bound() {
        let now = 1_000_000u64;
        let received_at = now - staleness_bound_secs(INTERVAL) - 1;
        assert!(
            is_stale(INTERVAL, received_at, now),
            "silence > bound must be stale"
        );
    }

    #[test]
    fn it_derives_the_bound_from_the_heartbeats_own_interval_not_a_global() {
        // A slow heartbeat (60 s interval) has a proportionally wider bound than
        // a fast one (10 s), so a slow-but-alive sender is never falsely evicted.
        let slow_interval = 60u64;
        let fast_interval = 10u64;
        assert!(
            staleness_bound_secs(slow_interval) > staleness_bound_secs(fast_interval),
            "a slower heartbeat gets a wider staleness window"
        );
        // Concretely: 30 s of silence is stale for the 10 s heartbeat but not
        // for the 60 s one.
        let now = 1_000_000u64;
        let received_at = now - 31; // 31 s ago
        assert!(
            is_stale(fast_interval, received_at, now),
            "31 s is stale for 10 s interval"
        );
        assert!(
            !is_stale(slow_interval, received_at, now),
            "31 s is not stale for 60 s interval"
        );
    }

    #[test]
    fn it_never_treats_a_slow_but_alive_heartbeat_as_stale() {
        // Worst-case late tick: the check fills the full interval budget, so
        // two consecutive ticks are up to two intervals apart. With multiplier 3
        // the entry must still be live at `2 * interval - 1` seconds of silence.
        let now = 1_000_000u64;
        let worst_case_gap = 2 * INTERVAL - 1;
        let received_at = now - worst_case_gap;
        assert!(
            !is_stale(INTERVAL, received_at, now),
            "worst-case late tick must not be treated as stale"
        );
    }

    #[test]
    fn it_handles_clock_skew_without_panicking() {
        // received_at > now_secs: saturating_sub prevents underflow; the entry
        // looks zero-seconds-old, which is never stale.
        assert!(!is_stale(INTERVAL, 1_000_100, 1_000_000));
    }
}
