//! `project set-status` — move a card between lanes / to the archive, enforcing
//! the state machine and stamping `disposition` on terminal moves.

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::output::Renderer;

use super::board::{Board, Card, Location};
use super::clock::{format_rfc3339, Clock};
use super::fs::Fs;
use super::layout::{self, Paths};
use super::model::{CardId, Status};
use super::note::Note;
use super::state::{rejection_reason, transition_allowed};

/// The target of a `status set`. Every board lane is a [`Status`]; `Icebox` is
/// the extra target that shelves a card off the board (design/PDD001). It is not
/// a `Status` because a note carries no status — the icebox is the absence of a
/// board card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetTarget {
    Status(Status),
    Icebox,
}

impl SetTarget {
    fn token(self) -> &'static str {
        match self {
            SetTarget::Status(s) => s.token(),
            SetTarget::Icebox => "icebox",
        }
    }
}

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
    target: SetTarget,
    force: bool,
) -> Result<()> {
    let (id, from) = apply_target(fs, paths, clock, id_input, target, force)?;
    let out = SetStatusOutput {
        id: id.to_string(),
        from,
        to: target.token().to_string(),
    };
    renderer.emit(&out, |r| {
        format!("{} {} -> {}", out.id, out.from, r.green(&out.to))
    })
}

/// The full `status set` decision, including the icebox edges (design/PDD001):
/// shelve a board card to the icebox, promote an iced note onto the board, or
/// cancel an iced note straight into the archive. A card already on the board
/// takes the ordinary state-machine path in [`apply`]. Returns `(id, from)`
/// where `from` is the human "from" label (`icebox` for an iced note).
pub fn apply_target(
    fs: &dyn Fs,
    paths: &Paths,
    clock: &dyn Clock,
    id_input: &str,
    target: SetTarget,
    force: bool,
) -> Result<(CardId, String)> {
    let board_text = fs
        .read(&paths.board_md())
        .with_context(|| format!("read {}", paths.board_md().display()))?;
    let mut board = Board::parse(&board_text);
    let notes = layout::load_notes(fs, paths)?;

    // Resolve against every known id — board cards and notes alike — so an iced
    // note (no board card) still resolves for a promote or a cancel.
    let mut known = super::board_ids(&board);
    known.extend(notes.iter().filter_map(|e| e.id()));
    let id = layout::resolve_id(id_input, &known)?;
    let on_board = board.locate(&id).is_some();

    match target {
        // Shelve a board card back to the icebox: remove the card, keep the note.
        SetTarget::Icebox => {
            if !on_board {
                return Ok((id, "icebox".to_string())); // already iced — a no-op
            }
            let from = current_status(&board, &notes, &id)?;
            if from != Status::Todo && !force {
                bail!("shelving {id} from '{from}' needs --force (a clean shelve is from To-do)");
            }
            board.remove_card(&id);
            fs.write(&paths.board_md(), &board.to_text())?;
            Ok((id, from.to_string()))
        }
        SetTarget::Status(to) => {
            if on_board {
                // Ordinary board transition — the state machine in `apply`.
                let (id, from) = apply(fs, paths, clock, id.as_str(), to, force)?;
                return Ok((id, from.to_string()));
            }
            // Promote or cancel out of the icebox. Only `cancelled` shelves an
            // iced note straight to the archive; `accepted` is not a promotion
            // target (you don't accept work that was never done), so it falls to
            // the non-todo guard below and is rejected without --force.
            if to == Status::Cancelled {
                // Build the archive mutation in memory first, so a corrupt board
                // (missing lane) fails before any write. Then stamp the note (the
                // first persistent write), then the board: a partial run leaves
                // the note iced and repeatable, never a board tombstone with no
                // disposition.
                if !board.insert_card(Status::Todo, Card::new_link(&id), true)
                    || !board.move_card(&id, to)
                {
                    bail!("cannot archive {id}: the board is missing a lane; run `project lint`");
                }
                update_note(fs, paths, clock, &notes, &id, Some(to))?;
                fs.write(&paths.board_md(), &board.to_text())?;
                return Ok((id, "icebox".to_string()));
            }
            if to != Status::Todo && !force {
                bail!("a promotion lands in To-do; use --force to enter '{to}' instead");
            }
            // Enter the pipeline at the front of the target lane.
            if !board.insert_card(to, Card::new_link(&id), true) {
                bail!("board has no '{to}' lane; run `project lint`");
            }
            fs.write(&paths.board_md(), &board.to_text())?;
            Ok((id, "icebox".to_string()))
        }
    }
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
            SetTarget::Status(Status::InProgress),
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
            SetTarget::Status(Status::Accepted),
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
            SetTarget::Status(Status::Accepted),
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
            run(
                &fs,
                &paths,
                &r,
                &FixedClock(T0),
                "a3f7b2",
                SetTarget::Status(to),
                false,
            )
            .unwrap();
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
            SetTarget::Status(Status::Cancelled),
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
            SetTarget::Status(Status::InProgress),
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
                run(
                    &fs,
                    &paths,
                    &r,
                    &FixedClock(T0),
                    hex,
                    SetTarget::Status(to),
                    false,
                )
                .unwrap();
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
                run(&fs, &paths, &r, &clock, hex, SetTarget::Status(to), false).unwrap();
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
            SetTarget::Status(Status::Accepted),
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
            SetTarget::Status(Status::InProgress),
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

    // ---- The icebox edges (design/PDD001) -----------------------------------

    /// A board with no cards and one iced note (`a3f7b2`, no board card).
    fn iced() -> (FakeFs, Paths) {
        let board = Board::parse(&layout::initial_board());
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &board.to_text())
            .with_file(
                "/b/issues/a3f7b2.md",
                "---\nid: a3f7b2\ntitle: Iced idea\ntype: feature\nblocked_by: []\ncreated: 2026-01-01T00:00:00Z\n---\n\nbody\n",
            );
        (fs, Paths::new("/b"))
    }

    fn a3f7b2() -> CardId {
        CardId::parse("a3f7b2").unwrap()
    }

    #[test]
    fn it_promotes_an_iced_note_into_todo() {
        let (fs, paths) = iced();
        let r = Renderer::new(false, false);
        run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            SetTarget::Status(Status::Todo),
            false,
        )
        .unwrap();
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        // A promotion enters the pipeline at the front of To-do.
        assert_eq!(board.cards_in(Status::Todo)[0].id().unwrap(), a3f7b2());
    }

    #[test]
    fn it_rejects_a_promotion_to_a_non_todo_lane_without_force() {
        let (fs, paths) = iced();
        let r = Renderer::new(false, false);
        assert!(run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            SetTarget::Status(Status::InProgress),
            false
        )
        .is_err());
        // --force lands the promotion in the requested lane.
        assert!(run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            SetTarget::Status(Status::InProgress),
            true
        )
        .is_ok());
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert_eq!(board.status_of(&a3f7b2()), Some(Status::InProgress));
    }

    #[test]
    fn it_shelves_a_todo_card_back_to_the_icebox() {
        let (fs, paths) = seeded(); // a3f7b2 in To-do
        let r = Renderer::new(false, false);
        run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            SetTarget::Icebox,
            false,
        )
        .unwrap();
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        // No board card, but the note survives.
        assert_eq!(board.locate(&a3f7b2()), None);
        assert!(fs.get("/b/issues/a3f7b2.md").is_some());
    }

    #[test]
    fn it_rejects_a_shelve_from_an_active_lane_without_force() {
        let (fs, paths) = seeded();
        let r = Renderer::new(false, false);
        run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            SetTarget::Status(Status::InProgress),
            false,
        )
        .unwrap();
        // Shelving from an active lane needs --force.
        assert!(run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            SetTarget::Icebox,
            false
        )
        .is_err());
        assert!(run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            SetTarget::Icebox,
            true
        )
        .is_ok());
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert_eq!(board.locate(&a3f7b2()), None);
    }

    #[test]
    fn it_cancels_an_iced_note_straight_into_the_archive() {
        let (fs, paths) = iced();
        let r = Renderer::new(false, false);
        run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            SetTarget::Status(Status::Cancelled),
            false,
        )
        .unwrap();
        // The note is tombstoned...
        let meta =
            NoteMeta::from_note(&Note::parse(&fs.get("/b/issues/a3f7b2.md").unwrap()).unwrap())
                .unwrap();
        assert_eq!(meta.disposition, Some(Status::Cancelled));
        assert!(meta.disposition_at.is_some());
        // ...and lands directly in the archive as a `- [x]` card, with no stop
        // in an active lane.
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert_eq!(board.archived().len(), 1);
        assert_eq!(board.archived()[0].id().unwrap(), a3f7b2());
        assert!(board.archived()[0].is_checked());
        assert_eq!(board.status_of(&a3f7b2()), None);
    }

    #[test]
    fn it_does_not_archive_an_iced_note_as_accepted_without_force() {
        // Only `cancelled` shelves an iced note to the archive; `accepted` is not
        // a promotion target and must be rejected (not silently archived).
        let (fs, paths) = iced();
        let r = Renderer::new(false, false);
        assert!(run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            SetTarget::Status(Status::Accepted),
            false
        )
        .is_err());
        // The note was not tombstoned and stays iced.
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert_eq!(board.archived().len(), 0);
        assert_eq!(board.locate(&a3f7b2()), None);
        let meta =
            NoteMeta::from_note(&Note::parse(&fs.get("/b/issues/a3f7b2.md").unwrap()).unwrap())
                .unwrap();
        assert_eq!(meta.disposition, None);
    }

    #[test]
    fn it_is_a_noop_to_shelve_an_already_iced_note() {
        let (fs, paths) = iced();
        let r = Renderer::new(false, false);
        let before = fs.get("/b/BOARD.md").unwrap();
        run(
            &fs,
            &paths,
            &r,
            &FixedClock(T0),
            "a3f7b2",
            SetTarget::Icebox,
            false,
        )
        .unwrap();
        assert_eq!(fs.get("/b/BOARD.md").unwrap(), before);
    }
}
