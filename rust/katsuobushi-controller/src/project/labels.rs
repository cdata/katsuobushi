//! `project labels` — enumerate the label vocabulary and the cards under each.
//!
//! The read counterpart to the `--label` filter: `--label` narrows the board to
//! one epic; `labels` lists every epic and its size. Archived (accepted or
//! cancelled) cards are excluded by default, since a label index is about live
//! work; `--include-archived` folds them back in. An iced note (a note with no
//! board card) counts — a label is a note property, and iced work is still work.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;

use crate::output::{render_table, Renderer, TableCell};

use super::board::{Board, Location};
use super::fs::Fs;
use super::layout::{self, NoteEntry, Paths};

/// One label and the cards that carry it.
#[derive(Serialize, Debug, PartialEq, Eq)]
struct LabelGroup {
    label: String,
    count: usize,
    ids: Vec<String>,
}

pub fn run(fs: &dyn Fs, paths: &Paths, renderer: &Renderer, include_archived: bool) -> Result<()> {
    let board_text = fs
        .read(&paths.board_md())
        .with_context(|| format!("read {}", paths.board_md().display()))?;
    let board = Board::parse(&board_text);
    let notes = layout::load_notes(fs, paths)?;

    let groups = collect(&board, &notes, include_archived);
    renderer.emit(&groups, |_| table(&groups))
}

/// Aggregate labels across the notes: label -> the ids carrying it. An archived
/// card (located in `## Archive`) is skipped unless `include_archived`. Labels
/// sort alphabetically (BTreeMap); ids sort within each group. A pure function
/// so the aggregation is unit-tested without touching the renderer.
fn collect(board: &Board, notes: &[NoteEntry], include_archived: bool) -> Vec<LabelGroup> {
    let mut by_label: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in notes {
        let Ok(meta) = &entry.meta else { continue };
        let archived = matches!(board.locate(&meta.id), Some(Location::Archive));
        if archived && !include_archived {
            continue;
        }
        for label in &meta.labels {
            by_label
                .entry(label.clone())
                .or_default()
                .push(meta.id.to_string());
        }
    }
    by_label
        .into_iter()
        .map(|(label, mut ids)| {
            ids.sort();
            ids.dedup();
            LabelGroup {
                count: ids.len(),
                label,
                ids,
            }
        })
        .collect()
}

fn table(groups: &[LabelGroup]) -> String {
    if groups.is_empty() {
        return "(no labels)".to_string();
    }
    let rows: Vec<Vec<TableCell>> = groups
        .iter()
        .map(|g| {
            vec![
                TableCell::plain(g.label.clone()),
                TableCell::plain(g.count.to_string()),
            ]
        })
        .collect();
    render_table(&["LABEL", "COUNT"], &rows, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::board::Card;
    use crate::project::fs::FakeFs;
    use crate::project::model::{CardId, Status};

    /// A board + notes fixture:
    /// - `aaaaaa` (To-do) labels [net, PDD001]
    /// - `bbbbbb` (To-do) labels [PDD001]
    /// - `cccccc` iced (no board card) labels [PDD001, debt]
    /// - `dddddd` archived (accepted) labels [net]
    fn seeded() -> (FakeFs, Paths) {
        let mut board = Board::parse(&layout::initial_board());
        board.insert_card(
            Status::Todo,
            Card::new_link(&CardId::parse("aaaaaa").unwrap()),
            false,
        );
        board.insert_card(
            Status::Todo,
            Card::new_link(&CardId::parse("bbbbbb").unwrap()),
            false,
        );
        // dddddd rides straight into the archive as an accepted card.
        board.insert_card(
            Status::Todo,
            Card::new_link(&CardId::parse("dddddd").unwrap()),
            false,
        );
        board.move_card(&CardId::parse("dddddd").unwrap(), Status::Accepted);
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &board.to_text())
            .with_file(
                "/b/issues/aaaaaa.md",
                "---\nid: aaaaaa\ntitle: A\ntype: feature\nblocked_by: []\nlabels: [net, PDD001]\n---\n",
            )
            .with_file(
                "/b/issues/bbbbbb.md",
                "---\nid: bbbbbb\ntitle: B\ntype: feature\nblocked_by: []\nlabels: [PDD001]\n---\n",
            )
            .with_file(
                "/b/issues/cccccc.md",
                "---\nid: cccccc\ntitle: C\ntype: feature\nblocked_by: []\nlabels: [PDD001, debt]\n---\n",
            )
            .with_file(
                "/b/issues/dddddd.md",
                "---\nid: dddddd\ntitle: D\ntype: feature\nblocked_by: []\nlabels: [net]\ndisposition: accepted\n---\n",
            );
        (fs, Paths::new("/b"))
    }

    fn collect_labels(include_archived: bool) -> Vec<LabelGroup> {
        let (fs, paths) = seeded();
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        let notes = layout::load_notes(&fs, &paths).unwrap();
        collect(&board, &notes, include_archived)
    }

    fn group<'a>(groups: &'a [LabelGroup], label: &str) -> Option<&'a LabelGroup> {
        groups.iter().find(|g| g.label == label)
    }

    #[test]
    fn it_enumerates_unique_labels_with_counts() {
        let groups = collect_labels(false);
        // Sorted alphabetically; archived `dddddd` excluded, so `net` counts only aaaaaa.
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, vec!["PDD001", "debt", "net"]);
        assert_eq!(group(&groups, "PDD001").unwrap().count, 3);
        assert_eq!(group(&groups, "net").unwrap().count, 1);
        assert_eq!(group(&groups, "debt").unwrap().count, 1);
    }

    #[test]
    fn it_maps_each_label_to_its_card_ids() {
        let groups = collect_labels(false);
        assert_eq!(
            group(&groups, "PDD001").unwrap().ids,
            vec!["aaaaaa", "bbbbbb", "cccccc"]
        );
        assert_eq!(group(&groups, "net").unwrap().ids, vec!["aaaaaa"]);
    }

    #[test]
    fn it_excludes_archived_cards_by_default() {
        let groups = collect_labels(false);
        // dddddd is archived (accepted); its `net` label does not add to the count.
        assert!(!group(&groups, "net")
            .unwrap()
            .ids
            .contains(&"dddddd".to_string()));
    }

    #[test]
    fn it_includes_archived_cards_with_the_flag() {
        let groups = collect_labels(true);
        // Now `net` picks up the archived dddddd too.
        assert_eq!(group(&groups, "net").unwrap().ids, vec!["aaaaaa", "dddddd"]);
        assert_eq!(group(&groups, "net").unwrap().count, 2);
    }

    #[test]
    fn it_counts_iced_notes() {
        // cccccc has no board card (iced) yet contributes to PDD001 and debt.
        let groups = collect_labels(false);
        assert!(group(&groups, "PDD001")
            .unwrap()
            .ids
            .contains(&"cccccc".to_string()));
        assert!(group(&groups, "debt").is_some());
    }
}
