//! `project new` — mint a card note and add it to To-do.

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::sandbox::output::Renderer;

use super::board::{Board, Card};
use super::clock::{format_rfc3339, Clock};
use super::fs::{Fs, Rng};
use super::layout::{self, Paths};
use super::model::{CardId, Kind, Status};
use super::note::render_new_note;

/// Parsed `new` arguments (the `--type`/`KindArg` already lowered to [`Kind`]).
pub struct NewArgs {
    pub title: String,
    pub kind: Kind,
    pub blocked_by: Vec<String>,
    pub design: Option<String>,
    pub labels: Vec<String>,
    pub top: bool,
    pub body: Option<String>,
    pub force: bool,
}

#[derive(Serialize)]
struct NewOutput {
    id: String,
    path: String,
    status: String,
}

pub fn run(
    fs: &dyn Fs,
    clock: &dyn Clock,
    rng: &mut dyn Rng,
    paths: &Paths,
    renderer: &Renderer,
    args: NewArgs,
) -> Result<()> {
    let board_text = fs.read(&paths.board_md()).with_context(|| {
        format!(
            "read {} (run `project init` first)",
            paths.board_md().display()
        )
    })?;
    let mut board = Board::parse(&board_text);

    // Every existing id, for blocker validation and collision-free minting.
    let notes = layout::load_notes(fs, paths)?;
    let mut known: Vec<CardId> = notes.iter().filter_map(|e| e.id()).collect();
    known.extend(super::board_ids(&board));

    // Validate blocked_by (full ids; existence unless --force).
    let mut blocked = Vec::new();
    for b in &args.blocked_by {
        let id = CardId::parse(b)
            .ok_or_else(|| anyhow::anyhow!("blocked-by '{b}' is not a 6-hex id"))?;
        if !args.force && !known.contains(&id) {
            bail!("blocked-by '{b}' does not exist (use --force to allow a forward reference)");
        }
        blocked.push(id);
    }

    let id = mint_unique(rng, &known);
    let created = format_rfc3339(clock.now_unix());
    let body = resolve_body(args.body)?;

    let note_text = render_new_note(
        &id,
        &args.title,
        args.kind,
        &blocked,
        args.design.as_deref(),
        &args.labels,
        &created,
        &body,
    );
    let note_path = paths.note(&id);
    fs.create_dir_all(&paths.issues_dir())?;
    fs.write(&note_path, &note_text)
        .with_context(|| format!("write {}", note_path.display()))?;

    if !board.insert_card(Status::Todo, Card::new_link(&id), args.top) {
        bail!("board has no 'To-do' lane; run `project lint`");
    }
    fs.write(&paths.board_md(), &board.to_text())?;

    let out = NewOutput {
        id: id.to_string(),
        path: note_path.display().to_string(),
        status: Status::Todo.to_string(),
    };
    renderer.emit(&out, |_| format!("{}  {}", out.id, out.path))
}

/// Resolve the body source: `Some("-")` reads stdin, `Some(text)` is literal,
/// `None` uses the template skeleton.
fn resolve_body(body: Option<String>) -> Result<String> {
    match body {
        Some(dash) if dash == "-" => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("read body from stdin")?;
            Ok(buf)
        }
        Some(text) => Ok(text),
        None => Ok(layout::card_body_template().to_string()),
    }
}

/// Mint a hex id not already present. 32 bits collide only at birthday scale
/// (~65k cards); retry on the rare clash and cap attempts defensively.
fn mint_unique(rng: &mut dyn Rng, known: &[CardId]) -> CardId {
    for _ in 0..1000 {
        let id = CardId::from_u32(rng.next_u32());
        if !known.contains(&id) {
            return id;
        }
    }
    // Astronomically unreachable; return whatever we last drew.
    CardId::from_u32(rng.next_u32())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::clock::FixedClock;
    use crate::project::fs::{FakeFs, SeqRng};
    use crate::project::layout;
    use crate::project::note::{Note, NoteMeta};

    fn board_fs() -> FakeFs {
        FakeFs::new().with_file("/b/BOARD.md", &layout::initial_board())
    }

    #[test]
    fn new_writes_note_and_card() {
        let fs = board_fs();
        let paths = Paths::new("/b");
        let r = Renderer::new(false, false);
        let mut rng = SeqRng::new(vec![0xa3f7b2]);
        let clock = FixedClock(1_784_312_524);

        run(
            &fs,
            &clock,
            &mut rng,
            &paths,
            &r,
            NewArgs {
                title: "Device identity and data root".into(),
                kind: Kind::Feature,
                blocked_by: vec![],
                design: Some("PDD005".into()),
                labels: vec!["security".into()],
                top: false,
                body: None,
                force: false,
            },
        )
        .unwrap();

        // The note exists, is typed, and stamped.
        let note_text = fs.get("/b/issues/a3f7b2.md").unwrap();
        let meta = NoteMeta::from_note(&Note::parse(&note_text).unwrap()).unwrap();
        assert_eq!(meta.id.as_str(), "a3f7b2");
        assert_eq!(meta.created.as_deref(), Some("2026-07-17T18:22:04Z"));
        assert_eq!(meta.design.as_deref(), Some("PDD005"));

        // The card is in To-do.
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert_eq!(board.status_of(&meta.id), Some(Status::Todo));
    }

    #[test]
    fn blocked_by_must_exist_unless_forced() {
        let fs = board_fs();
        let paths = Paths::new("/b");
        let r = Renderer::new(false, false);
        let mut rng = SeqRng::new(vec![0xa3f7b2]);
        let clock = FixedClock(0);

        let mk = |bl: Vec<String>, force: bool| NewArgs {
            title: "x".into(),
            kind: Kind::Bug,
            blocked_by: bl,
            design: None,
            labels: vec![],
            top: false,
            body: Some("b".into()),
            force,
        };

        // Nonexistent blocker rejected...
        assert!(run(
            &fs,
            &clock,
            &mut rng,
            &paths,
            &r,
            mk(vec!["deadbe".into()], false)
        )
        .is_err());
        // ...but allowed with --force.
        let mut rng2 = SeqRng::new(vec![0x1a2b3c]);
        assert!(run(
            &fs,
            &clock,
            &mut rng2,
            &paths,
            &r,
            mk(vec!["deadbe".into()], true)
        )
        .is_ok());
    }

    #[test]
    fn top_inserts_at_head() {
        let fs = board_fs();
        let paths = Paths::new("/b");
        let r = Renderer::new(false, false);
        let clock = FixedClock(0);

        let mut rng = SeqRng::new(vec![0xaaaaaa]);
        run(
            &fs,
            &clock,
            &mut rng,
            &paths,
            &r,
            NewArgs {
                title: "first".into(),
                kind: Kind::Chore,
                blocked_by: vec![],
                design: None,
                labels: vec![],
                top: false,
                body: Some("b".into()),
                force: false,
            },
        )
        .unwrap();
        let mut rng = SeqRng::new(vec![0xbbbbbb]);
        run(
            &fs,
            &clock,
            &mut rng,
            &paths,
            &r,
            NewArgs {
                title: "second".into(),
                kind: Kind::Chore,
                blocked_by: vec![],
                design: None,
                labels: vec![],
                top: true,
                body: Some("b".into()),
                force: false,
            },
        )
        .unwrap();

        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        let todo = board.cards_in(Status::Todo);
        assert_eq!(todo[0].id().unwrap().as_str(), "bbbbbb"); // top
        assert_eq!(todo[1].id().unwrap().as_str(), "aaaaaa");
    }
}
