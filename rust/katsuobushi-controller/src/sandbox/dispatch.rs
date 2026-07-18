//! `katsuctl sandbox dispatch <card-id>` — launch an agent VM to work a project
//! card. This is the one place the `sandbox` domain reaches into `project`
//! (the `sandbox -> project` dependency, design/project.md §8):
//!
//! 1. **Guard** — refuse a card that isn't Available (To-do with every blocker
//!    at ready/accepted) unless `--force`.
//! 2. **Claim** — move it `todo -> in-progress` via the shared state-machine
//!    writer, so the board reflects that it's being worked.
//! 3. **Compose** — the directive is `[optional .dispatch-instructions.md] +
//!    the card body`. Generic sandbox working-rules come from the guest + the
//!    sandbox skill and are deliberately not restated here.
//! 4. **Launch** — hand off to the agent-start path (`start --agent --name
//!    card-<id> --prompt <directive>`), which boots the VM and streams reports.
//!
//! The report bridge (done -> fetch + needs-review, blocked -> annotate + todo)
//! is driven by the orchestrator per the `project-orchestration` skill, not
//! hardcoded into the launch (design/project.md §8.1).

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::project::fs::{Fs, RealFs};
use crate::project::layout::{self, NoteEntry, Paths};
use crate::project::model::{CardId, Status};
use crate::project::{board::Board, select, set_status};
use crate::Global;

/// Optional per-project prelude, prepended to every dispatched directive.
const INSTRUCTIONS_FILE: &str = ".dispatch-instructions.md";

pub fn run(config: &Path, card: &str, board_dir: &Path, force: bool, global: Global) -> Result<()> {
    let fs = RealFs;
    let paths = Paths::new(board_dir.to_path_buf());

    // Everything up to the launch is the testable `prepare`; the launch itself
    // emit-execs and cannot be unit-tested.
    let (id, directive) = prepare(&fs, &paths, card, force)?;

    let name = format!("card-{id}");
    eprintln!("dispatching {id} -> agent sandbox '{name}'");
    super::start::run(config, true, Some(name), Some(directive), global)
}

/// Guard → compose → claim, returning `(resolved id, directive)`. Compose runs
/// **before** the claim, so a missing note or an unresolvable card leaves the
/// board unmutated. The testable core of dispatch (`run` only adds the launch).
fn prepare(fs: &dyn Fs, paths: &Paths, card: &str, force: bool) -> Result<(CardId, String)> {
    let board_text = fs.read(&paths.board_md()).with_context(|| {
        format!(
            "read {} — is --board-dir right? (run `project init`)",
            paths.board_md().display()
        )
    })?;
    let board = Board::parse(&board_text);
    let notes = layout::load_notes(fs, paths)?;
    let id = layout::resolve_id(card, &crate::project::board_ids(&board))?;

    // 1. Guard: Available-only unless --force.
    if !force && !select::is_available(&board, &notes, &id) {
        bail!("card {id} is not Available (must be To-do with every blocker at ready/accepted); use --force to dispatch anyway");
    }

    // 2. Compose first — a missing note aborts before we mutate the board.
    let directive = compose_directive(fs, paths, &notes, &id)?;

    // 3. Claim. --force also relaxes the transition guard (e.g. re-dispatching a
    // card already in progress). The claim is a non-terminal move, so the clock
    // (which only stamps `disposition_at` on a terminal crossing) is never read.
    set_status::apply(
        fs,
        paths,
        &crate::project::clock::SystemClock,
        id.as_str(),
        Status::InProgress,
        force,
    )
    .context("claiming the card (todo -> in-progress)")?;

    Ok((id, directive))
}

/// Build the directive: `[optional instructions file] + card title + body`.
fn compose_directive(
    fs: &dyn Fs,
    paths: &Paths,
    notes: &[NoteEntry],
    id: &CardId,
) -> Result<String> {
    let entry = notes
        .iter()
        .find(|e| e.id().as_ref() == Some(id))
        .ok_or_else(|| anyhow::anyhow!("card {id} has no note file to dispatch"))?;
    let title = entry
        .meta
        .as_ref()
        .ok()
        .map(|m| m.title.clone())
        .unwrap_or_default();
    let body = entry.note.body().trim();

    let mut out = String::new();
    let instr = paths.dir().join(INSTRUCTIONS_FILE);
    if let Ok(text) = fs.read(&instr) {
        let text = text.trim();
        if !text.is_empty() {
            out.push_str(text);
            out.push_str("\n\n");
        }
    }
    out.push_str(&format!(
        "You are implementing project-board card `{id}`. Do the work described below on your branch; \
         run the project's checks; commit and push. When complete, run `report done \"<one-paragraph \
         summary of what you built and how you verified it>\"`; if you get stuck, `report blocked \"<what \
         you need>\"`.\n\n"
    ));
    out.push_str(&format!("# {title}\n\n{body}\n"));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::board::Card;
    use crate::project::fs::FakeFs;
    use crate::project::layout::initial_board;

    /// A board with: `aaaaaa` (Available, has a note), `bbbbbb` (To-do but
    /// blocked by `aaaaaa`, has a note), and `dddddd` (To-do, **no** note file).
    fn seeded() -> (FakeFs, Paths) {
        let mut board = Board::parse(&initial_board());
        for hex in ["aaaaaa", "bbbbbb", "dddddd"] {
            board.insert_card(
                Status::Todo,
                Card::new_link(&CardId::parse(hex).unwrap()),
                false,
            );
        }
        // A card already in Ready: not Available, and ready -> in-progress is an
        // illegal transition — exercises --force relaxing *both* guards.
        board.insert_card(
            Status::Ready,
            Card::new_link(&CardId::parse("cccccc").unwrap()),
            false,
        );
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &board.to_text())
            .with_file(
                "/b/issues/aaaaaa.md",
                "---\nid: aaaaaa\ntitle: Alpha\ntype: feature\nblocked_by: []\n---\n\nbuild alpha\n",
            )
            .with_file(
                "/b/issues/bbbbbb.md",
                "---\nid: bbbbbb\ntitle: Beta\ntype: feature\nblocked_by: [aaaaaa]\n---\n\nbuild beta\n",
            )
            .with_file(
                "/b/issues/cccccc.md",
                "---\nid: cccccc\ntitle: Gamma\ntype: feature\nblocked_by: []\n---\n\nbuild gamma\n",
            );
        (fs, Paths::new("/b"))
    }

    fn status_of(fs: &FakeFs, hex: &str) -> Option<Status> {
        Board::parse(&fs.get("/b/BOARD.md").unwrap()).status_of(&CardId::parse(hex).unwrap())
    }

    #[test]
    fn prepare_rejects_a_blocked_card_and_leaves_the_board_unmutated() {
        let (fs, paths) = seeded();
        assert!(prepare(&fs, &paths, "bbbbbb", false).is_err());
        assert_eq!(status_of(&fs, "bbbbbb"), Some(Status::Todo)); // not claimed
    }

    #[test]
    fn prepare_force_bypasses_the_guard_and_claims() {
        let (fs, paths) = seeded();
        let (id, directive) = prepare(&fs, &paths, "bbbbbb", true).unwrap();
        assert_eq!(id.as_str(), "bbbbbb");
        assert!(directive.contains("build beta"));
        assert_eq!(status_of(&fs, "bbbbbb"), Some(Status::InProgress));
    }

    #[test]
    fn prepare_force_relaxes_both_the_guard_and_the_transition() {
        let (fs, paths) = seeded();
        // cccccc is in Ready: not Available *and* `ready -> in-progress` is an
        // illegal transition. --force must relax both.
        let (id, _) = prepare(&fs, &paths, "cccccc", true).unwrap();
        assert_eq!(id.as_str(), "cccccc");
        assert_eq!(status_of(&fs, "cccccc"), Some(Status::InProgress));

        // Without --force the guard rejects it (not Available), board untouched.
        let (fs2, paths2) = seeded();
        assert!(prepare(&fs2, &paths2, "cccccc", false).is_err());
        assert_eq!(status_of(&fs2, "cccccc"), Some(Status::Ready));
    }

    #[test]
    fn prepare_composes_before_claiming_so_a_missing_note_leaves_the_board_unmutated() {
        let (fs, paths) = seeded();
        // dddddd is Available (To-do, no blockers) but has no note -> compose fails
        // *before* the claim, so the board must be untouched.
        assert!(prepare(&fs, &paths, "dddddd", false).is_err());
        assert_eq!(status_of(&fs, "dddddd"), Some(Status::Todo));
    }

    #[test]
    fn prepare_claims_an_available_card_and_composes_its_directive() {
        let (fs, paths) = seeded();
        let (id, directive) = prepare(&fs, &paths, "aaaaaa", false).unwrap();
        assert_eq!(id.as_str(), "aaaaaa");
        assert!(directive.contains("# Alpha"));
        assert!(directive.contains("build alpha"));
        assert_eq!(status_of(&fs, "aaaaaa"), Some(Status::InProgress));
    }

    #[test]
    fn compose_skips_an_empty_instructions_file() {
        let fs = FakeFs::new().with_file("/b/.dispatch-instructions.md", "   \n  ");
        let notes = notes_with_card();
        let d = compose_directive(
            &fs,
            &Paths::new("/b"),
            &notes,
            &CardId::parse("a3f7b2").unwrap(),
        )
        .unwrap();
        assert!(d.starts_with("You are implementing")); // nothing prepended
    }

    #[test]
    fn compose_title_falls_back_when_note_meta_fails_to_parse() {
        // Valid card filename, but frontmatter has no `id` -> NoteMeta fails.
        let fs = FakeFs::new().with_file("/b/issues/eeeeee.md", "---\ntitle: x\n---\n\nthe body\n");
        let notes = crate::project::layout::load_notes(&fs, &Paths::new("/b")).unwrap();
        let d = compose_directive(
            &fs,
            &Paths::new("/b"),
            &notes,
            &CardId::parse("eeeeee").unwrap(),
        )
        .unwrap();
        assert!(d.contains("the body")); // composed despite the meta failure
    }

    fn notes_with_card() -> Vec<NoteEntry> {
        let fs = FakeFs::new().with_file(
            "/b/issues/a3f7b2.md",
            "---\nid: a3f7b2\ntitle: Add the thing\ntype: feature\nblocked_by: []\n---\n\n## What to build\n\nThe thing.\n",
        );
        layout::load_notes(&fs, &Paths::new("/b")).unwrap()
    }

    #[test]
    fn directive_includes_title_body_and_report_protocol() {
        let fs = FakeFs::new(); // no instructions file
        let notes = notes_with_card();
        let d = compose_directive(
            &fs,
            &Paths::new("/b"),
            &notes,
            &CardId::parse("a3f7b2").unwrap(),
        )
        .unwrap();
        assert!(d.contains("# Add the thing"));
        assert!(d.contains("The thing."));
        assert!(d.contains("report done"));
        assert!(d.contains("`a3f7b2`"));
    }

    #[test]
    fn directive_prepends_the_instructions_file_when_present() {
        let fs = FakeFs::new().with_file(
            "/b/.dispatch-instructions.md",
            "PROJECT RULES: build via `test`.",
        );
        let notes = notes_with_card();
        let d = compose_directive(
            &fs,
            &Paths::new("/b"),
            &notes,
            &CardId::parse("a3f7b2").unwrap(),
        )
        .unwrap();
        assert!(d.starts_with("PROJECT RULES: build via `test`."));
        assert!(d.contains("# Add the thing"));
    }

    #[test]
    fn missing_note_is_an_error() {
        let fs = FakeFs::new();
        let notes = notes_with_card();
        assert!(compose_directive(
            &fs,
            &Paths::new("/b"),
            &notes,
            &CardId::parse("ffffff").unwrap()
        )
        .is_err());
    }
}
