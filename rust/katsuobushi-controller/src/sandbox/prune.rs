//! `katsuctl sandbox prune` — reap vestigial git refs and state dirs for
//! instances whose project-board card has reached a terminal status
//! (`accepted` or `cancelled`).
//!
//! Two types of per-card instance are handled:
//!
//! - **Implementor** instances, named `card-<id>` by `sandbox dispatch`.
//! - **Reviewer** instances, named `review-<id>-<8hex>` by the orchestrator
//!   (via `sandbox start --agent --name review-<id>`). The `<id>` MUST be the
//!   6-hex card-id — that naming is contractual (see the
//!   `project-orchestration` skill), and prune relies on it to map the instance
//!   back to its card. If the suffix cannot be parsed as a valid card-id, the
//!   entry is skipped.
//!
//! For each such instance, prune reads the project board (read-only, the same
//! one-way seam `dispatch` uses) and, when the card is in the archive (i.e.,
//! terminal state), removes:
//!
//! - The fetched work-product ref:
//!   `refs/remotes/sandbox-guest/<inst>` (implementor and reviewer alike)
//! - The instance state dir: `<stateGlob>/<inst>/` (contains `sync.git/`,
//!   `instance.json`, and disk images)
//!
//! **Safety rule**: only cards explicitly in the archive (accepted or cancelled)
//! are pruned. Cards not found on the board at all (icebox, or a manually-named
//! instance that happens to look like `card-<id>`) are skipped — not provably
//! terminal.
//!
//! **Known limitation — liveness gap.** `prune` does not check whether a VM is
//! still running before removing its state dir. If a card is accepted while its
//! implementor or reviewer VM is still running, the state dir (and any in-flight
//! disk images) is removed under a live QEMU process. The workflow mitigates
//! this: the orchestrator `stop`s VMs before the product owner accepts.
//! `sandbox stop` has the same gap. A liveness gate is explicitly out of scope
//! here; the mitigation is procedural.
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

/// Instance name prefix stamped by `sandbox dispatch` on implementor instances.
const CARD_PREFIX: &str = "card-";

/// Instance name prefix stamped by the orchestrator on reviewer instances.
/// The full instance name is `review-<card-id>-<8hex>`, where `<card-id>` is
/// exactly 6 lowercase hex digits. That naming is contractual (see the
/// `project-orchestration` skill) — prune relies on it to map each reviewer
/// instance back to its card without consulting any external metadata.
const REVIEW_PREFIX: &str = "review-";

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

/// Extract the card-id an instance is responsible for, or `None` if the
/// instance name does not match a known card-instance pattern.
///
/// - `card-<id>` → the implementor for card `<id>`.
/// - `review-<id>-<8hex>` → the reviewer for card `<id>`; the `<id>` must be
///   exactly 6 lowercase hex (contractual per the `project-orchestration`
///   skill). An instance name whose slug cannot be parsed this way is skipped.
fn instance_card_id(inst: &str) -> Option<CardId> {
    if let Some(id_str) = inst.strip_prefix(CARD_PREFIX) {
        return CardId::parse(id_str);
    }
    if let Some(rest) = inst.strip_prefix(REVIEW_PREFIX) {
        // rest = "<card-id>-<8hex>": at minimum 6 + 1 + 8 = 15 chars.
        let n = rest.len();
        if n < 15 {
            return None;
        }
        let bytes = rest.as_bytes();
        // Separator dash and 8 lowercase hex digits occupy the last 9 chars.
        if bytes[n - 9] != b'-' {
            return None;
        }
        if !rest[n - 8..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return None;
        }
        return CardId::parse(&rest[..n - 9]);
    }
    None
}

/// The testable core: enumerate dispatched-card instances, reconcile against
/// the board, and prune those in terminal state.
///
/// Both implementor (`card-<id>`) and reviewer (`review-<id>-<8hex>`) instances
/// are considered. For all other instance names the entry is skipped.
///
/// - Only cards in the board archive (terminal: accepted/cancelled) are pruned.
/// - A card absent from the board at all is skipped — not provably terminal.
///
/// For each pruned instance:
/// 1. `git update-ref -d refs/remotes/sandbox-guest/<inst>` is run through the
///    host seam. A non-zero exit means the ref was already absent — treated as
///    success, `ref_deleted` is reported `false`. An IO error (git failed to
///    spawn) is a system-level failure and propagates, aborting the prune so
///    both the ref and the state dir remain for the next run to retry.
/// 2. `remove_dir` removes the state dir (contains `sync.git/`, disk images).
///    An error on one entry (e.g. the path is a file, not a directory) is
///    reported to stderr and that entry is skipped; other instances continue
///    to be pruned.
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
        // Determine which card this instance belongs to (if any).
        let Some(card_id) = instance_card_id(&inst) else {
            continue;
        };
        // Prune only when the card is explicitly terminal (in the archive).
        // A card absent from the board entirely is not provably terminal — skip.
        if board.locate(&card_id) != Some(Location::Archive) {
            continue;
        }

        // Delete the fetched ref. A non-zero exit means the ref was already
        // absent (idempotent). An IO failure means git could not be spawned —
        // that is a system error, not "ref not found", and must propagate so
        // the state dir is not removed under a ref that was never deleted.
        let ref_path = format!("refs/remotes/{SANDBOX_GUEST_REMOTE}/{inst}");
        let mut cmd = Command::new(&spec.tools.git);
        cmd.args(["update-ref", "-d", &ref_path]);
        let output = host
            .run(&cmd)
            .with_context(|| format!("spawning git to delete ref {ref_path}"))?;
        let ref_deleted = output.status.success();

        // Remove the state dir (sync.git, instance.json, disk images). If the
        // path is malformed (e.g. a file rather than a directory), report the
        // error and skip this entry — one bad entry must not abort the whole run.
        if let Err(e) = remove_dir(&roots.state_glob.join(&inst)) {
            eprintln!("prune: skipping malformed state dir for {inst}: {e:#}");
            continue;
        }

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
    use std::io;
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
    fn it_skips_a_card_in_needs_review_lane() {
        // needs-review is the most dangerous near-miss — one step from the
        // archive. A card in this lane must never be pruned.
        let spec = fake_spec("/state");
        let board = Board::parse(
            "---\nkanban-plugin: basic\n---\n\n\
             ## To-do\n\n## In Progress\n\n\
             ## Needs Review\n\n- [ ] [[dddddd]]\n\n\
             ## Ready\n\n\
             ---\n\n## Archive\n\n",
        );
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["card-dddddd"])));

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert!(
            pruned.is_empty(),
            "a card in needs-review must not be pruned"
        );
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
    fn it_propagates_a_git_io_error_for_a_ref_delete_failure() {
        // An IO failure spawning git (e.g. binary not found, disk error) must
        // propagate as an error — not be silently treated as "ref already absent".
        // The state dir must NOT be removed when git fails to spawn, so both
        // the ref and the state dir remain for the next prune run to retry.
        let state = "/state";
        let spec = fake_spec(state);
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["card-bbbbbb"])));
        host.push_run(Err(io::Error::new(
            io::ErrorKind::NotFound,
            "git: no such file or directory",
        )));

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let result = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        });

        assert!(result.is_err(), "a git IO error must propagate");
        assert!(
            removed.into_inner().is_empty(),
            "state dir must not be removed when git fails to spawn"
        );
    }

    #[test]
    fn it_skips_a_malformed_state_dir_and_continues() {
        // Two archived cards. remove_dir fails for card-aaaaaa (simulates a
        // file-not-directory entry) but succeeds for card-bbbbbb. The prune
        // must continue and report card-bbbbbb as pruned despite the earlier
        // failure — one bad entry must not abort the whole run.
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
            if p.ends_with("card-aaaaaa") {
                return Err(anyhow::anyhow!("not a directory"));
            }
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert_eq!(
            pruned.len(),
            1,
            "only the successfully-removed entry is reported"
        );
        assert_eq!(pruned[0].instance, "card-bbbbbb");
        assert_eq!(
            removed.into_inner(),
            vec![PathBuf::from(state).join("card-bbbbbb")]
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
    fn it_prunes_a_reviewer_instance_for_an_archived_card() {
        // review-bbbbbb-46c73967 is the reviewer for card bbbbbb which is in the
        // archive — both the ref and the state dir must be removed.
        let state = "/state";
        let spec = fake_spec(state);
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["review-bbbbbb-46c73967"])));
        host.push_run(Ok(exit(0))); // git update-ref -d: ref deleted

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].instance, "review-bbbbbb-46c73967");
        assert!(pruned[0].ref_deleted);
        assert_eq!(
            removed.into_inner(),
            vec![PathBuf::from(state).join("review-bbbbbb-46c73967")]
        );
    }

    #[test]
    fn it_skips_a_reviewer_instance_with_a_non_card_id_slug() {
        // "review-hello-46c73967": "hello" is not a valid 6-hex card-id.
        // "review-46c73967": too short — the remainder after stripping the prefix
        // has no space for both a card-id and an 8-hex suffix.
        let spec = fake_spec("/state");
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&[
            "review-hello-46c73967",
            "review-46c73967",
            "review-",
        ])));

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert!(
            pruned.is_empty(),
            "reviewer instances with non-card-id slugs must be skipped"
        );
        assert!(removed.into_inner().is_empty());
        assert!(!host.calls().iter().any(|c| matches!(c, Call::Run(_))));
    }

    #[test]
    fn it_skips_a_reviewer_instance_whose_card_is_not_terminal() {
        // review-aaaaaa-46c73967 is the reviewer for card aaaaaa which is in
        // To-do (live) — must not be pruned.
        let spec = fake_spec("/state");
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["review-aaaaaa-46c73967"])));

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert!(
            pruned.is_empty(),
            "reviewer instance for a live card must not be pruned"
        );
        assert!(removed.into_inner().is_empty());
    }

    #[test]
    fn it_prunes_both_implementor_and_reviewer_for_the_same_archived_card() {
        // A full pair: card-bbbbbb (implementor) and review-bbbbbb-46c73967
        // (reviewer). Both must be pruned in the same pass.
        let state = "/state";
        let spec = fake_spec(state);
        let board = mixed_board();
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["card-bbbbbb", "review-bbbbbb-46c73967"])));
        host.push_run(Ok(exit(0))); // git for card-bbbbbb
        host.push_run(Ok(exit(0))); // git for review-bbbbbb-46c73967

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let pruned = prune_with(&host, &spec, &board, |p| {
            removed.borrow_mut().push(p.to_path_buf());
            Ok(())
        })
        .unwrap();

        assert_eq!(pruned.len(), 2);
        let names: Vec<_> = pruned.iter().map(|p| p.instance.as_str()).collect();
        assert!(names.contains(&"card-bbbbbb"));
        assert!(names.contains(&"review-bbbbbb-46c73967"));
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

    // Unit tests for `instance_card_id` — the name→card mapping at the heart of
    // the reviewer-instance matching.
    #[test]
    fn instance_card_id_parses_card_prefix() {
        assert_eq!(
            instance_card_id("card-a3f7b2").as_ref().map(CardId::as_str),
            Some("a3f7b2")
        );
        assert!(instance_card_id("card-").is_none());
        assert!(instance_card_id("card-xyz").is_none());
    }

    #[test]
    fn instance_card_id_parses_review_prefix_with_valid_card_id() {
        assert_eq!(
            instance_card_id("review-c72eb6-46c73967")
                .as_ref()
                .map(CardId::as_str),
            Some("c72eb6")
        );
    }

    #[test]
    fn instance_card_id_rejects_review_with_short_remainder() {
        // "review-46c73967" → rest is only 8 chars, needs at least 15.
        assert!(instance_card_id("review-46c73967").is_none());
    }

    #[test]
    fn instance_card_id_rejects_review_with_non_hex_suffix() {
        // Last 8 chars contain non-hex ('z').
        assert!(instance_card_id("review-aabbcc-zzzzzzzz").is_none());
    }

    #[test]
    fn instance_card_id_rejects_review_with_non_card_id_slug() {
        // "hello" is not 6 lowercase hex.
        assert!(instance_card_id("review-hello-46c73967").is_none());
    }

    #[test]
    fn instance_card_id_rejects_unrecognized_prefix() {
        assert!(instance_card_id("named-foo").is_none());
        assert!(instance_card_id("interactive-bar").is_none());
        assert!(instance_card_id("").is_none());
    }
}
