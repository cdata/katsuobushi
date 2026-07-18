//! Shared selection helpers — a card's resolved status and whether it is
//! Available (grabbable). Pulled out of `query` so `sandbox dispatch` can guard
//! on the exact same rule the board uses, without duplicating it.

use super::board::{Board, Location};
use super::layout::NoteEntry;
use super::model::{CardId, Status};
use super::state::clears_dependents;

/// A card's status: its lane if active, else its note's `disposition` if
/// archived, else `None`.
pub fn resolve_status(board: &Board, notes: &[NoteEntry], id: &CardId) -> Option<Status> {
    match board.locate(id)? {
        Location::Lane(_) => board.status_of(id),
        Location::Archive => notes
            .iter()
            .find(|e| e.id().as_ref() == Some(id))
            .and_then(|e| e.meta.as_ref().ok())
            .and_then(|m| m.disposition),
    }
}

/// A card's declared blockers (from its note frontmatter), or empty.
fn blockers(notes: &[NoteEntry], id: &CardId) -> Vec<CardId> {
    notes
        .iter()
        .find(|e| e.id().as_ref() == Some(id))
        .and_then(|e| e.meta.as_ref().ok())
        .map(|m| m.blocked_by.clone())
        .unwrap_or_default()
}

/// Grabbable: To-do with every blocker at `ready` or later (design/project.md
/// §4 — downstream builds only on reviewed work).
pub fn is_available(board: &Board, notes: &[NoteEntry], id: &CardId) -> bool {
    resolve_status(board, notes, id) == Some(Status::Todo)
        && blockers(notes, id).iter().all(|b| {
            resolve_status(board, notes, b)
                .map(clears_dependents)
                .unwrap_or(false)
        })
}

/// A card's `created` timestamp (RFC-3339, so it sorts lexically = chronologically).
fn created_of(notes: &[NoteEntry], id: &CardId) -> Option<String> {
    notes
        .iter()
        .find(|e| e.id().as_ref() == Some(id))
        .and_then(|e| e.meta.as_ref().ok())
        .and_then(|m| m.created.clone())
}

/// Suggested-acceptance sort key: dated-before-undated, then `created` ascending,
/// then id for determinism.
fn accept_key(notes: &[NoteEntry], id: &CardId) -> (bool, String, String) {
    let created = created_of(notes, id);
    (
        created.is_none(),
        created.unwrap_or_default(),
        id.to_string(),
    )
}

/// Where a card entering the Ready lane should sit **without disturbing the cards
/// already there** (which may carry a manual priority order). Returns an index
/// into `existing` (`0..=existing.len()`) at which to insert `new`.
///
/// Dependencies are hard bounds: the entrant lands *after* every Ready card that
/// blocks it and *before* every Ready card it blocks. Within that window it is
/// placed by suggested-acceptance order (oldest `created` first) relative to the
/// existing cards — so a fresh arrival slots into a roughly-chronological
/// position, but the lane is never globally re-sorted (design/project.md; card
/// 5eb75c). `existing` must exclude `new` itself.
pub fn ready_insertion_index(notes: &[NoteEntry], existing: &[CardId], new: &CardId) -> usize {
    let new_blockers = blockers(notes, new);
    // Just past the last existing card that blocks `new`.
    let lo = existing
        .iter()
        .enumerate()
        .filter(|(_, id)| new_blockers.contains(id))
        .map(|(i, _)| i + 1)
        .max()
        .unwrap_or(0);
    // At the first existing card that `new` blocks (a dependent must follow it).
    let hi = existing
        .iter()
        .position(|id| blockers(notes, id).contains(new))
        .unwrap_or(existing.len())
        .max(lo); // defensive: a manual order that already violates a dependency.

    // Within [lo, hi], slot before the first existing card that sorts after `new`.
    let new_key = accept_key(notes, new);
    (lo..hi)
        .find(|&i| accept_key(notes, &existing[i]) > new_key)
        .unwrap_or(hi)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::fs::FakeFs;
    use crate::project::layout::{self, Paths};

    fn id(hex: &str) -> CardId {
        CardId::parse(hex).unwrap()
    }

    /// Build notes from `(id, created, blocked_by)` triples.
    fn notes_from(specs: &[(&str, &str, &[&str])]) -> Vec<NoteEntry> {
        let mut fs = FakeFs::new();
        for (hex, created, blocked) in specs {
            let bl = blocked.join(", ");
            let created_line = if created.is_empty() {
                String::new()
            } else {
                format!("created: {created}\n")
            };
            fs = fs.with_file(
                format!("/b/issues/{hex}.md"),
                &format!(
                    "---\nid: {hex}\ntitle: {hex}\ntype: feature\nblocked_by: [{bl}]\n{created_line}---\n\nbody\n"
                ),
            );
        }
        layout::load_notes(&fs, &Paths::new("/b")).unwrap()
    }

    #[test]
    fn it_slots_a_newer_card_below_older_ones_by_created() {
        // Existing lane in chronological order; a newest card lands at the bottom.
        let notes = notes_from(&[
            ("aaaaaa", "2026-01-01T00:00:00Z", &[]),
            ("bbbbbb", "2026-02-01T00:00:00Z", &[]),
            ("cccccc", "2026-03-01T00:00:00Z", &[]),
        ]);
        let existing = [id("aaaaaa"), id("bbbbbb")];
        assert_eq!(ready_insertion_index(&notes, &existing, &id("cccccc")), 2);
    }

    #[test]
    fn it_slots_an_older_card_above_newer_ones_by_created() {
        let notes = notes_from(&[
            ("aaaaaa", "2026-02-01T00:00:00Z", &[]),
            ("bbbbbb", "2026-03-01T00:00:00Z", &[]),
            ("cccccc", "2026-01-01T00:00:00Z", &[]),
        ]);
        let existing = [id("aaaaaa"), id("bbbbbb")];
        // Oldest cccccc slots above both.
        assert_eq!(ready_insertion_index(&notes, &existing, &id("cccccc")), 0);
    }

    #[test]
    fn it_preserves_a_manual_order_and_only_places_the_entrant() {
        // Existing lane is NOT in chronological order (a manual arrangement). The
        // entrant slots by created relative to it; the existing cards do not move
        // (this function only returns the entrant's index).
        let notes = notes_from(&[
            ("aaaaaa", "2026-05-01T00:00:00Z", &[]), // manually put on top
            ("bbbbbb", "2026-01-01T00:00:00Z", &[]),
            ("cccccc", "2026-03-01T00:00:00Z", &[]),
        ]);
        let existing = [id("aaaaaa"), id("bbbbbb")]; // manual: newer above older
                                                     // cccccc (2026-03) sorts after aaaaaa(05)? no — 03 < 05, so before aaaaaa
                                                     // at index 0 (first existing card whose key > cccccc's).
        assert_eq!(ready_insertion_index(&notes, &existing, &id("cccccc")), 0);
    }

    #[test]
    fn it_keeps_the_entrant_after_a_blocker_that_is_still_in_ready() {
        // aaaaaa blocks bbbbbb; even though bbbbbb is older, it must land AFTER
        // aaaaaa while the blocker is still in the lane.
        let notes = notes_from(&[
            ("aaaaaa", "2026-05-01T00:00:00Z", &[]),
            ("bbbbbb", "2026-01-01T00:00:00Z", &["aaaaaa"]),
        ]);
        let existing = [id("aaaaaa")];
        assert_eq!(ready_insertion_index(&notes, &existing, &id("bbbbbb")), 1);
    }

    #[test]
    fn it_keeps_the_entrant_before_a_card_it_blocks() {
        // The entrant aaaaaa blocks the existing bbbbbb, so it must land before it
        // regardless of dates.
        let notes = notes_from(&[
            ("aaaaaa", "2026-09-01T00:00:00Z", &[]),
            ("bbbbbb", "2026-01-01T00:00:00Z", &["aaaaaa"]),
        ]);
        let existing = [id("bbbbbb")];
        assert_eq!(ready_insertion_index(&notes, &existing, &id("aaaaaa")), 0);
    }

    #[test]
    fn it_ignores_a_blocker_that_has_left_the_ready_lane() {
        // aaaaaa is blocked by ffffff, which is NOT in the lane (already accepted),
        // so that edge imposes no bound and created placement wins.
        let notes = notes_from(&[
            ("aaaaaa", "2026-01-01T00:00:00Z", &["ffffff"]),
            ("bbbbbb", "2026-05-01T00:00:00Z", &[]),
        ]);
        let existing = [id("bbbbbb")];
        // aaaaaa (older) slots above bbbbbb.
        assert_eq!(ready_insertion_index(&notes, &existing, &id("aaaaaa")), 0);
    }

    #[test]
    fn it_appends_into_an_empty_ready_lane() {
        let notes = notes_from(&[("aaaaaa", "2026-01-01T00:00:00Z", &[])]);
        assert_eq!(ready_insertion_index(&notes, &[], &id("aaaaaa")), 0);
    }
}
