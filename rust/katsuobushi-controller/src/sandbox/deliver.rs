//! `katsuctl sandbox deliver` — push a branch from the host repo into another
//! instance's mirror. The opposite direction of `sandbox fetch`.
//!
//! `sandbox fetch` pulls a branch from a guest mirror into the orchestrator's
//! repo. `sandbox deliver` pushes the other way: from any git ref in the host
//! repo into a target instance's `sync.git` mirror at
//! `refs/heads/delivered/<basename>`, where `<basename>` is the last
//! `/`-delimited segment of the source ref. The `delivered/` prefix never
//! collides with the target guest's own working branch (`sandbox/<inst>`), so
//! a delivery cannot overwrite what the target is working on.
//!
//! The push is always force so re-delivery is idempotent.
//!
//! Primary callers:
//! - **Peer review**: deliver a fetched branch (`sandbox-guest/<src>`) into a
//!   reviewer instance's mirror so it can examine a colleague's work. A guest
//!   sees only its own host directory, so the orchestrator is the only path
//!   between two parties.
//! - **Base refresh**: push the current tip into a resumed instance's mirror so
//!   it starts from the latest state.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::sandbox::host::{self, Host, HostImpl};
use crate::sandbox::resolve::resolve_instance;
use crate::sandbox::spec::{load_spec, resolve_roots, Spec};
use crate::Global;

/// Production entry point: load the spec, stand up the real host seam, deliver.
pub fn run(config: &Path, instance: &str, branch: &str, global: Global) -> Result<()> {
    let spec = load_spec(config)?;
    let host = HostImpl::new().context("initializing the host IO seam")?;
    let line = deliver_with(&host, &spec, instance, branch, global.json)?;
    println!("{line}");
    Ok(())
}

/// The testable core: resolve the instance, run the pinned `git push` through
/// the seam, and return the line to print (machine-readable when `json`).
///
/// Pushes `branch` (any git ref in the host repo) into the target instance's
/// mirror at `<stateGlob>/<inst>/sync.git` as `refs/heads/delivered/<basename>`,
/// where `<basename>` is the last `/`-delimited segment of `branch`. Examples:
///
/// - `refs/remotes/sandbox-guest/card-abc` → `refs/heads/delivered/card-abc`
/// - `sandbox-guest/card-abc` → `refs/heads/delivered/card-abc`
/// - `main` → `refs/heads/delivered/main`
///
/// The `delivered/` prefix guarantees the delivered ref never collides with the
/// target guest's own `sandbox/<inst>` working branch.
fn deliver_with(
    host: &impl Host,
    spec: &Spec,
    instance: &str,
    branch: &str,
    json: bool,
) -> Result<String> {
    let roots = resolve_roots(&spec.roots)?;
    let inst = resolve_instance(&roots.state_glob, host, instance)?;

    let sync_git = roots.state_glob.join(&inst).join("sync.git");

    // Derive the target ref name from the last path component of the source ref.
    let basename = branch.rsplit('/').next().unwrap_or(branch);
    if basename.is_empty() {
        bail!(
            "branch argument {branch:?} has an empty final component; \
             pass a non-empty ref name"
        );
    }

    // Target ref under delivered/ — never collides with the guest's own
    // sandbox/<inst> working branch (different prefix by construction).
    let target_ref = format!("refs/heads/delivered/{basename}");
    let refspec = format!("+{branch}:{target_ref}");

    let mut cmd = Command::new(&spec.tools.git);
    cmd.arg("push").arg(&sync_git).arg(&refspec);

    host::run_ok(host, &cmd, &format!("git push to {inst}"))?;

    Ok(match json {
        true => {
            format!(r#"{{"delivered":"{branch}","instance":"{inst}","as":"delivered/{basename}"}}"#)
        }
        false => format!("delivered {branch} into {inst} as delivered/{basename}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host::{Call, FakeHost};
    use crate::sandbox::spec::{Roots, Tools};
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::{ExitStatus, Output};

    fn fake_spec(state_glob: &str, git: &str) -> Spec {
        Spec {
            spec_version: 2,
            project_id: "cdata/katsuobushi".into(),
            agent_user: "agent".into(),
            import_host_store_db: false,
            roots: Roots {
                state_glob: PathBuf::from(state_glob),
                runtime_glob: PathBuf::from("/run/katsuobushi"),
            },
            tools: Tools {
                git: PathBuf::from(git),
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
            graphics: crate::sandbox::spec::GraphicsSpec::default(),
        }
    }

    fn output(code: i32, stderr: &[u8]) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn it_runs_the_pinned_git_push_invocation() {
        let state = "/state/cdata/katsuobushi";
        let spec = fake_spec(state, "/nix/store/h1-git/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("reviewer-abc"));
        host.push_run(Ok(output(0, b"")));

        let line = deliver_with(
            &host,
            &spec,
            "reviewer-abc",
            "sandbox-guest/card-xyz",
            false,
        )
        .expect("deliver should succeed");
        assert_eq!(
            line,
            "delivered sandbox-guest/card-xyz into reviewer-abc as delivered/card-xyz"
        );

        // Exact seam interaction: existence probe for instance resolution,
        // then the pinned git push into the target mirror.
        assert_eq!(
            host.calls(),
            vec![
                Call::Exists(PathBuf::from(state).join("reviewer-abc")),
                Call::Run(vec![
                    "/nix/store/h1-git/bin/git".to_string(),
                    "push".to_string(),
                    format!("{state}/reviewer-abc/sync.git"),
                    "+sandbox-guest/card-xyz:refs/heads/delivered/card-xyz".to_string(),
                ]),
            ]
        );
    }

    #[test]
    fn it_pushes_under_the_delivered_prefix_not_the_sandbox_prefix() {
        // The target ref must begin with refs/heads/delivered/, never
        // refs/heads/sandbox/ — the guest's own working namespace.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("reviewer-r"));
        host.push_run(Ok(output(0, b"")));

        deliver_with(&host, &spec, "reviewer-r", "sandbox-guest/src-inst", false)
            .expect("deliver ok");

        let push_call = host.calls().into_iter().find_map(|c| match c {
            Call::Run(v) if v.iter().any(|a| a == "push") => Some(v),
            _ => None,
        });
        let refspec = push_call.unwrap().into_iter().last().unwrap();
        assert!(
            refspec.contains("refs/heads/delivered/"),
            "refspec must land under delivered/: {refspec}"
        );
        assert!(
            !refspec.contains("refs/heads/sandbox/"),
            "refspec must not land under sandbox/: {refspec}"
        );
    }

    #[test]
    fn it_derives_the_basename_from_a_full_refs_remotes_path() {
        // A fetched remote bookmark like refs/remotes/sandbox-guest/card-abc
        // should land as delivered/card-abc, using only the last segment.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-r"));
        host.push_run(Ok(output(0, b"")));

        let line = deliver_with(
            &host,
            &spec,
            "inst-r",
            "refs/remotes/sandbox-guest/card-abc123",
            false,
        )
        .expect("deliver ok");
        assert_eq!(
            line,
            "delivered refs/remotes/sandbox-guest/card-abc123 into inst-r as delivered/card-abc123"
        );

        let push_call = host.calls().into_iter().find_map(|c| match c {
            Call::Run(v) if v.iter().any(|a| a == "push") => Some(v),
            _ => None,
        });
        assert_eq!(
            push_call.unwrap().last().unwrap(),
            "+refs/remotes/sandbox-guest/card-abc123:refs/heads/delivered/card-abc123"
        );
    }

    #[test]
    fn it_uses_a_plain_branch_name_as_its_own_basename() {
        // A bare branch name like `main` has no slashes — it is its own basename.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-r"));
        host.push_run(Ok(output(0, b"")));

        let line = deliver_with(&host, &spec, "inst-r", "main", false).expect("deliver ok");
        assert_eq!(line, "delivered main into inst-r as delivered/main");
    }

    #[test]
    fn it_emits_json_when_requested() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-j"));
        host.push_run(Ok(output(0, b"")));

        let line = deliver_with(&host, &spec, "inst-j", "sandbox-guest/card-xyz", true)
            .expect("deliver ok");
        assert_eq!(
            line,
            r#"{"delivered":"sandbox-guest/card-xyz","instance":"inst-j","as":"delivered/card-xyz"}"#
        );
    }

    #[test]
    fn it_fails_when_git_push_exits_nonzero() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-x"));
        host.push_run(Ok(output(1, b"fatal: no such repository")));

        let err = deliver_with(&host, &spec, "inst-x", "sandbox-guest/src", false)
            .expect_err("nonzero git must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no such repository"),
            "error should surface stderr: {msg}"
        );
    }

    #[test]
    fn it_rejects_a_branch_ending_in_slash() {
        // A trailing slash produces an empty basename — reject it up front.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-x"));

        let err = deliver_with(&host, &spec, "inst-x", "refs/heads/", false)
            .expect_err("empty basename must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("empty final component"),
            "error should mention empty component: {msg}"
        );
        // No git push should have been attempted.
        assert!(
            !host.calls().iter().any(|c| matches!(c, Call::Run(_))),
            "no run call should be made for an invalid branch"
        );
    }
}
