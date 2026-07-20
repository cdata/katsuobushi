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

#[derive(Serialize)]
struct LintOutput {
    issues: Vec<Issue>,
    fixed: Vec<String>,
}

pub fn run(fs: &dyn Fs, paths: &Paths, renderer: &Renderer, fix: bool) -> Result<()> {
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

    // 4. Notes: parse failures, orphans, unknown blockers, disposition sanity.
    for e in &notes {
        match &e.meta {
            Err(err) => issues.push(error("note-parse", format!("{}: {err}", e.filename))),
            Ok(m) => {
                if !board_ids.contains(&m.id) {
                    issues.push(warn(
                        "orphan-note",
                        format!("note {} ({}) has no card on the board", e.filename, m.id),
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

    // --fix: prune orphan cards (board cards whose note is gone).
    if fix && !orphan_ids.is_empty() {
        for id in &orphan_ids {
            if board.remove_card(id).is_some() {
                fixed.push(format!("pruned orphan card {id}"));
            }
        }
        fs.write(&paths.board_md(), &board.to_text())?;
        issues.retain(|i| i.code != "orphan-card");
    }

    let has_error = issues.iter().any(|i| i.severity == "error");
    let out = LintOutput { issues, fixed };
    renderer.emit(&out, |r| human(&out, r))?;

    // Nonzero exit (for the flake check) when errors remain; the report is
    // already printed, so hand back a silent Reported.
    if has_error {
        return Err(Reported.into());
    }
    Ok(())
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
    fn orphan_note_is_a_warning_not_an_error() {
        // A note with no card on the board.
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &layout::initial_board())
            .with_file(
                "/b/issues/a3f7b2.md",
                "---\nid: a3f7b2\ntitle: X\ntype: feature\nblocked_by: []\n---\n",
            );
        let paths = Paths::new("/b");
        // Warnings only -> Ok (exit 0).
        assert!(run(&fs, &paths, &Renderer::new(true, false), false).is_ok());
    }
}
