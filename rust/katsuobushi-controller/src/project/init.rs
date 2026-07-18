//! `project init` — idempotent board-directory scaffold.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::sandbox::output::Renderer;

use super::fs::Fs;
use super::layout::{self, Paths};

#[derive(Serialize)]
struct InitOutput {
    board_dir: String,
    created: Vec<String>,
    existing: Vec<String>,
}

pub fn run(fs: &dyn Fs, paths: &Paths, renderer: &Renderer) -> Result<()> {
    // Creating the issues/ subdir also creates the board dir itself.
    fs.create_dir_all(&paths.issues_dir())
        .with_context(|| format!("create board dir {}", paths.dir().display()))?;

    let mut created = Vec::new();
    let mut existing = Vec::new();
    scaffold(
        fs,
        &paths.board_md(),
        &layout::initial_board(),
        &mut created,
        &mut existing,
    )?;
    scaffold(
        fs,
        &paths.readme(),
        &layout::readme(),
        &mut created,
        &mut existing,
    )?;
    scaffold(
        fs,
        &paths.template(),
        layout::card_body_template(),
        &mut created,
        &mut existing,
    )?;

    let out = InitOutput {
        board_dir: paths.dir().display().to_string(),
        created,
        existing,
    };
    renderer.emit(&out, |_| {
        let mut s = format!("initialized {}", out.board_dir);
        for f in &out.created {
            s.push_str(&format!("\n  created {f}"));
        }
        for f in &out.existing {
            s.push_str(&format!("\n  exists  {f}"));
        }
        s
    })
}

/// Write a scaffold file only if absent; record which bucket it fell into.
fn scaffold(
    fs: &dyn Fs,
    path: &Path,
    contents: &str,
    created: &mut Vec<String>,
    existing: &mut Vec<String>,
) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();
    if fs.exists(path) {
        existing.push(name);
    } else {
        fs.write(path, contents)
            .with_context(|| format!("write {}", path.display()))?;
        created.push(name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::board::Board;
    use crate::project::fs::FakeFs;

    #[test]
    fn init_scaffolds_then_is_idempotent() {
        let fs = FakeFs::new();
        let paths = Paths::new("/repo/project/kanban");
        let r = Renderer::new(false, false);

        run(&fs, &paths, &r).unwrap();
        // BOARD.md is a valid four-lane Kanban board.
        let board_text = fs.get("/repo/project/kanban/BOARD.md").unwrap();
        assert_eq!(Board::parse(&board_text).lanes().len(), 4);
        assert!(fs.get("/repo/project/kanban/README.md").is_some());
        assert!(fs.get("/repo/project/kanban/.card-template.md").is_some());

        // Second run must not clobber a modified board.
        fs.write(Path::new("/repo/project/kanban/BOARD.md"), "MODIFIED")
            .unwrap();
        run(&fs, &paths, &r).unwrap();
        assert_eq!(fs.get("/repo/project/kanban/BOARD.md").unwrap(), "MODIFIED");
    }
}
