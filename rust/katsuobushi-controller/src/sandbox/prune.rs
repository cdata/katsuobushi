//! `katsuctl sandbox prune` — reap vestigial git refs and state dirs for
//! instances whose project-board card has reached a terminal status
//! (`accepted` or `cancelled`).
//!
//! Dispatched card instances are named `card-<id>` by `sandbox dispatch`. For
//! each such instance, the prune reads the project board (read-only, the same
//! one-way seam `dispatch` uses) and, when the card is in the archive (i.e.,
//! terminal state), removes:
//!
//! - The fetched work-product ref:
//!   `refs/remotes/sandbox-guest/card-<id>`
//! - The instance state dir: `<stateGlob>/card-<id>/` (contains `sync.git/`,
//!   `instance.json`, and disk images)
//!
//! **Safety rule**: only cards explicitly in the archive (accepted or cancelled)
//! are pruned. Cards not found on the board at all (icebox, or a manually-named
//! instance that happens to look like `card-<id>`) are skipped — not provably
//! terminal.
//!
//! **jj op-log pruning** (`jj abandon`, `jj util gc`) is explicitly out of
//! scope here: reaping git refs and state dirs is low-risk; touching the op log
//! is not, and that cleanup belongs to a later card (9344ec).

use std::io;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::output::Renderer;
use crate::project::board::{Board, Location};
use crate::project::model::CardId;
use crate::sandbox::host::{Host, HostImpl};
use crate::sandbox::resolve::list_instances;
use crate::sandbox::spec::{load_spec, resolve_roots, Spec};
use crate::Global;

/// The remote-bookmark namespace for fetched sandbox refs (mirrors `fetch.rs`).
const SANDBOX_GUEST_REMOTE: &str = "sandbox-guest";

/// Instance name prefix stamped by `sandbox dispatch` on card instances.
const CARD_PREFIX: &str = "card-";

/// One successfully pruned instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Pruned {
    instance: String,
    /// Whether `refs/remotes/sandbox-guest/<inst>` was found and deleted.
    ref_deleted: bool,
}

/// Production entry point: load the spec, stand up the real host seam, read
/// the board read-only, and prune terminal-card instances.
pub fn run(config: &Path, board_dir: &Path, global: Global) -> Result<()> {
    let spec = load_spec(config)?;
    let host = HostImpl::new().context("initializing the host IO seam")?;

    let board_path = board_dir.join("BOARD.md");
    let board_text = std::fs::read_to_string(&board_path)
        .with_context(|| format!("reading board at {}", board_path.display()))?;
    let board = Board::parse(&board_text);

    let pruned = prune_with(&host, &spec, &board, remove_tree)?;

    let renderer = Renderer::resolve(global);
    renderer.emit(&pruned, |_| {
        if pruned.is_empty() {
            "nothing to prune".to_string()
        } else {
            let mut lines: Vec<String> = pruned
                .iter()
                .map(|p| format!("pruned {}", p.instance))
                .collect();
            lines.push(format!("pruned {} instance(s)", pruned.len()));
            lines.join("\n")
        }
    })
}

/// The testable core: enumerate dispatched-card instances, reconcile against
/// the board, and prune those in terminal state.
///
/// - Only `card-<id>` instances are considered; non-card instances are ignored.
/// - Only cards in the board archive (terminal: accepted/cancelled) are pruned.
/// - A card absent from the board at all is skipped — not provably terminal.
///
/// For each pruned instance:
/// 1. `git update-ref -d refs/remotes/sandbox-guest/<inst>` is run through the
///    host seam. A non-zero exit means the ref was already absent — treated as
///    success, `ref_deleted` is reported `false`.
/// 2. `remove_dir` removes the state dir (contains `sync.git/`, disk images).
fn prune_with(
    host: &impl Host,
    spec: &Spec,
    board: &Board,
    mut remove_dir: impl FnMut(&Path) -> Result<()>,
) -> Result<Vec<Pruned>> {
    let roots = resolve_roots(&spec.roots)?;
    let instances = list_instances(&roots.state_glob, host)?;

    let mut pruned = Vec::new();
    for inst in instances {
        // Only dispatched card instances carry the `card-` prefix.
        let Some(id_str) = inst.strip_prefix(CARD_PREFIX) else {
            continue;
        };
        let Some(card_id) = CardId::parse(id_str) else {
            continue; // not a valid 6-hex card id suffix
        };
        // Prune only when the card is explicitly terminal (in the archive).
        // A card absent from the board entirely is not provably terminal — skip.
        if board.locate(&card_id) != Some(Location::Archive) {
            continue;
        }

        // Delete the fetched ref (idempotent: non-zero = already absent).
        let ref_path = format!("refs/remotes/{SANDBOX_GUEST_REMOTE}/{inst}");
        let mut cmd = Command::new(&spec.tools.git);
        cmd.args(["update-ref", "-d", &ref_path]);
        let ref_deleted = host.run(&cmd).is_ok_and(|o| o.status.success());

        // Remove the state dir (sync.git, instance.json, disk images).
        remove_dir(&roots.state_glob.join(&inst))?;

        pruned.push(Pruned {
            instance: inst,
            ref_deleted,
        });
    }
    Ok(pruned)
}

/// Recursively remove `dir`, treating an already-absent path as success so
/// prune is idempotent when partially applied.
fn remove_tree(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", dir.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host::{Call, FakeHost};
    use crate::sandbox::spec::{GraphicsSpec, Roots, Tools};
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::{ExitStatus, Output};

    fn exit(code: i32) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    fn fake_spec(state: &str) -> Spec {
        Spec {
            spec_version: 2,
            project_id: "cdata/katsuobushi".into(),
            agent_user: "agent".into(),
            import_host_store_db: false,
            roots: Roots {
                state_glob: PathBuf::from(state),
                runtime_glob: PathBuf::from("/run/katsuobushi"),
            },
            tools: Tools {
                git: PathBuf::from("/nix/store/h1-git/bin/git"),
                ssh: PathBuf::from("/bin/ssh"),
                ssh_keygen: PathBuf::from("/bin/ssh-keygen"),
                tmux: PathBuf::from("/bin/tmux"),
                rsync: PathBuf::from("/bin/rsync"),
                sqlite3: None,
                bash: PathBuf::from("/bin/bash"),
                katsuctl: PathBuf::from("/bin/katsuctl"),
            },
            runner: PathBuf::from("/bin/microvm-run"),
            disk_images: vec![],
            context: vec![],
            secrets: vec![],
            vsock_port: 1024,
            host_cid: 2,
            heartbeat_secs: 10,
            heartbeat_miss: 3,
            delivery_deadline_secs: 20,
            delivery_retries: 3,
            ready_gate_secs: 60,
            stop_grace_ms: 1500,
            graphics: GraphicsSpec::default(),
        }
    }

    /// A board with `aaaaaa` in To-do (live) and `bbbbbb` in the archive
    /// (terminal).
    fn mixed_board() -> Board {
        Board::parse(
            "---\nkanban-plugin: basic\n---\n\n\
             ## To-do\n\n- [ ] [[aaaaaa]]\n\n\
             ## In Progress\n\n\
             ## Needs Review\n\n\
             ## Ready\n\n\
             ---\n\n\
             ## Archive\n\n- [x] [[bbbbbb]]\n\n",
        )
    }

    fn entries(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn it_skips_instances_without_the_card_prefix() {
        // Non-card-prefixed instances (user-started) must never be pruned.
        let spec = fake_spec("/state");
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["named-foo", "interactive-bar"])));

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert!(pruned.is_empty(), "non-card instances must not be pruned");
        assert!(removed.into_inner().is_empty());
        assert!(!host.calls().iter().any(|c| matches!(c, Call::Run(_))));
    }

    #[test]
    fn it_skips_instances_with_an_invalid_card_id_suffix() {
        // `card-xyz` and `card-tooshort` are not valid 6-hex ids.
        let spec = fake_spec("/state");
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["card-xyz", "card-tooshort", "card-"])));

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert!(pruned.is_empty());
        assert!(removed.into_inner().is_empty());
    }

    #[test]
    fn it_skips_a_card_in_an_active_lane() {
        // `card-aaaaaa` is in To-do (live): must not be pruned.
        let spec = fake_spec("/state");
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["card-aaaaaa"])));

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert!(pruned.is_empty(), "a live-lane card must not be pruned");
        assert!(removed.into_inner().is_empty());
        assert!(!host.calls().iter().any(|c| matches!(c, Call::Run(_))));
    }

    #[test]
    fn it_skips_a_card_not_on_the_board_at_all() {
        // `card-cccccc` is not mentioned anywhere on the board (not provably
        // terminal).
        let spec = fake_spec("/state");
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["card-cccccc"])));

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert!(
            pruned.is_empty(),
            "a card absent from the board must not be pruned"
        );
        assert!(removed.into_inner().is_empty());
    }

    #[test]
    fn it_prunes_an_archived_card_deleting_the_ref_and_state_dir() {
        // `card-bbbbbb` is in the archive → prune it.
        let state = "/state";
        let spec = fake_spec(state);
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["card-bbbbbb"])));
        host.push_run(Ok(exit(0))); // git update-ref -d: ref present, deleted

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].instance, "card-bbbbbb");
        assert!(pruned[0].ref_deleted);
        assert_eq!(
            removed.into_inner(),
            vec![PathBuf::from(state).join("card-bbbbbb")]
        );
    }

    #[test]
    fn it_runs_the_correct_git_update_ref_invocation() {
        let state = "/state";
        let spec = fake_spec(state);
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["card-bbbbbb"])));
        host.push_run(Ok(exit(0)));

        prune_with(&host, &spec, &board, |_| Ok(())).unwrap();

        let runs: Vec<_> = host
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Run(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0],
            vec![
                "/nix/store/h1-git/bin/git".to_string(),
                "update-ref".to_string(),
                "-d".to_string(),
                "refs/remotes/sandbox-guest/card-bbbbbb".to_string(),
            ]
        );
    }

    #[test]
    fn it_still_removes_the_state_dir_when_the_git_ref_is_already_absent() {
        // `git update-ref -d` exits nonzero when the ref doesn't exist.  The
        // prune must still remove the state dir — a missing ref just means the
        // instance was never fetched (or the ref was already cleaned up).
        let state = "/state";
        let spec = fake_spec(state);
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["card-bbbbbb"])));
        host.push_run(Ok(exit(1))); // git update-ref -d: ref not present

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert_eq!(pruned.len(), 1);
        assert!(!pruned[0].ref_deleted, "ref was already absent");
        assert!(
            !removed.into_inner().is_empty(),
            "state dir must still be removed"
        );
    }

    #[test]
    fn it_prunes_only_terminal_cards_from_a_mixed_listing() {
        // Mixed: aaaaaa (live/To-do), bbbbbb (archived/terminal), named-inst
        // (no card prefix). Only bbbbbb is pruned.
        let state = "/state";
        let spec = fake_spec(state);
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["card-aaaaaa", "card-bbbbbb", "named-inst"])));
        host.push_run(Ok(exit(0))); // git for card-bbbbbb only

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert_eq!(pruned.len(), 1, "only the archived card is pruned");
        assert_eq!(pruned[0].instance, "card-bbbbbb");
        assert_eq!(
            removed.into_inner(),
            vec![PathBuf::from(state).join("card-bbbbbb")]
        );
    }

    #[test]
    fn it_prunes_multiple_terminal_cards_in_one_pass() {
        // Two archived cards: both should be pruned in the same run.
        let state = "/state";
        let board = Board::parse(
            "---\nkanban-plugin: basic\n---\n\n\
             ## To-do\n\n## In Progress\n\n## Needs Review\n\n## Ready\n\n\
             ---\n\n## Archive\n\n- [x] [[aaaaaa]]\n- [x] [[bbbbbb]]\n\n",
        );
        let spec = fake_spec(state);
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["card-aaaaaa", "card-bbbbbb"])));
        host.push_run(Ok(exit(0))); // git for card-aaaaaa
        host.push_run(Ok(exit(0))); // git for card-bbbbbb

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert_eq!(pruned.len(), 2);
        let names: Vec<_> = pruned.iter().map(|p| p.instance.as_str()).collect();
        assert!(names.contains(&"card-aaaaaa") && names.contains(&"card-bbbbbb"));
        assert_eq!(removed.into_inner().len(), 2);
    }

    #[test]
    fn it_serializes_a_pruned_instance_as_json() {
        let p = Pruned {
            instance: "card-bbbbbb".into(),
            ref_deleted: true,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            json.contains("\"instance\":\"card-bbbbbb\""),
            "json: {json}"
        );
        assert!(json.contains("\"ref_deleted\":true"), "json: {json}");
    }
}
