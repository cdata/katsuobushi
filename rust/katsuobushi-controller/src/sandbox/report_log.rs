//! The per-instance report journal — `reports.ndjson` in an instance's state
//! dir.
//!
//! An agent's reports used to exist only as a transient stream: the guest
//! relays each one to the host, and the driving `prompt`/`dispatch` process
//! renders it to its own stdout. Nothing wrote the *text* down. Host-side
//! persistence is `liveness.json` (heartbeat freshness and the turn-id counter);
//! guest-side is `turn-state.json` (phase and timestamps). Both record that a
//! turn reached `ended-ok` — neither records what it said.
//!
//! So a reviewer's `report done "VERDICT: …"` could land cleanly and still be
//! unrecoverable the moment its terminal was gone: in the field an operator
//! grepped the whole instance state dir for "VERDICT", found nothing, and had to
//! re-prompt the reviewer to restate its own conclusion.
//!
//! This journals every relayed report — plus the `Stopped`/`ReArmed` lifecycle
//! verdicts, which are equally part of "what happened" — as newline-delimited
//! JSON, appended at the single sink every streamed event already passes
//! through. Semantics match the rest of the state dir: **best-effort**. A failed
//! append warns once and never breaks the drive.
//!
//! Growth is a non-issue — reports are rare (a handful per turn) and the file
//! lives in the per-instance state dir, reaped with everything else.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Filename within the per-instance state dir.
const REPORTS_FILE: &str = "reports.ndjson";

/// `<state_dir>/<name>/reports.ndjson`.
pub fn path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(name).join(REPORTS_FILE)
}

/// One journaled line. `status` is the report's own status
/// (`working`/`info`/`done`/`blocked`) or a lifecycle verdict
/// (`stopped`/`re-armed`), so a reader can tell "the agent concluded X" from
/// "the agent stopped without concluding" without a second file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Host-side RFC-3339 timestamp. Host-side deliberately: the point is to
    /// reconstruct what the *operator* would have seen and when.
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    pub status: String,
    pub text: String,
}

impl Entry {
    /// Whether this entry is a turn's terminal outcome — the thing an operator
    /// asking "what did it conclude?" wants. `working`/`info` are progress.
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "done" | "blocked" | "stopped")
    }
}

/// Append one entry. Creates the file (and its directory) on first write.
pub fn append(state_dir: &Path, name: &str, entry: &Entry) -> Result<()> {
    let p = path(state_dir, name);
    let dir = p.parent().expect("report path always has a parent");
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating instance state dir {}", dir.display()))?;
    let mut line = serde_json::to_string(entry).context("serializing a report journal entry")?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .with_context(|| format!("opening {}", p.display()))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("appending to {}", p.display()))?;
    Ok(())
}

/// Parse journal bytes, skipping unparseable lines.
///
/// A malformed line is diagnostic, never fatal — the same rule the guest-message
/// decoder follows. A partially-written trailing line (the process died
/// mid-append) must not make the rest of the history unreadable.
///
/// Takes bytes rather than a path so a caller that already reads through the
/// [`Host`](crate::sandbox::host::Host) seam — `sandbox status` does — can parse
/// without a second, unseamed filesystem hit.
pub fn parse(bytes: &[u8]) -> Vec<Entry> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .collect()
}

/// The most recent terminal entry in `bytes` — what `sandbox status` surfaces as
/// the instance's last conclusion.
pub fn latest_terminal(bytes: &[u8]) -> Option<Entry> {
    parse(bytes).into_iter().rfind(Entry::is_terminal)
}

/// Read and parse the journal straight off disk.
///
/// Test-only on purpose: every production reader goes through the
/// [`Host`](crate::sandbox::host::Host) seam and parses with [`parse`], so an
/// unseamed path-based reader would be a way to accidentally reintroduce a
/// direct filesystem hit inside `summarize`. Tests that write real files want
/// it, and nothing else should.
#[cfg(test)]
pub fn read(state_dir: &Path, name: &str) -> Vec<Entry> {
    std::fs::read(path(state_dir, name))
        .map(|b| parse(&b))
        .unwrap_or_default()
}

/// [`latest_terminal`] straight off disk — the test-only companion to [`read`].
#[cfg(test)]
pub fn latest_terminal_at(state_dir: &Path, name: &str) -> Option<Entry> {
    read(state_dir, name).into_iter().rfind(Entry::is_terminal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "katsu-reportlog-{tag}-{}-{}",
            std::process::id(),
            tag.len()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn entry(at: &str, turn: u64, status: &str, text: &str) -> Entry {
        Entry {
            at: at.to_string(),
            turn_id: Some(turn),
            status: status.to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn appended_entries_accumulate_as_one_json_object_per_line() {
        let root = tmp_root("append");
        append(
            &root,
            "inst",
            &entry("2026-08-01T00:00:00Z", 1, "working", "building"),
        )
        .unwrap();
        append(
            &root,
            "inst",
            &entry("2026-08-01T00:05:00Z", 1, "done", "VERDICT: accept"),
        )
        .unwrap();

        let raw = std::fs::read_to_string(path(&root, "inst")).unwrap();
        assert_eq!(raw.lines().count(), 2, "{raw}");
        assert!(raw.ends_with('\n'), "each line is terminated: {raw:?}");

        let got = read(&root, "inst");
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].text, "VERDICT: accept");
        assert_eq!(got[1].turn_id, Some(1));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_multiline_verdict_survives_the_round_trip() {
        // The exact shape that was lost in the field: a long, multi-line
        // terminal report with quotes and newlines. NDJSON must not truncate it
        // at the first newline.
        let root = tmp_root("multiline");
        let text = "VERDICT: accept\n\nStrongest finding: `note.rs:81` — the \"scalar\" reader.\nWould block: no.";
        append(
            &root,
            "inst",
            &entry("2026-08-01T00:05:00Z", 2, "done", text),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(path(&root, "inst"))
                .unwrap()
                .lines()
                .count(),
            1,
            "a multi-line report is still ONE journal line"
        );
        assert_eq!(read(&root, "inst")[0].text, text);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn latest_terminal_ignores_progress_reports() {
        let root = tmp_root("terminal");
        for e in [
            entry("2026-08-01T00:00:00Z", 1, "working", "a"),
            entry("2026-08-01T00:01:00Z", 1, "done", "first conclusion"),
            entry("2026-08-01T00:02:00Z", 2, "info", "b"),
            entry("2026-08-01T00:03:00Z", 2, "blocked", "needs a token"),
            entry("2026-08-01T00:04:00Z", 2, "working", "c"),
        ] {
            append(&root, "inst", &e).unwrap();
        }
        let last = latest_terminal_at(&root, "inst").unwrap();
        assert_eq!(last.status, "blocked");
        assert_eq!(last.text, "needs a token");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_truncated_trailing_line_does_not_hide_earlier_history() {
        // A drive killed mid-append leaves a partial line; everything before it
        // must still read.
        let root = tmp_root("torn");
        append(
            &root,
            "inst",
            &entry("2026-08-01T00:00:00Z", 1, "done", "survived"),
        )
        .unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(path(&root, "inst"))
            .unwrap();
        f.write_all(b"{\"at\":\"2026-08-01T00:01:00Z\",\"stat")
            .unwrap();
        drop(f);

        let got = read(&root, "inst");
        assert_eq!(got.len(), 1, "the torn line is skipped, not fatal");
        assert_eq!(got[0].text, "survived");
        assert_eq!(latest_terminal_at(&root, "inst").unwrap().text, "survived");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_absent_journal_reads_as_empty_rather_than_failing() {
        let root = tmp_root("absent");
        assert!(read(&root, "nope").is_empty());
        assert!(latest_terminal_at(&root, "nope").is_none());
    }
}
