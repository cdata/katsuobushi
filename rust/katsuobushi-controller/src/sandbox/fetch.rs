//! `katsuctl sandbox fetch` — pull the work-product branch (the sandbox branch)
//! into the host repo ("act directly"). Replaces the
//! shell at: a single resolved `git fetch`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::sandbox::host::{self, Host, HostImpl};
use crate::sandbox::instance;
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

    host::run_ok(host, &cmd, &format!("git fetch for sandbox/{inst}"))?;

    // A dispatched agent can end its turn without committing. Compare the fetched
    // branch tip to the seed commit persisted at launch: equal means nothing was
    // committed on top of the seed. This is exact for every seed kind (a stash
    // snapshot, HEAD, or a resumed branch tip), so there are no false alarms.
    // When the seed is unknown (an instance from before it was recorded) or a
    // probe fails, assume work landed rather than warn wrongly (card e3e1d2).
    let landed = work_landed(host, &spec.tools.git, &roots.state_glob, &inst);

    Ok(match (json, landed) {
        (true, _) => format!(r#"{{"fetched":"sandbox/{inst}","landed":{landed}}}"#),
        (false, true) => format!("fetched sandbox/{inst}"),
        (false, false) => format!(
            "fetched sandbox/{inst} — WARNING: no committed work landed. The branch tip still \
             equals the seed commit, so the agent ended without committing. Inspect with `sandbox \
             attach {inst}`, or reset the card and re-dispatch a fresh instance."
        ),
    })
}

/// Whether the fetched `sandbox/<inst>` branch advanced past its seed commit.
/// Reads the seed SHA persisted in `instance.json` at launch and compares it to
/// the branch tip (`git rev-parse`). If the seed is unknown or either probe
/// fails, assume the work landed rather than raise a false alarm.
fn work_landed(host: &impl Host, git: &Path, state_glob: &Path, inst: &str) -> bool {
    let Some(seed) = read_seed(host, state_glob, inst) else {
        return true;
    };
    let mut cmd = Command::new(git);
    cmd.args(["rev-parse", &format!("sandbox/{inst}")]);
    match host.run(&cmd) {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim() != seed,
        _ => true,
    }
}

/// The seed commit recorded for `inst`, or `None` when `instance.json` is
/// missing, unparseable, or predates the field.
fn read_seed(host: &impl Host, state_glob: &Path, inst: &str) -> Option<String> {
    let path = state_glob.join(inst).join("instance.json");
    let bytes = host.read(&path).ok()?;
    instance::from_json_bytes(&bytes).ok()?.seed
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
        host.push_run(Ok(output(0, b""))); // the fetch
        host.push_read(Ok(instance_json("seedsha"))); // instance.json (seed)
        host.push_run(Ok(output_stdout(b"realsha\n"))); // rev-parse tip

        let line = fetch_with(&host, &spec, "inst-abc", false).expect("fetch should succeed");
        assert_eq!(line, "fetched sandbox/inst-abc");

        // The exact seam interaction: existence probe, the pinned git fetch, the
        // instance.json read for the seed, then the `rev-parse` tip probe.
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
                Call::Read(PathBuf::from(state).join("inst-abc").join("instance.json")),
                Call::Run(vec![
                    "/nix/store/h1-git/bin/git".to_string(),
                    "rev-parse".to_string(),
                    "sandbox/inst-abc".to_string(),
                ]),
            ]
        );
    }

    /// A minimal `instance.json` body carrying `seed`, for the `read` seam.
    fn instance_json(seed: &str) -> Vec<u8> {
        format!(
            r#"{{"instanceVersion":2,"name":"x","mode":"agent","named":false,"sshPort":2222,"vsockCid":4242,"seed":"{seed}"}}"#
        )
        .into_bytes()
    }

    #[test]
    fn it_emits_json_when_requested() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-1-abc"));
        host.push_run(Ok(output(0, b""))); // the fetch
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"realsha\n"))); // tip != seed

        let line = fetch_with(&host, &spec, "inst-1-abc", true).expect("fetch should succeed");
        assert_eq!(line, r#"{"fetched":"sandbox/inst-1-abc","landed":true}"#);
    }

    /// An `Output` that exited 0 with the given stdout (a `rev-parse` tip SHA).
    fn output_stdout(stdout: &[u8]) -> Output {
        Output {
            status: ExitStatus::from_raw(0),
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn it_warns_when_the_tip_still_equals_the_seed() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-seed"));
        host.push_run(Ok(output(0, b""))); // the fetch
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"seedsha\n"))); // tip == seed: no work

        let line = fetch_with(&host, &spec, "inst-seed", false).expect("fetch ok");
        assert!(
            line.contains("no committed work landed"),
            "should warn: {line}"
        );
    }

    #[test]
    fn it_reports_landed_false_in_json_for_a_seed_only_branch() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-seed"));
        host.push_run(Ok(output(0, b"")));
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"seedsha\n"))); // tip == seed

        let line = fetch_with(&host, &spec, "inst-seed", true).expect("fetch ok");
        assert_eq!(line, r#"{"fetched":"sandbox/inst-seed","landed":false}"#);
    }

    #[test]
    fn it_assumes_landed_when_the_tip_probe_fails() {
        // If `git rev-parse` can't run, assume the work landed rather than raise
        // a false alarm.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-x"));
        host.push_run(Ok(output(0, b""))); // the fetch succeeds
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output(1, b"fatal: bad revision"))); // the tip probe fails
        let line = fetch_with(&host, &spec, "inst-x", false).expect("fetch ok");
        assert_eq!(line, "fetched sandbox/inst-x");
    }

    #[test]
    fn it_assumes_landed_when_the_seed_is_unknown() {
        // An instance.json from before the seed field (or a missing file) leaves
        // the seed unknown; we can't tell, so assume landed and never probe the
        // tip.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-old"));
        host.push_run(Ok(output(0, b""))); // the fetch; no push_read -> read NotFound

        let line = fetch_with(&host, &spec, "inst-old", false).expect("fetch ok");
        assert_eq!(line, "fetched sandbox/inst-old");
        assert!(
            !host.calls().iter().any(|c| matches!(
                c,
                Call::Run(v) if v.iter().any(|a| a == "rev-parse")
            )),
            "a seedless instance must not probe the tip"
        );
    }

    #[test]
    fn it_reports_landed_for_a_real_commit_tip() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-real"));
        host.push_run(Ok(output(0, b"")));
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"realsha\n"))); // tip != seed

        let line = fetch_with(&host, &spec, "inst-real", false).expect("fetch ok");
        assert_eq!(line, "fetched sandbox/inst-real");
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
