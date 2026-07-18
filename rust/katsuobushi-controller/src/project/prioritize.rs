//! `project prioritize` — reorder a card within its lane (position = priority).

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::sandbox::output::Renderer;

use super::board::{Anchor, Board};
use super::fs::Fs;
use super::layout::{self, Paths};

#[derive(Serialize)]
struct PrioritizeOutput {
    id: String,
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    fs: &dyn Fs,
    paths: &Paths,
    renderer: &Renderer,
    id_input: &str,
    top: bool,
    bottom: bool,
    before: Option<String>,
    after: Option<String>,
) -> Result<()> {
    let board_text = fs
        .read(&paths.board_md())
        .with_context(|| format!("read {}", paths.board_md().display()))?;
    let mut board = Board::parse(&board_text);
    let known = super::board_ids(&board);

    let id = layout::resolve_id(id_input, &known)?;
    let anchor = if top {
        Anchor::Top
    } else if bottom {
        Anchor::Bottom
    } else if let Some(b) = before {
        Anchor::Before(layout::resolve_id(&b, &known)?)
    } else if let Some(a) = after {
        Anchor::After(layout::resolve_id(&a, &known)?)
    } else {
        bail!("specify one of --top, --bottom, --before <id>, or --after <id>");
    };

    if !board.reorder(&id, anchor) {
        bail!("card {id} is not in an active lane (only lane cards have a priority)");
    }
    fs.write(&paths.board_md(), &board.to_text())?;

    let out = PrioritizeOutput { id: id.to_string() };
    renderer.emit(&out, |_| format!("{} reprioritized", out.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::board::Card;
    use crate::project::fs::FakeFs;
    use crate::project::model::{CardId, Status};

    fn seeded() -> (FakeFs, Paths) {
        let mut board = Board::parse(&layout::initial_board());
        for hex in ["aaaaaa", "bbbbbb", "cccccc"] {
            board.insert_card(
                Status::Todo,
                Card::new_link(&CardId::parse(hex).unwrap()),
                false,
            );
        }
        (
            FakeFs::new().with_file("/b/BOARD.md", &board.to_text()),
            Paths::new("/b"),
        )
    }

    #[test]
    fn top_and_after_reorder_within_the_lane() {
        let (fs, paths) = seeded();
        let r = Renderer::new(false, false);

        run(&fs, &paths, &r, "cccccc", true, false, None, None).unwrap();
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        assert_eq!(
            board.cards_in(Status::Todo)[0].id().unwrap().as_str(),
            "cccccc"
        );

        run(
            &fs,
            &paths,
            &r,
            "cccccc",
            false,
            false,
            None,
            Some("aaaaaa".into()),
        )
        .unwrap();
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        let order: Vec<String> = board
            .cards_in(Status::Todo)
            .iter()
            .map(|c| c.id().unwrap().to_string())
            .collect();
        assert_eq!(order, vec!["aaaaaa", "cccccc", "bbbbbb"]);
    }

    #[test]
    fn requires_an_anchor() {
        let (fs, paths) = seeded();
        let r = Renderer::new(false, false);
        assert!(run(&fs, &paths, &r, "aaaaaa", false, false, None, None).is_err());
    }
}
