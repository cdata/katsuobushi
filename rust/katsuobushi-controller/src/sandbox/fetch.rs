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

/// Remote name used for the per-instance sandbox bookmark. Writing to
/// `refs/remotes/` rather than `refs/heads/` means jj imports the ref as a
/// _remote_ bookmark: a remote bookmark moving never rewrites local history, so
/// force-updating it cannot orphan host commits.
const SANDBOX_GUEST_REMOTE: &str = "sandbox-guest";

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
/// The invocation writes into a remote bookmark rather than a local branch:
/// `git fetch <stateGlob>/<inst>/sync.git +sandbox/<inst>:refs/remotes/sandbox-guest/<inst>`,
/// with `git` taken from `spec.tools.git`. jj imports `refs/remotes/*`
/// alongside `refs/heads/*` and `refs/tags/*`, so the ref is visible to a
/// colocated jj repo. The leading `+` (force) keeps refetching idempotent at
/// the ref level. Because the destination is a remote bookmark, force-updating
/// it has no side effect on local history — jj never rewrites commits when a
/// remote bookmark moves.
///
/// Before the fetch, a guard reads the current ref tip and refuses if any local
/// `refs/heads/` branch descends from it. That detects the off-script case
/// where the host has used the old rebase-based landing workflow (which rewrites
/// guest commits into local branches), turning a silent data-loss into a loud
/// refusal.
fn fetch_with(host: &impl Host, spec: &Spec, instance: &str, json: bool) -> Result<String> {
    let roots = resolve_roots(&spec.roots)?;
    let inst = resolve_instance(&roots.state_glob, host, instance)?;

    let sync_git = roots.state_glob.join(&inst).join("sync.git");
    let refspec = format!("+sandbox/{inst}:refs/remotes/{SANDBOX_GUEST_REMOTE}/{inst}");

    // Guard: if the ref already exists, refuse when local branches descend from
    // its current tip. That is evidence of the old rebase-based landing, where
    // rebasing a guest commit onto host history made local branches track that
    // guest SHA — a force-update would then orphan those host commits.
    if let Some(old_tip) = read_remote_ref_tip(host, &spec.tools.git, &inst) {
        guard_against_local_descendants(host, &spec.tools.git, &inst, &old_tip)?;
    }

    let mut cmd = Command::new(&spec.tools.git);
    cmd.arg("fetch").arg(&sync_git).arg(&refspec);

    host::run_ok(host, &cmd, &format!("git fetch for sandbox/{inst}"))?;

    // A dispatched agent can end its turn without committing. Compare the fetched
    // ref tip to the seed commit persisted at launch: equal means nothing was
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

/// Returns the current tip SHA of `refs/remotes/sandbox-guest/<inst>`, or
/// `None` when the ref does not exist (first fetch) or the probe fails.
fn read_remote_ref_tip(host: &impl Host, git: &Path, inst: &str) -> Option<String> {
    let mut cmd = Command::new(git);
    cmd.args([
        "rev-parse",
        &format!("refs/remotes/{SANDBOX_GUEST_REMOTE}/{inst}"),
    ]);
    match host.run(&cmd) {
        Ok(out) if out.status.success() => {
            let tip = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if tip.is_empty() {
                None
            } else {
                Some(tip)
            }
        }
        _ => None,
    }
}

/// Refuses if any local `refs/heads/` branch **strictly descends** from
/// `old_tip` — evidence that the host has rebased the guest commit into local
/// history. Force-updating the remote bookmark past that tip would orphan those
/// host commits.
///
/// "Strictly descends" means the ref's tip is NOT old_tip itself. A ref that
/// points exactly at old_tip is migration debris — `refs/heads/sandbox/<inst>`
/// left by an older fetch scheme that wrote to `refs/heads/` instead of
/// `refs/remotes/` — not evidence of a rebased host commit. `--contains`
/// reports such a ref (a commit contains itself), so we must filter it out by
/// comparing the objectname to old_tip.
///
/// Silently passes through when the probe command fails (e.g., an older git
/// that does not support `--contains`): never block a legitimate fetch based on
/// a tooling error.
fn guard_against_local_descendants(
    host: &impl Host,
    git: &Path,
    inst: &str,
    old_tip: &str,
) -> Result<()> {
    let mut cmd = Command::new(git);
    cmd.args([
        "for-each-ref",
        "--format=%(refname) %(objectname)",
        &format!("--contains={old_tip}"),
        "refs/heads/",
    ]);
    let Ok(out) = host.run(&cmd) else {
        return Ok(());
    };
    if !out.status.success() {
        return Ok(());
    }
    let output = String::from_utf8_lossy(&out.stdout);
    let refs: Vec<&str> = output
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| match l.split_once(' ') {
            // Strict descent: keep only refs whose tip differs from old_tip.
            Some((refname, obj)) if obj.trim() != old_tip => Some(refname),
            // Objectname equals old_tip: migration debris — not a descendant.
            Some(_) => None,
            // Unparseable line (no space): be conservative and treat as descendant.
            None => Some(l),
        })
        .collect();
    if !refs.is_empty() {
        anyhow::bail!(
            "refusing fetch: local branch(es) {} have commits on top of the \
             current refs/remotes/{SANDBOX_GUEST_REMOTE}/{inst} tip — \
             force-updating would orphan that host work.\n\n\
             If any listed branch carries host work rebased onto the guest tip \
             (from the old landing workflow), cherry-pick those commits to a \
             new branch before re-fetching.",
            refs.join(", ")
        );
    }
    Ok(())
}

/// Whether the fetched branch advanced past its seed commit. Reads the seed SHA
/// persisted in `instance.json` at launch and compares it to the ref tip
/// (`git rev-parse refs/remotes/sandbox-guest/<inst>`) — the same ref the fetch
/// just wrote. If the seed is unknown or either probe fails, assume the work
/// landed rather than raise a false alarm.
fn work_landed(host: &impl Host, git: &Path, state_glob: &Path, inst: &str) -> bool {
    let Some(seed) = read_seed(host, state_glob, inst) else {
        return true;
    };
    let mut cmd = Command::new(git);
    cmd.args([
        "rev-parse",
        &format!("refs/remotes/{SANDBOX_GUEST_REMOTE}/{inst}"),
    ]);
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

    /// An `Output` that exited 0 with the given stdout (a `rev-parse` tip SHA).
    fn output_stdout(stdout: &[u8]) -> Output {
        Output {
            status: ExitStatus::from_raw(0),
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    /// A minimal `instance.json` body carrying `seed`, for the `read` seam.
    fn instance_json(seed: &str) -> Vec<u8> {
        format!(
            r#"{{"instanceVersion":2,"name":"x","mode":"agent","named":false,"sshPort":2222,"vsockCid":4242,"seed":"{seed}"}}"#
        )
        .into_bytes()
    }

    #[test]
    fn it_runs_the_pinned_git_fetch_invocation() {
        let state = "/state/cdata/katsuobushi";
        let spec = fake_spec(state, "/nix/store/h1-git/bin/git");
        let mut host = FakeHost::new();
        // Literal-name resolution checks the instance's state dir exists.
        host.with_existing(PathBuf::from(state).join("inst-abc"));
        host.push_run(Ok(output(1, b""))); // old tip probe: ref does not exist yet
        host.push_run(Ok(output(0, b""))); // the fetch
        host.push_read(Ok(instance_json("seedsha"))); // instance.json (seed)
        host.push_run(Ok(output_stdout(b"realsha\n"))); // rev-parse tip

        let line = fetch_with(&host, &spec, "inst-abc", false).expect("fetch should succeed");
        assert_eq!(line, "fetched sandbox/inst-abc");

        // The exact seam interaction: existence probe, old-tip probe (fails —
        // first fetch), the pinned git fetch into a remote bookmark, the
        // instance.json read for the seed, then the `rev-parse` tip probe.
        assert_eq!(
            host.calls(),
            vec![
                Call::Exists(PathBuf::from(state).join("inst-abc")),
                Call::Run(vec![
                    "/nix/store/h1-git/bin/git".to_string(),
                    "rev-parse".to_string(),
                    "refs/remotes/sandbox-guest/inst-abc".to_string(),
                ]),
                Call::Run(vec![
                    "/nix/store/h1-git/bin/git".to_string(),
                    "fetch".to_string(),
                    format!("{state}/inst-abc/sync.git"),
                    "+sandbox/inst-abc:refs/remotes/sandbox-guest/inst-abc".to_string(),
                ]),
                Call::Read(PathBuf::from(state).join("inst-abc").join("instance.json")),
                Call::Run(vec![
                    "/nix/store/h1-git/bin/git".to_string(),
                    "rev-parse".to_string(),
                    "refs/remotes/sandbox-guest/inst-abc".to_string(),
                ]),
            ]
        );
    }

    #[test]
    fn it_fetches_a_branch_into_a_jj_visible_remotes_ref() {
        // Writing to refs/remotes/ ensures jj imports it as a remote bookmark
        // (not a local branch), so a force-update never rewrites local history.
        // In a colocated jj repo, `jj git import` picks up the ref automatically.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-t"));
        host.push_run(Ok(output(1, b""))); // old tip probe: not found
        host.push_run(Ok(output(0, b"")));
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"realsha\n")));

        fetch_with(&host, &spec, "inst-t", false).expect("fetch ok");
        let fetched = host.calls().into_iter().find_map(|c| match c {
            Call::Run(v) if v.iter().any(|a| a == "fetch") => Some(v),
            _ => None,
        });
        assert_eq!(
            fetched.unwrap().last().unwrap(),
            "+sandbox/inst-t:refs/remotes/sandbox-guest/inst-t"
        );
    }

    #[test]
    fn it_reads_the_landed_probe_from_the_remotes_ref() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-p"));
        host.push_run(Ok(output(1, b""))); // old tip probe: not found
        host.push_run(Ok(output(0, b"")));
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"realsha\n")));

        fetch_with(&host, &spec, "inst-p", false).expect("fetch ok");
        let rev_parse = host.calls().into_iter().find_map(|c| match c {
            Call::Run(v)
                if v.iter().any(|a| a == "rev-parse")
                    && v.last().is_some_and(|a| a.contains("sandbox-guest/inst-p")) =>
            {
                Some(v)
            }
            _ => None,
        });
        // The landed probe reads from the same refs/remotes/ destination.
        assert_eq!(
            rev_parse.unwrap().last().unwrap(),
            "refs/remotes/sandbox-guest/inst-p"
        );
    }

    #[test]
    fn it_fetches_the_same_instance_twice_without_non_fast_forward() {
        // The host uses git merge --squash to land guest commits. The squash
        // commit on the host has no parent relationship to the guest commits, so
        // the remote bookmark always points only at guest history. A repeated
        // fetch of an already-landed instance (every review bounce) finds no
        // local descendants and succeeds cleanly.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-b"));

        // First fetch: ref does not exist yet; no old tip, no guard.
        host.push_run(Ok(output(1, b""))); // old tip probe: not found
        host.push_run(Ok(output(0, b""))); // the fetch
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"commit-a\n"))); // tip advanced

        // Second fetch (the bounce): old tip = commit-a; guard finds no local
        // descendants, so the fetch proceeds.
        host.push_run(Ok(output_stdout(b"commit-a\n"))); // old tip probe: found
        host.push_run(Ok(output(0, b""))); // for-each-ref: no local descendants
        host.push_run(Ok(output(0, b""))); // the fetch
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"commit-b\n"))); // tip advanced again

        assert!(fetch_with(&host, &spec, "inst-b", false).is_ok());
        assert!(fetch_with(&host, &spec, "inst-b", false).is_ok());
    }

    #[test]
    fn it_emits_json_when_requested() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-1-abc"));
        host.push_run(Ok(output(1, b""))); // old tip probe: not found
        host.push_run(Ok(output(0, b""))); // the fetch
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"realsha\n"))); // tip != seed

        let line = fetch_with(&host, &spec, "inst-1-abc", true).expect("fetch should succeed");
        assert_eq!(line, r#"{"fetched":"sandbox/inst-1-abc","landed":true}"#);
    }

    #[test]
    fn it_warns_when_the_tip_still_equals_the_seed() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-seed"));
        host.push_run(Ok(output(1, b""))); // old tip probe: not found
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
        host.push_run(Ok(output(1, b""))); // old tip probe: not found
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
        host.push_run(Ok(output(1, b""))); // old tip probe: not found
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
        host.push_run(Ok(output(1, b""))); // old tip probe: not found
        host.push_run(Ok(output(0, b""))); // the fetch; no push_read -> read NotFound

        let line = fetch_with(&host, &spec, "inst-old", false).expect("fetch ok");
        assert_eq!(line, "fetched sandbox/inst-old");
        // The pre-fetch old-tip probe (one rev-parse) is expected; the
        // work_landed tip probe must NOT run when the seed is unknown.
        // Count: exactly one rev-parse with sandbox-guest in the path.
        let rev_parse_count = host
            .calls()
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    Call::Run(v)
                        if v.iter().any(|a| a == "rev-parse")
                            && v.last().is_some_and(|a| a.contains("sandbox-guest"))
                )
            })
            .count();
        assert_eq!(
            rev_parse_count, 1,
            "only the pre-fetch old-tip probe may run; work_landed must not probe when seed is unknown"
        );
    }

    #[test]
    fn it_reports_landed_for_a_real_commit_tip() {
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-real"));
        host.push_run(Ok(output(1, b""))); // old tip probe: not found
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
        host.push_run(Ok(output(1, b""))); // old tip probe: not found
        host.push_run(Ok(output(1, b"fatal: no such remote")));

        let err = fetch_with(&host, &spec, "inst-x", false).expect_err("nonzero git must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no such remote"),
            "should surface stderr: {msg}"
        );
    }

    #[test]
    fn it_refuses_when_a_local_branch_descends_from_the_current_tip() {
        // Simulates the off-script case: host has rebased guest-A onto a local
        // branch (old landing workflow), then the guest pushes guest-B onto the
        // original guest-A. The guard detects a local descendant and refuses
        // rather than silently orphaning host work.
        //
        // The for-each-ref output carries "%(refname) %(objectname)"; the
        // objectname def456 differs from the old tip abc123, so it is a genuine
        // descendant (not migration debris).
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-g"));
        // Old tip probe succeeds: the ref exists from a prior fetch.
        host.push_run(Ok(output_stdout(b"abc123\n")));
        // for-each-ref: refs/heads/main whose tip (def456) ≠ old tip (abc123)
        // — a genuine descendant with host commits on top.
        host.push_run(Ok(output_stdout(b"refs/heads/main def456\n")));
        // The fetch is never reached.

        let err = fetch_with(&host, &spec, "inst-g", false)
            .expect_err("should refuse when local descendants exist");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("orphan"),
            "error should mention orphaning: {msg}"
        );
        assert!(
            msg.contains("refs/heads/main"),
            "error should name the offending branch: {msg}"
        );
    }

    #[test]
    fn it_proceeds_when_for_each_ref_returns_only_migration_debris() {
        // After upgrading from the previous fetch scheme (which wrote to
        // refs/heads/sandbox/<inst>), a refs/heads/sandbox/<inst> ref is left
        // pointing at the same commit that refs/remotes/sandbox-guest/<inst>
        // now holds. --contains returns it because a commit contains itself,
        // but its objectname equals old_tip, so strict-descent filtering
        // removes it and the fetch proceeds.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-g"));
        // Old tip probe: the new-scheme ref exists from the first post-upgrade fetch.
        host.push_run(Ok(output_stdout(b"abc123\n")));
        // for-each-ref: refs/heads/sandbox/inst-g whose objectname IS abc123
        // (equal to old tip) — migration debris, not a genuine descendant.
        host.push_run(Ok(output_stdout(b"refs/heads/sandbox/inst-g abc123\n")));
        // The fetch proceeds past the guard.
        host.push_run(Ok(output(0, b"")));
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"def456\n")));

        let result = fetch_with(&host, &spec, "inst-g", false);
        assert!(
            result.is_ok(),
            "fetch must proceed when for-each-ref returns only migration debris: {result:?}"
        );
    }

    #[test]
    fn it_succeeds_on_bounce_when_no_local_branches_descend() {
        // The canonical review bounce: guest pushes commit-A, host fetches, host
        // lands via `git merge --squash` (the squash commit has no parent
        // relationship to the guest commits, leaving the remote bookmark
        // untouched), guest pushes commit-B onto the original commit-A, host
        // fetches again. No local branch descends from the old tip, so the guard
        // passes and the second fetch lands.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-bounce"));

        // First fetch: no old tip yet.
        host.push_run(Ok(output(1, b""))); // old tip probe: not found
        host.push_run(Ok(output(0, b""))); // fetch commit-A
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"commit-a\n")));

        // Second fetch (the bounce): old tip = commit-a; no local descendants
        // (host used jj duplicate, not jj rebase).
        host.push_run(Ok(output_stdout(b"commit-a\n"))); // old tip found
        host.push_run(Ok(output(0, b""))); // for-each-ref: no descendants
        host.push_run(Ok(output(0, b""))); // fetch commit-B
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"commit-b\n")));

        let first = fetch_with(&host, &spec, "inst-bounce", false);
        let second = fetch_with(&host, &spec, "inst-bounce", false);
        assert!(first.is_ok(), "first fetch must succeed: {first:?}");
        assert!(
            second.is_ok(),
            "bounce fetch must succeed without orphaning: {second:?}"
        );
    }

    #[test]
    fn it_refuses_with_cherry_pick_guidance_only() {
        // The refusal message must advise cherry-picking host work to a new
        // branch. It must NOT offer the stale "delete refs/heads/sandbox/<inst>"
        // advice: strict-descent filtering removes any ref whose tip equals
        // old_tip before the message is built, so every listed ref already has
        // commits on top — pure-guest-history refs never reach this message.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-g"));
        host.push_run(Ok(output_stdout(b"abc123\n"))); // old tip found
        host.push_run(Ok(output_stdout(b"refs/heads/main def456\n"))); // genuine descendant

        let err = fetch_with(&host, &spec, "inst-g", false)
            .expect_err("should refuse when local descendants exist");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("cherry-pick"),
            "message should advise cherry-pick: {msg}"
        );
        assert!(
            !msg.contains("git branch -D"),
            "message must not advise deleting the branch (stale clause): {msg}"
        );
        assert!(
            !msg.contains("pure guest history"),
            "message must not mention pure guest history (stale clause): {msg}"
        );
    }

    #[test]
    fn it_skips_the_guard_when_the_for_each_ref_probe_fails() {
        // If the probe itself fails (e.g., an older git), we pass through silently
        // rather than blocking a legitimate fetch.
        let state = "/state";
        let spec = fake_spec(state, "/bin/git");
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(state).join("inst-old-git"));
        host.push_run(Ok(output_stdout(b"abc123\n"))); // old tip found
        host.push_run(Ok(output(1, b"error: unknown option"))); // for-each-ref fails
        host.push_run(Ok(output(0, b""))); // fetch proceeds anyway
        host.push_read(Ok(instance_json("seedsha")));
        host.push_run(Ok(output_stdout(b"def456\n")));

        let line = fetch_with(&host, &spec, "inst-old-git", false).expect("fetch ok");
        assert_eq!(line, "fetched sandbox/inst-old-git");
    }
}
