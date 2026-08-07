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
        return Ok(Duration::from_secs(mins * 60));
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
        serde_yaml::from_str(text).map_err(|e| err(format!("YAML parse error: {e}")))?;

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

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("yaml") | Some("yml")
            )
        })
        .collect();
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
    }

    #[test]
    fn it_rejects_a_file_with_no_check() {
        let yaml = "label: X\ntimeout: 10s\n";
        let err = parse(yaml).unwrap_err();
        assert!(err.reason.contains("check"), "{}", err.reason);
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
  pgrep -f rustc >/dev/null
  echo done
";
        let hb = parse(yaml).unwrap();
        assert!(hb.check.contains("pgrep -f rustc"));
        assert!(hb.check.contains("echo done"));
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
}
