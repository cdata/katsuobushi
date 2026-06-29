//! `katsuctl sandbox fetch` — pull the work-product branch (the sandbox branch)
//! into the host repo ("act directly"). Replaces the
//! shell at: a single resolved `git fetch`.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::sandbox::host::{Host, HostImpl};
use crate::sandbox::resolve::resolve_instance;
use crate::sandbox::spec::{load_spec, resolve_roots, Spec};
use crate::Global;

/// Production entry point: load the spec, stand up the real host seam, fetch.
pub fn run(config: &Path, instance: &str, global: Global) -> Result<()> {
    let spec = load_spec(config)?;
    let host = HostImpl::new().context("initializing the host IO seam")?;
    let line = fetch_with(&host, &spec, instance, global.json)?;
    println!("{line}");
    Ok(())
}

/// The testable core: resolve the instance, run the pinned `git fetch` through
/// the seam, and return the line to print (machine-readable when `json`).
///
/// The invocation is exactly today's shell:
/// `git fetch <stateGlob>/<inst>/sync.git sandbox/<inst>:sandbox/<inst>`,
/// with `git` taken from `spec.tools.git`.
fn fetch_with(host: &impl Host, spec: &Spec, instance: &str, json: bool) -> Result<String> {
    let roots = resolve_roots(&spec.roots)?;
    let inst = resolve_instance(&roots.state_glob, host, instance)?;

    let sync_git = roots.state_glob.join(&inst).join("sync.git");
    let refspec = format!("sandbox/{inst}:sandbox/{inst}");

    let mut cmd = Command::new(&spec.tools.git);
    cmd.arg("fetch").arg(&sync_git).arg(&refspec);

    let output = host
        .run(&cmd)
        .with_context(|| format!("running git fetch for sandbox/{inst}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        bail!(
            "git fetch for sandbox/{inst} failed{}{}",
            if detail.is_empty() { "" } else { ": " },
            detail
        );
    }

    Ok(if json {
        format!(r#"{{"fetched":"sandbox/{inst}"}}"#)
    } else {
        format!("fetched sandbox/{inst}")
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

    /// A spec whose roots are token-free (so `resolve_roots` is the identity and
    /// the recorded paths are deterministic) and whose `git` is a known path.
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
            },
            runner: PathBuf::from("/bin/microvm-run"),
            disk_images: vec![],
            context: vec![],
            secrets: vec![],
            vsock_port: 1024,
            host_cid: 2,
            heartbeat_secs: 10,
            heartbeat_miss: 3,
            progress_stall_secs: 300,
            delivery_deadline_secs: 20,
            delivery_retries: 3,
            ready_gate_secs: 60,
            stop_grace_ms: 1500,
            graphics: crate::sandbox::spec::GraphicsSpec::default(),
        }
    }

    /// An `Output` for a process that exited with `code`.
    fn output(code: i32, stderr: &[u8]) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn it_runs_the_pinned_git_fetch_invocation() {
        let state = "/state/cdata/katsuobushi";
        let spec = fake_spec(state, "/nix/store/h1-git/bin/git");
        let mut host = FakeHost::new();
        // Literal-name resolution checks the instance's state dir exists.
        host.with_existing(PathBuf::from(state).join("inst-abc"));

        let line = fetch_with(&host, &spec, "inst-abc", false).expect("fetch should succeed");
        assert_eq!(line, "fetched sandbox/inst-abc");

        // The exact seam interaction: existence probe, then the pinned git fetch.
        assert_eq!(
            host.calls(),
            vec![
                Call::Exists(PathBuf::from(state).join("inst-abc")),
                Call::Run(vec![
                    "/nix/store/h1-git/bin/git".to_string(),
                    "fetch".to_string(),
                    format!("{state}/inst-abc/sync.git"),
                    "sandbox/inst-abc:sandbox/inst-abc".to_string(),
                ]),
            ]
        );
    }

    #[test]
    fn it_emits_json_when_requested() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-1-abc"));

        let line = fetch_with(&host, &spec, "inst-1-abc", true).expect("fetch should succeed");
        assert_eq!(line, r#"{"fetched":"sandbox/inst-1-abc"}"#);
    }

    #[test]
    fn it_fails_when_git_fetch_exits_nonzero() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-x"));
        host.push_run(Ok(output(1, b"fatal: no such remote")));

        let err = fetch_with(&host, &spec, "inst-x", false).expect_err("nonzero git must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no such remote"),
            "should surface stderr: {msg}"
        );
    }
}
