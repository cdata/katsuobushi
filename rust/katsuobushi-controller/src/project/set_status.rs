//! `project set-status` — move a card between lanes / to the archive, enforcing
//! the state machine and stamping `disposition` on terminal moves.

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::sandbox::output::Renderer;

use super::board::{Board, Location};
use super::clock::{format_rfc3339, Clock};
use super::fs::Fs;
use super::layout::{self, Paths};
use super::model::{CardId, Status};
use super::note::Note;
use super::state::{rejection_reason, transition_allowed};

#[derive(Serialize)]
struct SetStatusOutput {
    id: String,
    from: String,
    to: String,
}

pub fn run(
    fs: &dyn Fs,
    paths: &Paths,
    renderer: &Renderer,
    clock: &dyn Clock,
    id_input: &str,
    to: Status,
    force: bool,
) -> Result<()> {
    let (id, from) = apply(fs, paths, clock, id_input, to, force)?;
    let out = SetStatusOutput {
        id: id.to_string(),
        from: from.to_string(),
        to: to.to_string(),
    };
    renderer.emit(&out, |r| {
        format!("{} {} -> {}", out.id, out.from, r.green(&out.to))
    })
}

/// `project status set --accept-all` — move every card in the Ready lane to
/// Accepted (`ready -> accepted`), the product owner's bulk sign-off. Each card
/// is archived with `disposition`/`disposition_at` stamped, exactly as a single
/// `set … accepted` would. Prints the accepted ids (human) / a JSON list.
pub fn accept_all(
    fs: &dyn Fs,
    paths: &Paths,
    renderer: &Renderer,
    clock: &dyn Clock,
) -> Result<()> {
    let board_text = fs
        .read(&paths.board_md())
        .with_context(|| format!("read {}", paths.board_md().display()))?;
    let board = Board::parse(&board_text);
    // Snapshot the Ready ids first: each `apply` re-reads and rewrites the board,
    // so we must not iterate a lane we are draining.
    let ready: Vec<CardId> = board
        .cards_in(Status::Ready)
        .iter()
        .filter_map(|c| c.id())
        .collect();

    let mut accepted: Vec<String> = Vec::new();
    for id in &ready {
        apply(fs, paths, clock, id.as_str(), Status::Accepted, false)
            .with_context(|| format!("accepting {id}"))?;
        accepted.push(id.to_string());
    }

    renderer.emit(&accepted, |r| {
        if accepted.is_empty() {
            "(no cards in Ready)".to_string()
        } else {
            accepted
                .iter()
                .map(|id| format!("{id} ready -> {}", r.green("accepted")))
                .collect::<Vec<_>>()
                .join("\n")
        }
    })
}

/// The state-machine writer, without any rendering: validate the transition,
/// stamp `disposition` on a terminal crossing, move the card, and persist the
/// board. Returns `(resolved id, previous status)`. Shared by `set-status` and
/// by `sandbox dispatch`'s claim step.
pub fn apply(
    fs: &dyn Fs,
    paths: &Paths,
    clock: &dyn Clock,
    id_input: &str,
    to: Status,
    force: bool,
) -> Result<(CardId, Status)> {
    let board_text = fs
        .read(&paths.board_md())
        .with_context(|| format!("read {}", paths.board_md().display()))?;
    let mut board = Board::parse(&board_text);
    let notes = layout::load_notes(fs, paths)?;

    let id = layout::resolve_id(id_input, &super::board_ids(&board))?;
    let from = current_status(&board, &notes, &id)?;
    if !force && !transition_allowed(from, to) {
        bail!("{}", rejection_reason(from, to));
    }

    // Record disposition (+ disposition_at) when crossing into (or, on a forced
    // reopen, out of) a terminal state — the one note write set-status performs.
    if to.is_terminal() {
        update_note(fs, paths, clock, &notes, &id, Some(to))?;
    } else if from.is_terminal() {
        update_note(fs, paths, clock, &notes, &id, None)?;
    }

    if !board.move_card(&id, to) {
        bail!(
            "could not move {id}: the '{}' lane is missing (run `project lint`)",
            to
        );
    }
    // Slot a card entering Ready into suggested-acceptance position (dependencies
    // first, then oldest `created`) WITHOUT reordering the cards already there, so
    // a manual priority order in the lane is preserved (design/project.md; card
    // 5eb75c).
    if to == Status::Ready {
        let existing: Vec<CardId> = board
            .cards_in(Status::Ready)
            .iter()
            .filter_map(|c| c.id())
            .filter(|c| c != &id)
            .collect();
        let idx = super::select::ready_insertion_index(&notes, &existing, &id);
        let anchor = match existing.get(idx) {
            Some(before) => super::board::Anchor::Before(before.clone()),
            None => super::board::Anchor::Bottom,
        };
        board.reorder(&id, anchor);
    }
    fs.write(&paths.board_md(), &board.to_text())?;
    Ok((id, from))
}

/// A card's current status: its lane for active cards, or the note's
/// `disposition` for archived ones.
fn current_status(board: &Board, notes: &[layout::NoteEntry], id: &CardId) -> Result<Status> {
    match board.locate(id) {
        Some(Location::Lane(_)) => board
            .status_of(id)
            .ok_or_else(|| anyhow::anyhow!("card {id} is in an unrecognized lane")),
        Some(Location::Archive) => notes
            .iter()
            .find(|e| e.id().as_ref() == Some(id))
            .and_then(|e| e.meta.as_ref().ok())
            .and_then(|m| m.disposition)
            .ok_or_else(|| {
                anyhow::anyhow!("archived card {id} has no `disposition` (run `project lint`)")
            }),
        None => bail!("no card {id} on the board"),
    }
}

/// Set or clear the note's `disposition:`/`disposition_at:` pair. Entering a
/// terminal state stamps both (the outcome + the instant, via the clock);
/// leaving it (a forced reopen) clears both to empty, mirroring how the board
/// re-lanes the card.
fn update_note(
    fs: &dyn Fs,
    paths: &Paths,
    clock: &dyn Clock,
    notes: &[layout::NoteEntry],
    id: &CardId,
    disposition: Option<Status>,
) -> Result<()> {
    let entry = notes
        .iter()
        .find(|e| e.id().as_ref() == Some(id))
        .ok_or_else(|| anyhow::anyhow!("card {id} has no note file; cannot record disposition"))?;
    let path = paths.issues_dir().join(&entry.filename);
    let text = fs.read(&path)?;
    let mut note = Note::parse(&text)?;
    let stamp = match disposition {
        Some(_) => format_rfc3339(clock.now_unix()),
        None => String::new(),
    };
    note.set_scalar("disposition", disposition.map(|s| s.token()).unwrap_or(""));
    note.set_scalar("disposition_at", &stamp);
    fs.write(&path, &note.to_text())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::clock::{parse_rfc3339, FixedClock};
    use crate::project::fs::FakeFs;
    use crate::project::layout;
    use crate::project::note::{Note, NoteMeta};

    /// A fixed instant for the clock seam: 2026-07-17T18:22:04Z.
    const T0: i64 = 1_784_312_524;

    /// A board with one To-do card and its note.
    fn seeded() -> (FakeFs, Paths) {
        let mut board = Board::parse(&layout::initial_board());
        board.insert_card(
            Status::Todo,
            crate::project::board::Card::new_link(&CardId::parse("a3f7b2").unwrap()),
            false,
        );
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &board.to_text())
            .with_file(
                "/b/issues/a3f7b2.md",
                "---\nid: a3f7b2\ntitle: Thing\ntype: feature\nblocked_by: []\ncreated: 2026-01-01T00:00:00Z\n---\n\nbody\n",
            );
        (fs, Paths::new("/b"))
    }

    #[test]
    fn legal_move_updates_the_lane() {
        let (fs, paths) = seeded();
        let r = Renderer::new(false, false);
        run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            Status::InProgress,
            false,
        )
        .unwrap();
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert_eq!(
            board.status_of(&CardId::parse("a3f7b2").unwrap()),
            Some(Status::InProgress)
        );
    }

    #[test]
    fn illegal_move_is_rejected_but_force_wins() {
        let (fs, paths) = seeded();
        let r = Renderer::new(false, false);
        // todo -> accepted is illegal.
        assert!(run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            Status::Accepted,
            false
        )
        .is_err());
        // ...but --force bypasses.
        assert!(run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            Status::Accepted,
            true
        )
        .is_ok());
    }

    #[test]
    fn terminal_move_archives_and_stamps_disposition() {
        let (fs, paths) = seeded();
        let r = Renderer::new(false, false);
        // todo -> in-progress -> needs-review -> ready -> accepted.
        for to in [
            Status::InProgress,
            Status::NeedsReview,
            Status::Ready,
            Status::Accepted,
        ] {
            run(&fs, &paths, &r, &FixedClock(T0), "a3f7b2", to, false).unwrap();
        }
        // The note now carries disposition: accepted, stamped at the clock instant.
        let note = Note::parse(&fs.get("/b/issues/a3f7b2.md").unwrap()).unwrap();
        let meta = NoteMeta::from_note(&note).unwrap();
        assert_eq!(meta.disposition, Some(Status::Accepted));
        assert_eq!(meta.disposition_at.as_deref(), Some("2026-07-17T18:22:04Z"));
        // And the card is archived (off the lanes).
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert_eq!(board.status_of(&CardId::parse("a3f7b2").unwrap()), None);
        assert_eq!(board.archived().len(), 1);
    }

    #[test]
    fn cancelled_move_archives_and_stamps_disposition() {
        // The cancelled path shares update_note with accepted, but assert it
        // directly: todo -> cancelled stamps disposition + disposition_at.
        let (fs, paths) = seeded();
        let r = Renderer::new(false, false);
        run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            Status::Cancelled,
            false,
        )
        .unwrap();
        let note = Note::parse(&fs.get("/b/issues/a3f7b2.md").unwrap()).unwrap();
        let meta = NoteMeta::from_note(&note).unwrap();
        assert_eq!(meta.disposition, Some(Status::Cancelled));
        assert_eq!(
            meta.disposition_at.as_deref().and_then(parse_rfc3339),
            Some(T0)
        );
        // And the card is archived (off the lanes).
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert_eq!(board.status_of(&CardId::parse("a3f7b2").unwrap()), None);
        assert_eq!(board.archived().len(), 1);
    }

    #[test]
    fn unknown_card_errors() {
        let (fs, paths) = seeded();
        let r = Renderer::new(false, false);
        assert!(run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "deadbe",
            Status::InProgress,
            false
        )
        .is_err());
    }

    #[test]
    fn entering_ready_slots_the_entrant_by_acceptance_order() {
        // Two independent cards; the newer one reaches Ready first. When the older
        // one enters it slots above (earlier acceptance) — the entrant is placed,
        // the incumbent is not moved.
        let mut board = Board::parse(&layout::initial_board());
        for hex in ["aaaaaa", "bbbbbb"] {
            board.insert_card(
                Status::Todo,
                crate::project::board::Card::new_link(&CardId::parse(hex).unwrap()),
                false,
            );
        }
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &board.to_text())
            .with_file(
                "/b/issues/aaaaaa.md",
                "---\nid: aaaaaa\ntitle: A\ntype: feature\nblocked_by: []\ncreated: 2026-06-01T00:00:00Z\n---\n\nbody\n",
            )
            .with_file(
                "/b/issues/bbbbbb.md",
                "---\nid: bbbbbb\ntitle: B\ntype: feature\nblocked_by: []\ncreated: 2026-01-01T00:00:00Z\n---\n\nbody\n",
            );
        let paths = Paths::new("/b");
        let r = Renderer::new(false, false);
        // Newer aaaaaa enters Ready first, then older bbbbbb.
        for hex in ["aaaaaa", "bbbbbb"] {
            for to in [Status::InProgress, Status::NeedsReview, Status::Ready] {
                run(&fs, &paths, &r, &FixedClock(T0), hex, to, false).unwrap();
            }
        }
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        let ready: Vec<String> = board
            .cards_in(Status::Ready)
            .iter()
            .filter_map(|c| c.id())
            .map(|i| i.to_string())
            .collect();
        assert_eq!(ready, vec!["bbbbbb".to_string(), "aaaaaa".to_string()]);
    }

    #[test]
    fn accept_all_accepts_every_ready_card() {
        // Two cards, both driven to Ready.
        let mut board = Board::parse(&layout::initial_board());
        for hex in ["aaaaaa", "bbbbbb"] {
            board.insert_card(
                Status::Todo,
                crate::project::board::Card::new_link(&CardId::parse(hex).unwrap()),
                false,
            );
        }
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &board.to_text())
            .with_file(
                "/b/issues/aaaaaa.md",
                "---\nid: aaaaaa\ntitle: A\ntype: feature\nblocked_by: []\ncreated: 2026-01-01T00:00:00Z\n---\n\nbody\n",
            )
            .with_file(
                "/b/issues/bbbbbb.md",
                "---\nid: bbbbbb\ntitle: B\ntype: feature\nblocked_by: []\ncreated: 2026-01-01T00:00:00Z\n---\n\nbody\n",
            );
        let paths = Paths::new("/b");
        let r = Renderer::new(false, false);
        let clock = FixedClock(T0);
        for hex in ["aaaaaa", "bbbbbb"] {
            for to in [Status::InProgress, Status::NeedsReview, Status::Ready] {
                run(&fs, &paths, &r, &clock, hex, to, false).unwrap();
            }
        }

        accept_all(&fs, &paths, &r, &clock).unwrap();

        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert!(board.cards_in(Status::Ready).is_empty());
        assert_eq!(board.archived().len(), 2);
        for hex in ["aaaaaa", "bbbbbb"] {
            let note = Note::parse(&fs.get(format!("/b/issues/{hex}.md")).unwrap()).unwrap();
            let meta = NoteMeta::from_note(&note).unwrap();
            assert_eq!(meta.disposition, Some(Status::Accepted));
            assert_eq!(
                meta.disposition_at.as_deref().and_then(parse_rfc3339),
                Some(T0)
            );
        }
    }

    #[test]
    fn accept_all_on_empty_ready_is_a_noop() {
        // The seeded board's only card is in To-do; Ready is empty.
        let (fs, paths) = seeded();
        let r = Renderer::new(false, false);
        accept_all(&fs, &paths, &r, &FixedClock(T0)).unwrap();
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert_eq!(board.archived().len(), 0);
    }

    #[test]
    fn forced_reopen_from_terminal_clears_disposition() {
        let (fs, paths) = seeded();
        let r = Renderer::new(false, false);
        // Force to accepted: archived + disposition/disposition_at stamped.
        run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            Status::Accepted,
            true,
        )
        .unwrap();
        let note = Note::parse(&fs.get("/b/issues/a3f7b2.md").unwrap()).unwrap();
        let meta = NoteMeta::from_note(&note).unwrap();
        assert_eq!(meta.disposition, Some(Status::Accepted));
        assert!(meta.disposition_at.is_some());

        // Force reopen accepted -> in-progress: both cleared, card re-laned.
        run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            Status::InProgress,
            true,
        )
        .unwrap();
        let note = Note::parse(&fs.get("/b/issues/a3f7b2.md").unwrap()).unwrap();
        let meta = NoteMeta::from_note(&note).unwrap();
        assert_eq!(meta.disposition, None);
        assert_eq!(meta.disposition_at, None);
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert_eq!(
            board.status_of(&CardId::parse("a3f7b2").unwrap()),
            Some(Status::InProgress)
        );
    }
}
