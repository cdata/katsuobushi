//! `katsuctl sandbox stop` — power an instance off over native QMP, then
//! conditionally remove its state/runtime dirs.
//!
//! Replaces the shell at (the `sandbox:stop` command):
//! the empty-instance guard (a destructive `rm` must never expand to the whole
//! state root), the `qmp_capabilities`+`quit` handshake, and the
//! named-vs-ephemeral removal policy.
//!
//! The world-touching pieces are injected so the policy core is exercisable
//! against a [`FakeHost`](crate::sandbox::host::FakeHost) without a VM or a real
//! filesystem: instance resolution + the QMP-socket probe go through the host
//! seam, while the `named` lookup and the recursive removal are passed in as
//! closures (production reads `instance.json` and calls `std::fs`, tests record).

use std::io;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::sandbox::host::{Host, HostImpl};
use crate::sandbox::instance;
use crate::sandbox::output::Renderer;
use crate::sandbox::qmp;
use crate::sandbox::resolve::resolve_instance;
use crate::sandbox::spec::{load_spec, resolve_roots};
use crate::Global;

/// The outcome of a stop: the resolved instance and whether its dirs were
/// removed. Serializes as the `--json` body `{"instance":...,"removed":...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Stopped {
    instance: String,
    removed: bool,
}

impl Stopped {
    /// The human confirmation line. A removed instance is gone; a kept (named)
    /// one prints the restart/discard hints, mirroring the shell's message.
    fn human(&self) -> String {
        if self.removed {
            format!("stopped and removed {}", self.instance)
        } else {
            format!(
                "stopped {0} (named; kept — restart: sandbox:start --name {0}, \
                 discard: sandbox:stop --remove {0})",
                self.instance
            )
        }
    }
}

/// Production entry point: load the spec, stand up the real host seam, and drive
/// [`stop_core`] with the real `instance.json` read and a `std::fs` remover.
pub fn run(config: &Path, remove: bool, instance: &str, global: Global) -> Result<()> {
    let spec = load_spec(config)?;
    let roots = resolve_roots(&spec.roots)?;
    let host = HostImpl::new().context("initializing the host IO seam")?;

    let state_glob = roots.state_glob.as_path();
    let runtime_glob = roots.runtime_glob.as_path();

    let stopped = stop_core(
        &host,
        state_glob,
        runtime_glob,
        remove,
        instance,
        // A missing/unreadable `instance.json` means no persistence marker, so
        // treat it as ephemeral (mirrors the shell's absent `.named` → remove).
        |inst| {
            Ok(instance::read(state_glob, inst)
                .map(|i| i.named)
                .unwrap_or(false))
        },
        quit_and_confirm,
        remove_tree,
    )?;

    Renderer::resolve(global).emit(&stopped, |_| stopped.human())
}

/// The testable core: guard, resolve, QMP-quit + confirm death, then apply the
/// removal policy.
///
/// `read_named` learns whether the instance is persistent (`instance.json`'s
/// `named`); `shutdown` quits the VM and reports whether the monitor went away;
/// `remove_dir` removes a directory tree. All are injected so a FakeHost test
/// drives the whole flow without a VM or real filesystem.
#[allow(clippy::too_many_arguments)]
fn stop_core(
    host: &impl Host,
    state_glob: &Path,
    runtime_glob: &Path,
    remove: bool,
    instance: &str,
    read_named: impl FnOnce(&str) -> Result<bool>,
    shutdown: impl FnOnce(&Path) -> bool,
    mut remove_dir: impl FnMut(&Path) -> Result<()>,
) -> Result<Stopped> {
    // HARD-GUARD: an empty selector would let the `rm` paths below collapse onto
    // the whole state/runtime root. Refuse it before touching anything.
    if instance.is_empty() {
        bail!("usage: sandbox stop [--remove] <instance|#>");
    }
    let inst = resolve_instance(state_glob, host, instance)?;
    // Defensive second guard, beyond resolve_instance's own check: never let a
    // resolved-empty name reach the removal paths (destructive-command paranoia).
    if inst.is_empty() {
        bail!("refusing to stop an empty instance name");
    }

    // Power the VM off via the qemu monitor and confirm it actually died. A
    // missing socket means the instance is already stopped — tolerate it. But a
    // monitor that keeps answering after `quit` means qemu is still running:
    // removing its dirs then would delete the disk images out from under a live
    // VM (which keeps running on the unlinked fds) while reporting success — so
    // refuse and leave everything in place instead.
    let sock = runtime_glob.join(&inst).join("katsuobushi.sock");
    if host.exists(&sock) && !shutdown(&sock) {
        bail!(
            "instance '{inst}' is still running: its QMP monitor kept answering \
             after `quit`. Nothing was removed — retry, or inspect the qemu \
             process before discarding its state."
        );
    }

    // The launching process tears down its own instance on exit, but a stop from
    // elsewhere (or after that process is gone) must do it too. Ephemeral
    // instances are always removed; named ones are kept unless `--remove` is
    // given to discard them.
    let named = read_named(&inst)?;
    let removed = remove || !named;
    if removed {
        // Attempt BOTH removals before surfacing any error: short-circuiting on
        // the first would strand the other dir (a half-torn-down instance whose
        // leftover socket/key confuse a later stop). Runtime (the volatile
        // socket + ssh key) goes first.
        let runtime_result = remove_dir(&runtime_glob.join(&inst));
        let state_result = remove_dir(&state_glob.join(&inst));
        runtime_result?;
        state_result?;
    }

    Ok(Stopped {
        instance: inst,
        removed,
    })
}

/// Production shutdown: send the QMP quit handshake, then poll the monitor
/// until it stops answering. Returns whether the VM is confirmed down.
///
/// `quit`'s own error is deliberately not propagated: a stale socket file with
/// no listener refuses the connect, and that *is* the already-dead case — the
/// `is_alive` poll below is what distinguishes dead (proceed) from a monitor
/// that keeps answering (refuse). qemu normally exits within a moment of
/// processing `quit`; the poll only bounds a wedged monitor.
fn quit_and_confirm(sock: &Path) -> bool {
    let _ = qmp::quit(sock);
    for _ in 0..10 {
        if !qmp::is_alive(sock) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    false
}

/// Recursively remove `dir`, treating an already-absent path as success — a stop
/// is idempotent cleanup, so a half-torn-down instance must not error.
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
    use crate::sandbox::host::FakeHost;
    use std::cell::RefCell;
    use std::path::PathBuf;

    const STATE: &str = "/state/cdata/katsuobushi";
    const RUNTIME: &str = "/run/cdata/katsuobushi";

    /// A FakeHost where the literal instance's state dir exists, so
    /// `resolve_instance` accepts the name. The QMP socket is left absent, so the
    /// quit handshake is skipped (the policy is what these tests exercise).
    fn host_with_instance(inst: &str) -> FakeHost {
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(STATE).join(inst));
        host
    }

    /// Run `stop_core` with a recording remover, the given `named`, and a
    /// shutdown that always confirms death, returning the outcome and the
    /// directories the remover was asked to delete.
    fn run_stop(
        host: &FakeHost,
        remove: bool,
        instance: &str,
        named: bool,
    ) -> (Result<Stopped>, Vec<PathBuf>) {
        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let outcome = stop_core(
            host,
            Path::new(STATE),
            Path::new(RUNTIME),
            remove,
            instance,
            |_| Ok(named),
            |_| true,
            |p| {
                removed.borrow_mut().push(p.to_path_buf());
                Ok(())
            },
        );
        (outcome, removed.into_inner())
    }

    #[test]
    fn it_removes_an_ephemeral_instance() {
        let host = host_with_instance("inst-eph");
        let (outcome, removed) = run_stop(&host, false, "inst-eph", false);

        let stopped = outcome.expect("ephemeral stop should succeed");
        assert!(stopped.removed, "ephemeral instances are always removed");
        assert_eq!(
            removed,
            vec![
                PathBuf::from(RUNTIME).join("inst-eph"),
                PathBuf::from(STATE).join("inst-eph"),
            ],
            "both the runtime and state dirs are removed"
        );
    }

    #[test]
    fn it_keeps_a_named_instance_without_remove() {
        let host = host_with_instance("inst-named");
        let (outcome, removed) = run_stop(&host, false, "inst-named", true);

        let stopped = outcome.expect("named stop should succeed");
        assert!(!stopped.removed, "a named instance is kept, not removed");
        assert!(removed.is_empty(), "nothing is removed for a kept instance");
        // The human line carries the restart + discard hints.
        let line = stopped.human();
        assert!(
            line.contains("restart: sandbox:start --name inst-named"),
            "{line}"
        );
        assert!(
            line.contains("discard: sandbox:stop --remove inst-named"),
            "{line}"
        );
    }

    #[test]
    fn it_removes_a_named_instance_with_remove() {
        let host = host_with_instance("inst-named");
        let (outcome, removed) = run_stop(&host, true, "inst-named", true);

        let stopped = outcome.expect("named --remove stop should succeed");
        assert!(stopped.removed, "--remove discards even a named instance");
        assert_eq!(
            removed,
            vec![
                PathBuf::from(RUNTIME).join("inst-named"),
                PathBuf::from(STATE).join("inst-named"),
            ]
        );
    }

    #[test]
    fn it_refuses_an_empty_selector_and_removes_nothing() {
        let host = FakeHost::new();
        let (outcome, removed) = run_stop(&host, true, "", false);

        let err = outcome.expect_err("an empty selector must be refused");
        assert!(format!("{err:#}").contains("usage"), "{err:#}");
        assert!(
            removed.is_empty(),
            "the hard guard must fire before any removal"
        );
        // Nothing was probed either — the guard bails before resolution.
        assert!(
            host.calls().is_empty(),
            "no seam interaction before the guard"
        );
    }

    #[test]
    fn it_quits_qmp_only_when_the_socket_exists() {
        // With the socket marked present, the core probes it (and would quit).
        let mut host = host_with_instance("inst-eph");
        let sock = PathBuf::from(RUNTIME)
            .join("inst-eph")
            .join("katsuobushi.sock");
        host.with_existing(sock.clone());

        let (outcome, _removed) = run_stop(&host, false, "inst-eph", false);
        outcome.expect("stop should succeed");

        use crate::sandbox::host::Call;
        assert!(
            host.calls().contains(&Call::Exists(sock)),
            "the QMP socket is probed through the seam"
        );
    }

    #[test]
    fn it_refuses_removal_when_the_vm_survives_quit() {
        // Socket present, but the monitor keeps answering after `quit`: nothing
        // may be removed — deleting the disk images under a live qemu would
        // leave an untracked VM running on unlinked fds.
        let mut host = host_with_instance("inst-eph");
        host.with_existing(
            PathBuf::from(RUNTIME)
                .join("inst-eph")
                .join("katsuobushi.sock"),
        );

        let removed: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let outcome = stop_core(
            &host,
            Path::new(STATE),
            Path::new(RUNTIME),
            true,
            "inst-eph",
            |_| Ok(false),
            |_| false, // shutdown could not confirm death
            |p| {
                removed.borrow_mut().push(p.to_path_buf());
                Ok(())
            },
        );

        let err = outcome.expect_err("a surviving VM must refuse the stop");
        assert!(format!("{err:#}").contains("still running"), "{err:#}");
        assert!(
            removed.into_inner().is_empty(),
            "nothing is removed while the VM lives"
        );
    }

    #[test]
    fn it_attempts_both_removals_before_erroring() {
        // The first (runtime) removal fails; the state removal must still be
        // attempted so a partial failure does not strand a half-torn-down
        // instance — and the error still surfaces.
        let host = host_with_instance("inst-eph");
        let attempted: RefCell<Vec<PathBuf>> = RefCell::new(Vec::new());
        let outcome = stop_core(
            &host,
            Path::new(STATE),
            Path::new(RUNTIME),
            false,
            "inst-eph",
            |_| Ok(false),
            |_| true,
            |p| {
                attempted.borrow_mut().push(p.to_path_buf());
                if p.starts_with(RUNTIME) {
                    bail!("EBUSY");
                }
                Ok(())
            },
        );

        assert!(outcome.is_err(), "the removal failure must surface");
        assert_eq!(
            attempted.into_inner(),
            vec![
                PathBuf::from(RUNTIME).join("inst-eph"),
                PathBuf::from(STATE).join("inst-eph"),
            ],
            "both removals are attempted despite the first failing"
        );
    }

    #[test]
    fn it_emits_the_json_body_shape() {
        let stopped = Stopped {
            instance: "inst-eph".into(),
            removed: true,
        };
        let json = serde_json::to_string(&stopped).expect("serialize");
        assert_eq!(json, r#"{"instance":"inst-eph","removed":true}"#);
    }
}
