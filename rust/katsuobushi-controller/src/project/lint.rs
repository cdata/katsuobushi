//! `project lint` — board <-> note consistency, the price of two
//! independently-editable stores. `--fix` prunes the safe cases.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::output::{Renderer, Reported};

use super::board::{Board, Location};
use super::fs::Fs;
use super::layout::{self, Paths};
use super::model::{CardId, Status};

#[derive(Serialize)]
struct Issue {
    severity: &'static str,
    code: &'static str,
    message: String,
}

fn error(code: &'static str, message: String) -> Issue {
    Issue {
        severity: "error",
        code,
        message,
    }
}

/// Classify one board card: malformed link, or count it toward duplicate/orphan
/// detection. A free function (not a closure) so the mutable accumulators stay
/// freely usable after the scan.
#[allow(clippy::too_many_arguments)]
fn check_board_card(
    id: Option<CardId>,
    where_: &str,
    raw: &str,
    note_ids: &HashSet<CardId>,
    issues: &mut Vec<Issue>,
    seen: &mut HashMap<CardId, u32>,
    orphan_ids: &mut Vec<CardId>,
) {
    match id {
        None => issues.push(error(
            "malformed-card",
            format!("card in {where_} has no resolvable [[id-slug]] link: {raw}"),
        )),
        Some(id) => {
            *seen.entry(id.clone()).or_default() += 1;
            if !note_ids.contains(&id) {
                issues.push(error(
                    "orphan-card",
                    format!("card {id} in {where_} has no note file"),
                ));
                orphan_ids.push(id);
            }
        }
    }
}
fn warn(code: &'static str, message: String) -> Issue {
    Issue {
        severity: "warn",
        code,
        message,
    }
}

/// An informational line: never fails the gate, records inventory or a
/// `--fix`-able migration the board owner should know about.
fn info(code: &'static str, message: String) -> Issue {
    Issue {
        severity: "info",
        code,
        message,
    }
}

/// Rewrite a board's `%% kanban:settings` JSON to drop the retired `design`
/// metadata-key, returning the new board text only when the column was present.
/// The JSON is a single compact line; serde re-serializes it in the same
/// sorted-key form the settings block is written in, so every surviving column
/// stays byte-for-byte.
fn strip_design_column(board_text: &str) -> Option<String> {
    let mut changed = false;
    let mut lines: Vec<String> = Vec::new();
    for line in board_text.lines() {
        let trimmed = line.trim();
        if !changed
            && trimmed.starts_with("{\"kanban-plugin\"")
            && trimmed.contains("metadata-keys")
        {
            if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if let Some(keys) = value
                    .get_mut("metadata-keys")
                    .and_then(|k| k.as_array_mut())
                {
                    let before = keys.len();
                    keys.retain(|k| {
                        k.get("metadataKey").and_then(|m| m.as_str()) != Some("design")
                    });
                    if keys.len() != before {
                        changed = true;
                        lines.push(value.to_string());
                        continue;
                    }
                }
            }
        }
        lines.push(line.to_string());
    }
    if !changed {
        return None;
    }
    let mut out = lines.join("\n");
    if board_text.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

#[derive(Serialize)]
struct LintOutput {
    issues: Vec<Issue>,
    fixed: Vec<String>,
}

pub fn run(fs: &dyn Fs, paths: &Paths, renderer: &Renderer, fix: bool) -> Result<()> {
    let out = evaluate(fs, paths, fix)?;
    let has_error = out.issues.iter().any(|i| i.severity == "error");
    renderer.emit(&out, |r| human(&out, r))?;

    // Nonzero exit (for the flake check) when errors remain; the report is
    // already printed, so hand back a silent Reported.
    if has_error {
        return Err(Reported.into());
    }
    Ok(())
}

/// The board<->note checks and any `--fix` mutations, returning the findings so
/// tests can inspect them directly. `run` renders this and maps errors to a
/// nonzero exit.
fn evaluate(fs: &dyn Fs, paths: &Paths, fix: bool) -> Result<LintOutput> {
    let board_text = fs
        .read(&paths.board_md())
        .with_context(|| format!("read {}", paths.board_md().display()))?;
    let mut board = Board::parse(&board_text);
    let notes = layout::load_notes(fs, paths)?;
    let note_ids: HashSet<CardId> = notes.iter().filter_map(|e| e.id()).collect();

    let mut issues = Vec::new();
    let mut fixed = Vec::new();

    // 1. Required active lanes.
    for st in Status::ACTIVE {
        let title = st.lane_title().unwrap();
        if !board.lanes().iter().any(|l| l.title == title) {
            issues.push(error(
                "missing-lane",
                format!("board is missing the '{title}' lane"),
            ));
        }
    }

    // 1b. Structural lane corruption (card 5b4df3). A duplicate heading is
    // unambiguous corruption — `cards_in` reads only the first, so cards in the
    // rest are dropped on the next CLI rewrite — so it is an error. A lane the
    // tool doesn't recognize is only a *warning*: its cards are unreachable by
    // `status set` and hidden from `status`, but a deliberate extra lane (an
    // "Icebox") is a legitimate Obsidian arrangement the parser preserves, so it
    // must not hard-fail the `lint` gate.
    let mut lane_counts: HashMap<&str, u32> = HashMap::new();
    for lane in board.lanes() {
        *lane_counts.entry(lane.title.as_str()).or_default() += 1;
    }
    for (title, n) in &lane_counts {
        if *n > 1 {
            issues.push(error(
                "duplicate-lane",
                format!("board has {n} '{title}' lane headings; consolidate them into one (drag in Obsidian)"),
            ));
        }
    }
    for lane in board.lanes() {
        if !lane.cards.is_empty() && Status::from_lane_title(&lane.title).is_none() {
            issues.push(warn(
                "unrecognized-lane",
                format!(
                    "lane '{}' is not a known status; its {} card(s) are unreachable by `status set` and hidden from `status`",
                    lane.title,
                    lane.cards.len()
                ),
            ));
        }
    }

    // 2. Settings block.
    if !board_text.contains("%% kanban:settings") {
        issues.push(warn(
            "no-settings",
            "board has no `%% kanban:settings` block; the plugin won't surface card metadata (run `project init`)".into(),
        ));
    } else if strip_design_column(&board_text).is_some() {
        issues.push(info(
            "legacy-design-column",
            "board settings still declare the retired `design` column; run `lint --fix` to drop it"
                .into(),
        ));
    }

    // 3. Board cards: malformed links, duplicates, orphan cards.
    let mut seen: HashMap<CardId, u32> = HashMap::new();
    let mut orphan_ids: Vec<CardId> = Vec::new();
    for lane in board.lanes() {
        for card in &lane.cards {
            let where_ = format!("'{}'", lane.title);
            check_board_card(
                card.id(),
                &where_,
                card.raw(),
                &note_ids,
                &mut issues,
                &mut seen,
                &mut orphan_ids,
            );
            // A ticked box in an active lane is never written by the tool: it
            // signals a card that looks done but was not archived — e.g. cards
            // re-laned from an archive whose `## Archive` heading was lost (card
            // e6f6b7). Surface it so it isn't silently treated as live work.
            if card.is_checked() {
                let id = card.id().map_or_else(|| "?".to_string(), |i| i.to_string());
                issues.push(warn(
                    "checked-in-lane",
                    format!("card {id} in {where_} is checked (`- [x]`) but not archived"),
                ));
            }
        }
    }
    for card in board.archived() {
        check_board_card(
            card.id(),
            "the archive",
            card.raw(),
            &note_ids,
            &mut issues,
            &mut seen,
            &mut orphan_ids,
        );
    }
    for (id, n) in &seen {
        if *n > 1 {
            issues.push(error(
                "duplicate-card",
                format!("card {id} appears {n} times on the board"),
            ));
        }
    }
    let board_ids: HashSet<CardId> = seen.keys().cloned().collect();

    // 4. Notes: parse failures, iced notes, unknown blockers, disposition sanity.
    // A note with no board card is the icebox (design/PDD001): an intentional,
    // normal state, reported as an `info` inventory line rather than a warning.
    let mut iced_ids: Vec<CardId> = Vec::new();
    for e in &notes {
        match &e.meta {
            Err(err) => issues.push(error("note-parse", format!("{}: {err}", e.filename))),
            Ok(m) => {
                // A title that parses empty is unambiguous corruption — `new`
                // requires one — and it is the one defect nothing else
                // surfaces: `status` renders a blank column and the Obsidian
                // card face renders blank, while the board and the note stay
                // perfectly consistent. Silent blanking is the sharpest edge
                // here (card 579595), so it is an error, not a warning.
                if m.title.trim().is_empty() {
                    issues.push(error(
                        "empty-title",
                        format!(
                            "note {} ({}) has no readable `title:` — the card face and `project status` will render blank",
                            e.filename, m.id
                        ),
                    ));
                }
                if !board_ids.contains(&m.id) {
                    iced_ids.push(m.id.clone());
                }
                // The `design:` field is retired: it says what a label says.
                // Flag any note that still carries one; `--fix` folds it in.
                if e.note.get_scalar("design").is_some_and(|s| !s.is_empty()) {
                    issues.push(info(
                        "legacy-design",
                        format!(
                            "note {} ({}) carries a deprecated `design:` field; run `lint --fix` to fold it into a label",
                            e.filename, m.id
                        ),
                    ));
                }
                for b in &m.blocked_by {
                    if !note_ids.contains(b) && !board_ids.contains(b) {
                        issues.push(warn(
                            "unknown-blocker",
                            format!("{} is blocked_by unknown card {b}", m.id),
                        ));
                    }
                }
                let archived = matches!(board.locate(&m.id), Some(Location::Archive));
                let active = board.status_of(&m.id).is_some();
                if archived && m.disposition.is_none() {
                    issues.push(warn(
                        "no-disposition",
                        format!("archived card {} has no `disposition`", m.id),
                    ));
                }
                if active && m.disposition.is_some() {
                    issues.push(warn(
                        "stale-disposition",
                        format!("active card {} still carries `disposition`", m.id),
                    ));
                }
            }
        }
    }
    if !iced_ids.is_empty() {
        let ids = iced_ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        issues.push(info(
            "icebox",
            format!("{} note(s) in icebox: {ids}", iced_ids.len()),
        ));
    }

    // --fix: prune orphan cards, migrate legacy `design:` fields into labels,
    // and drop the retired `design` settings column. One board write at the end.
    if fix {
        let mut board_dirty = false;
        for id in &orphan_ids {
            if board.remove_card(id).is_some() {
                fixed.push(format!("pruned orphan card {id}"));
                board_dirty = true;
            }
        }
        issues.retain(|i| i.code != "orphan-card");

        // Fold any legacy `design:` field into labels, then drop the dead key.
        // The note is edited line-wise, so unknown keys stay byte-for-byte.
        for e in &notes {
            let Ok(m) = &e.meta else { continue };
            let Some(reference) = e.note.get_scalar("design").filter(|s| !s.is_empty()) else {
                continue;
            };
            let mut note = e.note.clone();
            let mut labels = note.get_list("labels");
            if !labels.contains(&reference) {
                labels.push(reference.clone());
            }
            note.set_list("labels", &labels);
            note.remove_key("design");
            let path = paths.note(&m.id);
            fs.write(&path, &note.to_text())
                .with_context(|| format!("rewrite {}", path.display()))?;
            fixed.push(format!(
                "folded design `{reference}` into a label on {}",
                m.id
            ));
        }
        issues.retain(|i| i.code != "legacy-design");

        // Drop the retired `design` column from the settings block. Strip the
        // freshly-rendered board if a card was pruned, else the original text.
        let mut board_out = if board_dirty {
            board.to_text()
        } else {
            board_text.clone()
        };
        if let Some(stripped) = strip_design_column(&board_out) {
            board_out = stripped;
            board_dirty = true;
            fixed.push("dropped the `design` column from the board settings".into());
        }
        issues.retain(|i| i.code != "legacy-design-column");

        if board_dirty {
            fs.write(&paths.board_md(), &board_out)?;
        }
    }

    Ok(LintOutput { issues, fixed })
}

fn human(out: &LintOutput, r: &Renderer) -> String {
    let mut s = String::new();
    for f in &out.fixed {
        s.push_str(&format!("{} {f}\n", r.green("fixed:")));
    }
    if out.issues.is_empty() {
        s.push_str(&r.green("clean — board and notes are consistent"));
        return s;
    }
    for i in &out.issues {
        let tag = match i.severity {
            "error" => r.red("error"),
            "info" => r.blue("info "),
            _ => r.yellow("warn "),
        };
        s.push_str(&format!("{tag} {}: {}\n", i.code, i.message));
    }
    s.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::board::Card;
    use crate::project::fs::FakeFs;
    use crate::project::note::Note;

    /// A legacy board: its settings still declare the retired `design` column,
    /// and card `a3f7b2`'s note still carries a `design: PDD005` field.
    fn legacy_fs() -> FakeFs {
        let settings = "%% kanban:settings\n\n```\n{\"kanban-plugin\":\"basic\",\"metadata-keys\":[{\"containsMarkdown\":false,\"label\":\"\",\"metadataKey\":\"title\",\"shouldHideLabel\":true},{\"containsMarkdown\":false,\"label\":\"type\",\"metadataKey\":\"type\",\"shouldHideLabel\":false},{\"containsMarkdown\":false,\"label\":\"design\",\"metadataKey\":\"design\",\"shouldHideLabel\":false},{\"containsMarkdown\":false,\"label\":\"blocked by\",\"metadataKey\":\"blocked_by\",\"shouldHideLabel\":false},{\"containsMarkdown\":false,\"label\":\"labels\",\"metadataKey\":\"labels\",\"shouldHideLabel\":false}]}\n```\n\n%%";
        let board = format!(
            "---\nkanban-plugin: basic\n---\n\n## To-do\n\n- [ ] [[a3f7b2]]\n\n## In Progress\n\n## Needs Review\n\n## Ready\n\n{settings}\n"
        );
        FakeFs::new()
            .with_file("/b/BOARD.md", &board)
            .with_file(
                "/b/issues/a3f7b2.md",
                "---\nid: a3f7b2\ntitle: X\ntype: feature\nblocked_by: []\ndesign: PDD005\nlabels: [net]\ncreated: 2026-01-01T00:00:00Z\n---\n\nbody\n",
            )
    }

    fn run_fix(fs: &FakeFs) {
        run(fs, &Paths::new("/b"), &Renderer::new(true, false), true).unwrap();
    }

    #[test]
    fn it_folds_a_legacy_design_field_into_labels() {
        let fs = legacy_fs();
        run_fix(&fs);
        let note = Note::parse(&fs.get("/b/issues/a3f7b2.md").unwrap()).unwrap();
        assert_eq!(note.get_list("labels"), vec!["net", "PDD005"]);
    }

    #[test]
    fn it_drops_the_design_key_from_the_note() {
        let fs = legacy_fs();
        run_fix(&fs);
        let note = Note::parse(&fs.get("/b/issues/a3f7b2.md").unwrap()).unwrap();
        assert_eq!(note.get_scalar("design"), None);
    }

    #[test]
    fn it_drops_the_design_column_from_the_board_settings() {
        let fs = legacy_fs();
        run_fix(&fs);
        let board = fs.get("/b/BOARD.md").unwrap();
        assert!(!board.contains("\"metadataKey\":\"design\""));
        // Every other column survives.
        assert!(board.contains("\"metadataKey\":\"labels\""));
        assert!(board.contains("\"metadataKey\":\"blocked_by\""));
        assert!(board.contains("\"metadataKey\":\"type\""));
    }

    #[test]
    fn it_is_idempotent_on_a_second_run() {
        let fs = legacy_fs();
        run_fix(&fs);
        let board_1 = fs.get("/b/BOARD.md").unwrap();
        let note_1 = fs.get("/b/issues/a3f7b2.md").unwrap();
        // A second --fix reports no change: the files are byte-identical.
        run_fix(&fs);
        assert_eq!(fs.get("/b/BOARD.md").unwrap(), board_1);
        assert_eq!(fs.get("/b/issues/a3f7b2.md").unwrap(), note_1);
    }

    #[test]
    fn clean_board_has_no_issues() {
        let mut board = Board::parse(&layout::initial_board());
        board.insert_card(
            Status::Todo,
            Card::new_link(&CardId::parse("a3f7b2").unwrap()),
            false,
        );
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &board.to_text())
            .with_file(
                "/b/issues/a3f7b2.md",
                "---\nid: a3f7b2\ntitle: X\ntype: feature\nblocked_by: []\n---\n",
            );
        let paths = Paths::new("/b");
        // No errors -> Ok.
        assert!(run(&fs, &paths, &Renderer::new(true, false), false).is_ok());
    }

    #[test]
    fn orphan_card_is_an_error_and_fix_prunes_it() {
        let mut board = Board::parse(&layout::initial_board());
        board.insert_card(
            Status::Todo,
            Card::new_link(&CardId::parse("deadbe").unwrap()),
            false,
        );
        let fs = FakeFs::new().with_file("/b/BOARD.md", &board.to_text());
        let paths = Paths::new("/b");
        let r = Renderer::new(true, false);

        // Orphan card (no note) is an error -> nonzero.
        assert!(run(&fs, &paths, &r, false).is_err());
        // --fix prunes it and then the board is clean.
        run(&fs, &paths, &r, true).unwrap();
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert!(board.cards_in(Status::Todo).is_empty());
    }

    #[test]
    fn duplicate_lane_heading_is_an_error() {
        // Two `## To-do` lanes — the shape the old separator-loss bug produced.
        let board = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n## To-do\n\n## In Progress\n\n## Needs Review\n\n## Ready\n\n%% kanban:settings\n\n```\n{}\n```\n\n%%\n";
        let fs = FakeFs::new().with_file("/b/BOARD.md", board);
        let paths = Paths::new("/b");
        assert!(run(&fs, &paths, &Renderer::new(true, false), false).is_err());
    }

    #[test]
    fn card_in_an_unrecognized_lane_is_a_warning_not_an_error() {
        // A card in a lane the tool doesn't know (a deliberate "Icebox", or a
        // mangled lane) is surfaced but must not hard-fail the gate, since an
        // extra Obsidian lane is a legitimate, parser-preserved arrangement.
        let board = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n## In Progress\n\n## Needs Review\n\n## Ready\n\n## Icebox\n\n- [ ] [[a3f7b2]]\n\n%% kanban:settings\n\n```\n{}\n```\n\n%%\n";
        let fs = FakeFs::new().with_file("/b/BOARD.md", board).with_file(
            "/b/issues/a3f7b2.md",
            "---\nid: a3f7b2\ntitle: X\ntype: feature\nblocked_by: []\n---\n",
        );
        let paths = Paths::new("/b");
        // Warning only -> exit 0.
        assert!(run(&fs, &paths, &Renderer::new(true, false), false).is_ok());
    }

    #[test]
    fn checked_card_in_an_active_lane_is_a_warning() {
        // A `- [x]` card left in To-do — e.g. re-laned from an archive whose
        // heading was lost — is surfaced but doesn't hard-fail the gate.
        let board = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n- [x] [[a3f7b2]]\n\n## In Progress\n\n## Needs Review\n\n## Ready\n\n%% kanban:settings\n\n```\n{}\n```\n\n%%\n";
        let fs = FakeFs::new().with_file("/b/BOARD.md", board).with_file(
            "/b/issues/a3f7b2.md",
            "---\nid: a3f7b2\ntitle: X\ntype: feature\nblocked_by: []\n---\n",
        );
        let paths = Paths::new("/b");
        // Warning only -> exit 0, but the finding is present.
        assert!(run(&fs, &paths, &Renderer::new(true, false), false).is_ok());
    }

    /// A board with no cards and one iced note (`a3f7b2`, no board card).
    fn iced_fs() -> (FakeFs, Paths) {
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &layout::initial_board())
            .with_file(
                "/b/issues/a3f7b2.md",
                "---\nid: a3f7b2\ntitle: X\ntype: feature\nblocked_by: []\n---\n",
            );
        (fs, Paths::new("/b"))
    }

    #[test]
    fn it_reports_an_iced_note_as_info_not_warn() {
        let (fs, paths) = iced_fs();
        let out = evaluate(&fs, &paths, false).unwrap();
        let iced: Vec<_> = out.issues.iter().filter(|i| i.code == "icebox").collect();
        assert_eq!(iced.len(), 1);
        assert_eq!(iced[0].severity, "info");
        assert!(iced[0].message.contains("a3f7b2"));
        // The old `orphan-note` warning is gone.
        assert!(out.issues.iter().all(|i| i.code != "orphan-note"));
    }

    #[test]
    fn it_exits_zero_with_iced_notes_present() {
        let (fs, paths) = iced_fs();
        // Info only -> Ok (exit 0).
        assert!(run(&fs, &paths, &Renderer::new(true, false), false).is_ok());
    }

    /// A board with one To-do card whose note is `note_text`.
    fn board_with_note(note_text: &str) -> (FakeFs, Paths) {
        let mut board = Board::parse(&layout::initial_board());
        board.insert_card(
            Status::Todo,
            Card::new_link(&CardId::parse("a3f7b2").unwrap()),
            false,
        );
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &board.to_text())
            .with_file("/b/issues/a3f7b2.md", note_text);
        (fs, Paths::new("/b"))
    }

    #[test]
    fn a_card_with_no_readable_title_is_an_error() {
        // The board and the note are perfectly consistent here, so every other
        // check passes — lint used to report `clean` on a card that renders
        // blank everywhere (card 579595).
        let (fs, paths) = board_with_note("---\nid: a3f7b2\ntitle:\ntype: feature\n---\n");
        assert!(run(&fs, &paths, &Renderer::new(true, false), false).is_err());
    }

    #[test]
    fn a_prettier_wrapped_title_lints_clean() {
        // The reflowed shape `markdown format` produces is readable, so it must
        // not trip the empty-title error.
        let (fs, paths) = board_with_note(
            "---\nid: a3f7b2\ntitle:\n  \"Hit points in the heart: damage, the Damaged event, and phase-free death\"\ntype: feature\n---\n",
        );
        assert!(run(&fs, &paths, &Renderer::new(true, false), false).is_ok());
    }
}
