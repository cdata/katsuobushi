//! `katsuctl sandbox attach` — the first emit-script command (design §8.3).
//!
//! `katsuctl` does the two probe-dependent decisions *directly* — the VM is
//! running (QMP) and the agent's tmux session is armed (`tmux has-session`, no
//! PTY) — then emits a tiny terminal-handoff recipe the devshell wrapper
//! `exec`s. The pre-checks act in Rust rather than in the script so a freshly
//! launched VM, an interactive (non `--agent`) VM, or a finished agent never
//! drops the caller into a bare login shell (the "attached, then kicked out
//! right after the README" symptom; lib/sandbox/default.nix:1959-1972).
//!
//! Replaces the `sandbox:attach` shell at lib/sandbox/default.nix:1935-1984; the
//! menu command becomes the documented emit+exec wrapper (design §8.1).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::sandbox::emit::{self, Recipe};
use crate::sandbox::host::{Host, HostImpl, OsRng, Rng};
use crate::sandbox::instance;
use crate::sandbox::resolve::resolve_instance;
use crate::sandbox::spec::{load_spec, resolve_roots, Spec};
use crate::Global;

/// The fixed tmux session name the agent harness runs under
/// (lib/sandbox/default.nix:1967).
const SESSION: &str = "katsuobushi";

/// What the planning pass decided: emit the handoff, or stop with guidance.
enum Outcome {
    /// A recipe was written and its path printed (the wrapper will `exec` it).
    Emitted,
    /// A pre-check failed; the caller prints this to stderr and exits nonzero
    /// **without** emitting a script (design §8.1: planning failure → no path).
    Guidance(String),
}

/// Production entry point: load the spec, stand up the real host seam, resolve
/// the instance and read its `ssh_port`, then plan + emit (or stop with
/// guidance). On a failed pre-check we print the guidance to stderr and exit
/// nonzero — no script, so the wrapper's `exec` is never reached.
pub fn run(config: &Path, instance: &str, _global: Global) -> Result<()> {
    let spec = load_spec(config)?;
    let host = HostImpl::new().context("initializing the host IO seam")?;
    let roots = resolve_roots(&spec.roots)?;
    let inst = resolve_instance(&roots.state_glob, &host, instance)?;
    // `ssh_port` lives in instance.json (design §6); read it before probing so
    // both the has-session pre-check and the emitted recipe use the same port.
    let meta = instance::read(&roots.state_glob, &inst)?;

    let mut rng = OsRng::new();
    let script_dir = emit::script_runtime_dir();
    match attach_with(&host, &mut rng, &script_dir, &spec, &inst, meta.ssh_port)? {
        Outcome::Emitted => Ok(()),
        Outcome::Guidance(text) => {
            eprint!("{text}");
            std::process::exit(1);
        }
    }
}

/// The testable core (design §7.2): with `inst` already resolved and `ssh_port`
/// already read, do the two direct probes through the seam and, only when both
/// pass, emit the recipe. Returns [`Outcome::Guidance`] (no script) on a failed
/// probe so the seam tests can assert "guidance + no temp-file write".
fn attach_with(
    host: &impl Host,
    rng: &mut impl Rng,
    script_dir: &Path,
    spec: &Spec,
    inst: &str,
    ssh_port: u16,
) -> Result<Outcome> {
    let roots = resolve_roots(&spec.roots)?;

    // Verify the VM is running via QMP (the one true liveness probe, §2.3);
    // same socket path as `sandbox:stop` (lib/sandbox/default.nix:1917).
    let sock = roots.runtime_glob.join(inst).join("katsuobushi.sock");
    if !host.qmp_alive(&sock) {
        return Ok(Outcome::Guidance(not_running(inst)));
    }

    // The ssh key path matches the prior art (lib/sandbox/default.nix:1955,
    // :1886): the per-instance private key under the runtime root.
    let key = roots.runtime_glob.join(inst).join("id");
    if !has_session(host, &key, ssh_port, &spec.agent_user) {
        return Ok(Outcome::Guidance(no_session(inst)));
    }

    emit::emit(host, script_dir, rng, || {
        Ok(attach_recipe(&key, ssh_port, &spec.agent_user))
    })?;
    Ok(Outcome::Emitted)
}

/// The direct `tmux has-session` pre-check (design §8.3): an ssh with **no PTY**
/// so it stays silent on success and skips the guest's login greeting. Mirrors
/// the prior-art `_ssh 'tmux has-session -t katsuobushi'`
/// (lib/sandbox/default.nix:1967); a nonzero exit means no live session.
fn has_session(host: &impl Host, key: &Path, ssh_port: u16, agent_user: &str) -> bool {
    let mut cmd = Command::new("ssh");
    cmd.arg("-i")
        .arg(key)
        .arg("-p")
        .arg(ssh_port.to_string())
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null")
        .arg(format!("{agent_user}@127.0.0.1"))
        .arg("tmux")
        .arg("has-session")
        .arg("-t")
        .arg(SESSION);
    matches!(host.run(&cmd), Ok(out) if out.status.success())
}

/// The emitted terminal-handoff recipe (design §8.3). A flat recipe whose tail
/// is the single `exec ssh … -t 'TERM=… tmux attach'`: `-t` forces a PTY for the
/// interactive client, and `TERM` is pinned in the *remote* command environment
/// (not forwarded via `SetEnv`, which needs sshd's `AcceptEnv`) because the
/// caller's `$TERM` may have no terminfo entry in the guest — tmux would abort
/// with "missing or unsuitable terminal" (lib/sandbox/default.nix:1973-1982).
fn attach_recipe(key: &Path, ssh_port: u16, agent_user: &str) -> Recipe {
    let mut recipe = Recipe::new();
    recipe
        .comment("katsuctl sandbox attach: hand the terminal to ssh + tmux attach")
        .line(format!(
            "exec ssh -i {} -p {ssh_port} -o StrictHostKeyChecking=no \
             -o UserKnownHostsFile=/dev/null {agent_user}@127.0.0.1 \
             -t 'TERM=xterm-256color tmux attach -t {SESSION}'",
            key.display()
        ));
    recipe
}

/// Guidance for a stopped instance (QMP says it is not running).
fn not_running(inst: &str) -> String {
    format!(
        "sandbox:attach: '{inst}' is not running\n{}",
        guidance_tail(inst)
    )
}

/// Guidance when the VM is up but has no live agent tmux session yet.
fn no_session(inst: &str) -> String {
    format!(
        "sandbox:attach: no live '{SESSION}' tmux session in '{inst}'\n{}",
        guidance_tail(inst)
    )
}

/// The shared "what to do next" lines both pre-check failures end with
/// (lib/sandbox/default.nix:1969-1970): a freshly launched VM needs ~30-60s to
/// arm the session, and only `--agent` instances ever run one.
fn guidance_tail(inst: &str) -> String {
    format!(
        "  - if it just launched, give it ~30-60s to arm, then retry\n  \
         - only --agent instances run a '{SESSION}' tmux session (check: sandbox:status {inst})\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host::{Call, FakeHost};
    use crate::sandbox::spec::{Roots, Tools};
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::{ExitStatus, Output};

    /// A scripted [`Rng`] yielding a fixed sequence (repeating the last value),
    /// so the emitted temp-file name is deterministic — same shape the `emit`
    /// and `host` test modules use.
    struct FakeRng {
        values: Vec<u32>,
        next: usize,
    }

    impl FakeRng {
        fn new(values: &[u32]) -> Self {
            Self {
                values: values.to_vec(),
                next: 0,
            }
        }
    }

    impl Rng for FakeRng {
        fn next_u32(&mut self) -> u32 {
            let value = self.values[self.next.min(self.values.len() - 1)];
            self.next += 1;
            value
        }
    }

    /// A spec whose roots are token-free (so `resolve_roots` is the identity and
    /// the recorded paths are deterministic), mirroring `fetch`'s test spec.
    fn fake_spec() -> Spec {
        Spec {
            spec_version: 1,
            project_id: "cdata/katsuobushi".into(),
            agent_user: "agent".into(),
            import_host_store_db: false,
            roots: Roots {
                state_glob: PathBuf::from("/state/katsuobushi"),
                runtime_glob: PathBuf::from("/run/katsuobushi"),
            },
            tools: Tools {
                git: PathBuf::from("/bin/git"),
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
        }
    }

    /// An `Output` for a process that exited with `code`.
    fn output(code: i32) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    /// Whether any recorded call is a temp-file write (a script was emitted).
    fn wrote_a_script(host: &FakeHost) -> bool {
        host.calls().iter().any(|c| matches!(c, Call::Write(..)))
    }

    // ---- golden snapshot (design §7.2 tier 2): the emitted recipe ----

    #[test]
    fn it_snapshots_the_attach_recipe() {
        // Fixed key/port/user keep the snapshot stable across machines.
        let key = Path::new("/run/katsuobushi/katsuobushi-20260627-abc123/id");
        insta::assert_snapshot!(attach_recipe(key, 2222, "agent").render());
    }

    // ---- the success path: a running instance with a live session emits ----

    #[test]
    fn it_emits_the_recipe_for_a_running_instance_with_a_session() {
        let spec = fake_spec();
        let mut host = FakeHost::new();
        // QMP is alive at the instance's socket, and has-session succeeds (the
        // default `run` result is a zero exit).
        host.with_alive_sock(PathBuf::from("/run/katsuobushi/inst-abc/katsuobushi.sock"));
        let mut rng = FakeRng::new(&[1, 2]);

        let outcome = attach_with(
            &host,
            &mut rng,
            Path::new("/run/user/1000"),
            &spec,
            "inst-abc",
            2222,
        )
        .expect("planning should succeed");

        assert!(matches!(outcome, Outcome::Emitted));
        assert!(wrote_a_script(&host), "a script must be emitted on success");

        // The probes happened in order: QMP liveness, then the no-PTY ssh
        // has-session with the pinned key path and port.
        let calls = host.calls();
        assert_eq!(
            calls[0],
            Call::QmpAlive(PathBuf::from("/run/katsuobushi/inst-abc/katsuobushi.sock"))
        );
        assert_eq!(
            calls[1],
            Call::Run(vec![
                "ssh".to_string(),
                "-i".to_string(),
                "/run/katsuobushi/inst-abc/id".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=no".to_string(),
                "-o".to_string(),
                "UserKnownHostsFile=/dev/null".to_string(),
                "agent@127.0.0.1".to_string(),
                "tmux".to_string(),
                "has-session".to_string(),
                "-t".to_string(),
                "katsuobushi".to_string(),
            ])
        );
    }

    // ---- the guard paths: guidance + nonzero, and NO script emitted ----

    #[test]
    fn it_prints_guidance_and_emits_no_script_when_no_session() {
        let spec = fake_spec();
        let mut host = FakeHost::new();
        host.with_alive_sock(PathBuf::from("/run/katsuobushi/inst-abc/katsuobushi.sock"));
        // has-session exits nonzero -> no live session.
        host.push_run(Ok(output(1)));
        let mut rng = FakeRng::new(&[1, 2]);

        let outcome = attach_with(
            &host,
            &mut rng,
            Path::new("/run/user/1000"),
            &spec,
            "inst-abc",
            2222,
        )
        .expect("a missing session is guidance, not an error");

        match outcome {
            Outcome::Guidance(text) => {
                assert!(
                    text.contains("no live 'katsuobushi' tmux session"),
                    "{text}"
                );
                assert!(text.contains("30-60s"), "{text}");
                assert!(text.contains("--agent"), "{text}");
            }
            Outcome::Emitted => panic!("must not emit when there is no session"),
        }
        // Critically: nothing was written — the caller is never dropped into a
        // bare login shell (design §8.1, no path on a planning failure).
        assert!(!wrote_a_script(&host), "no script on a missing session");
    }

    #[test]
    fn it_prints_guidance_and_emits_no_script_when_not_running() {
        let spec = fake_spec();
        // No alive socket registered -> qmp_alive is false.
        let host = FakeHost::new();
        let mut rng = FakeRng::new(&[1, 2]);

        let outcome = attach_with(
            &host,
            &mut rng,
            Path::new("/run/user/1000"),
            &spec,
            "inst-abc",
            2222,
        )
        .expect("a stopped instance is guidance, not an error");

        match outcome {
            Outcome::Guidance(text) => {
                assert!(text.contains("'inst-abc' is not running"), "{text}");
                assert!(text.contains("30-60s"), "{text}");
            }
            Outcome::Emitted => panic!("must not emit when the VM is not running"),
        }
        assert!(!wrote_a_script(&host), "no script when not running");
        // It short-circuits at the liveness probe: no ssh has-session attempt.
        assert!(
            !host.calls().iter().any(|c| matches!(c, Call::Run(_))),
            "must not probe the session when the VM is down"
        );
    }
}
