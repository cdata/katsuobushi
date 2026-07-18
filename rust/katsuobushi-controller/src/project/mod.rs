//! The `project` domain: an Obsidian-Kanban-native backlog.
//!
//! BOARD.md is authoritative for lifecycle (lane) and priority (position); card
//! notes own identity/detail/dependencies. The pure cores ([`model`], [`state`],
//! [`board`], [`note`], [`clock`]) hold all the logic and are exhaustively unit
//! tested; the verb modules are thin IO glue over the [`fs::Fs`] seam.

pub mod board;
pub mod clock;
pub mod fs;
pub mod layout;
pub mod model;
pub mod note;
pub mod select;
pub mod state;

mod init;
mod lint;
mod new;
mod prioritize;
mod query;
pub mod set_status;

use anyhow::Result;

use crate::sandbox::output::{Renderer, Reported};
use crate::{Global, ProjectArgs, ProjectCommand, StatusArgs, StatusCommand};

use clock::SystemClock;
use fs::{OsRng, RealFs};
use layout::Paths;

/// Route a parsed `project` invocation to its subcommand handler, rendering any
/// expected user error through the shared layer (honoring `--json`) and handing
/// `main` a [`Reported`] so it exits nonzero without re-printing.
pub fn dispatch(args: ProjectArgs, global: Global) -> Result<()> {
    let ProjectArgs { board_dir, command } = args;
    let renderer = Renderer::resolve(global);
    let paths = Paths::new(board_dir);
    let fs = RealFs;
    let clock = SystemClock;
    let mut rng = OsRng;

    let result = match command {
        ProjectCommand::Init => init::run(&fs, &paths, &renderer),
        ProjectCommand::New {
            title,
            kind,
            blocked_by,
            design,
            labels,
            top,
            body,
            force,
        } => new::run(
            &fs,
            &clock,
            &mut rng,
            &paths,
            &renderer,
            new::NewArgs {
                title,
                kind: kind.into(),
                blocked_by,
                design,
                labels,
                top,
                body,
                force,
            },
        ),
        ProjectCommand::Status(args) => {
            let StatusArgs {
                command,
                id,
                lane,
                available,
            } = args;
            match command {
                Some(StatusCommand::Set {
                    id,
                    status,
                    accept_all,
                    force,
                }) => {
                    if accept_all {
                        set_status::accept_all(&fs, &paths, &renderer, &clock)
                    } else {
                        match (id, status) {
                            (Some(id), Some(status)) => set_status::run(
                                &fs,
                                &paths,
                                &renderer,
                                &clock,
                                &id,
                                status.into(),
                                force,
                            ),
                            _ => Err(anyhow::anyhow!(
                                "`status set` needs an id and a status, or --accept-all"
                            )),
                        }
                    }
                }
                None => query::show(
                    &fs,
                    &paths,
                    &renderer,
                    &clock,
                    id,
                    lane.map(Into::into),
                    available,
                ),
            }
        }
        ProjectCommand::Prioritize {
            id,
            top,
            bottom,
            before,
            after,
        } => prioritize::run(&fs, &paths, &renderer, &id, top, bottom, before, after),
        ProjectCommand::Lint { fix } => lint::run(&fs, &paths, &renderer, fix),
    };

    result.map_err(|e| {
        if e.is::<Reported>() {
            e
        } else {
            eprintln!("{}", renderer.render_error("project", &format!("{e:#}")));
            Reported.into()
        }
    })
}

/// The known card ids on a board (lane + archive), for id resolution.
pub(crate) fn board_ids(board: &board::Board) -> Vec<model::CardId> {
    let mut ids = Vec::new();
    for lane in board.lanes() {
        ids.extend(lane.cards.iter().filter_map(|c| c.id()));
    }
    ids.extend(board.archived().iter().filter_map(|c| c.id()));
    ids
}
