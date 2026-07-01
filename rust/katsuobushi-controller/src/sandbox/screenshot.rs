//! `katsuctl sandbox screenshot` — the one host-visible graphical capability.
//!
//! Stateless framebuffer grab: on-demand `grim -` run as the agent user
//! over the **existing loopback ssh hostfwd** — no daemon, no new port, no new
//! channel. The compositor is already running as that user, so
//! `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR` are already correct in the ssh session;
//! `grim` reads the headless-sway output via wlr-screencopy and streams the PNG
//! back to host stdout, which we land at the requested path (or pass through to
//! host stdout for `-`).
//!
//! Two documented behaviors:
//!  1. It captures the *composited* sway output — the focused app + windows. A
//!     pure **Layer-0** workload that renders entirely offscreen (its own
//!     FBO/swapchain) and never puts a surface on the compositor screenshots as
//!     **blank** — `grim` only sees what is on the output. Correct and expected,
//!     not a bug.
//!  2. It requires the graphics opt-in. With graphics off there is no
//!     compositor, so we fail with a clear *"graphics not enabled for this
//!     instance"* up front rather than letting a cryptic `grim` error surface.
//!
//! The ssh invocation mirrors `attach.rs` (pinned `spec.tools.ssh`, the
//! per-instance key, and the no-known-hosts options); real capture is verified
//! separately on a real boot, so the tests here stay hermetic — they assert the
//! command line and the
//! stdout-vs-file routing without booting a VM.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::sandbox::host::{self, Host, HostImpl};
use crate::sandbox::instance;
use crate::sandbox::resolve::resolve_instance;
use crate::sandbox::spec::{load_spec, resolve_roots, Spec};
use crate::Global;

/// Where a captured PNG goes once `grim` hands it back.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Destination {
    /// Stream the bytes through to the host's stdout (the `-` argument).
    Stdout,
    /// Write the bytes to this file.
    File(PathBuf),
}

/// Production entry point: load the spec, stand up the real host seam, resolve
/// the instance and read its `ssh_port`, then capture and deliver the PNG.
pub fn run(config: &Path, instance: &str, path: Option<String>, _global: Global) -> Result<()> {
    let spec = load_spec(config)?;
    let host = HostImpl::new().context("initializing the host IO seam")?;
    let roots = resolve_roots(&spec.roots)?;
    let inst = resolve_instance(&roots.state_glob, &host, instance)?;
    // `ssh_port` lives in instance.json, exactly as `attach` reads it.
    let meta = instance::read(&roots.state_glob, &inst)?;
    // The per-instance private key under the runtime root (same path attach uses).
    let key = roots.runtime_glob.join(&inst).join("id");

    let dest = resolve_destination(path.as_deref(), &inst, &timestamp());
    let png = capture(&host, &spec, &inst, &key, meta.ssh_port)?;
    deliver(&host, &dest, &png)?;
    Ok(())
}

/// Resolve the destination from the optional `path` argument:
///  * `Some("-")` ⇒ host stdout,
///  * `Some(p)` ⇒ that file,
///  * `None` ⇒ a timestamped default `screenshot-<inst>-<ts>.png` in the cwd.
fn resolve_destination(path: Option<&str>, inst: &str, ts: &str) -> Destination {
    match path {
        Some("-") => Destination::Stdout,
        Some(p) => Destination::File(PathBuf::from(p)),
        None => Destination::File(PathBuf::from(format!("screenshot-{inst}-{ts}.png"))),
    }
}

/// Guard on the graphics opt-in, then run `grim -` over ssh and return the PNG
/// bytes. A disabled instance fails here with the clear message instead
/// of a cryptic `grim` error; a nonzero ssh/grim exit surfaces its stderr.
fn capture(
    host: &impl Host,
    spec: &Spec,
    inst: &str,
    key: &Path,
    ssh_port: u16,
) -> Result<Vec<u8>> {
    if !spec.graphics.enable {
        bail!(
            "graphics not enabled for this instance '{inst}' — there is no compositor to \
             screenshot; start it with graphics.enable=true (see sandbox:status {inst})"
        );
    }

    let cmd = build_ssh_command(&spec.tools.ssh, key, ssh_port, &spec.agent_user);
    let output = host::run_ok(host, &cmd, &format!("grim capture for '{inst}'"))?;
    Ok(output.stdout)
}

/// Build the `ssh … 'grim -'` invocation. Mirrors `attach.rs`'s no-PTY ssh: the
/// pinned `ssh` program, the per-instance key, the same no-known-hosts options,
/// then the remote `grim -` that writes the PNG to its stdout. Equivalent to
/// `ssh -p <port> agent@127.0.0.1 'grim -'`.
fn build_ssh_command(ssh: &Path, key: &Path, ssh_port: u16, agent_user: &str) -> Command {
    let mut cmd = Command::new(ssh);
    cmd.arg("-i")
        .arg(key)
        .arg("-p")
        .arg(ssh_port.to_string())
        .arg("-o")
        .arg("StrictHostKeyChecking=no")
        .arg("-o")
        .arg("UserKnownHostsFile=/dev/null")
        .arg(format!("{agent_user}@127.0.0.1"))
        .arg("grim")
        .arg("-");
    cmd
}

/// Land the captured bytes: pass them through to host stdout for `-`, or write
/// the file through the host seam (so the write is mockable in tests).
fn deliver(host: &impl Host, dest: &Destination, png: &[u8]) -> Result<()> {
    match dest {
        Destination::Stdout => std::io::stdout()
            .write_all(png)
            .context("streaming screenshot to stdout"),
        Destination::File(path) => {
            host.write(path, png)
                .with_context(|| format!("writing screenshot to {}", path.display()))?;
            // A Layer-0 offscreen workload screenshots as blank — note it so
            // a blank PNG is not mistaken for a bug.
            eprintln!(
                "wrote {} (a pure offscreen Layer-0 workload captures as blank — grim sees \
                 only the composited sway output)",
                path.display()
            );
            Ok(())
        }
    }
}

/// A monotonic-enough, library-free timestamp for the default filename: whole
/// seconds since the Unix epoch (no `chrono` is vendored, and calendar
/// formatting is not worth hand-rolling). Falls back to `0` on the impossible
/// pre-epoch clock.
fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host::{Call, FakeHost};
    use crate::sandbox::spec::{GraphicsSpec, Roots, Tools};
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::{ExitStatus, Output};

    /// A spec whose roots are token-free and whose `ssh` is a known path, with
    /// graphics toggled by `graphics_on` (mirrors the other subcommand specs).
    fn fake_spec(ssh: &str, graphics_on: bool) -> Spec {
        Spec {
            spec_version: 3,
            project_id: "cdata/katsuobushi".into(),
            agent_user: "agent".into(),
            import_host_store_db: false,
            roots: Roots {
                state_glob: PathBuf::from("/state/katsuobushi"),
                runtime_glob: PathBuf::from("/run/katsuobushi"),
            },
            tools: Tools {
                git: PathBuf::from("/bin/git"),
                ssh: PathBuf::from(ssh),
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
            graphics: GraphicsSpec {
                enable: graphics_on,
                ..GraphicsSpec::default()
            },
        }
    }

    /// An `Output` for a process that exited with `code`, carrying `stdout`.
    fn output(code: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    // ---- the graphics guard: a disabled instance fails with the clear message ----

    #[test]
    fn it_errors_clearly_when_graphics_is_disabled() {
        let spec = fake_spec("/nix/store/h-openssh/bin/ssh", false);
        let host = FakeHost::new();
        let key = PathBuf::from("/run/katsuobushi/inst-abc/id");

        let err = capture(&host, &spec, "inst-abc", &key, 2222)
            .expect_err("graphics off must fail before any ssh");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("graphics not enabled for this instance"),
            "clear guard message expected: {msg}"
        );
        // It short-circuits: no ssh/grim was attempted.
        assert!(
            host.calls().is_empty(),
            "no command should run when graphics is off"
        );
    }

    // ---- the ssh command line: ssh -p <port> agent@127.0.0.1 'grim -' ----

    #[test]
    fn it_builds_the_grim_over_ssh_command_line() {
        let ssh = "/nix/store/h-openssh/bin/ssh";
        let spec = fake_spec(ssh, true);
        let mut host = FakeHost::new();
        // grim succeeds, handing back PNG bytes on stdout.
        host.push_run(Ok(output(0, b"\x89PNG\r\n", b"")));
        let key = PathBuf::from("/run/katsuobushi/inst-abc/id");

        let png = capture(&host, &spec, "inst-abc", &key, 2222).expect("capture should succeed");
        assert_eq!(png, b"\x89PNG\r\n");

        // The exact invocation: pinned ssh, per-instance key, no-known-hosts
        // options, agent@127.0.0.1, then the remote `grim -`.
        assert_eq!(
            host.calls(),
            vec![Call::Run(vec![
                ssh.to_string(),
                "-i".to_string(),
                "/run/katsuobushi/inst-abc/id".to_string(),
                "-p".to_string(),
                "2222".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=no".to_string(),
                "-o".to_string(),
                "UserKnownHostsFile=/dev/null".to_string(),
                "agent@127.0.0.1".to_string(),
                "grim".to_string(),
                "-".to_string(),
            ])]
        );
    }

    #[test]
    fn it_surfaces_grim_stderr_on_a_nonzero_exit() {
        let spec = fake_spec("/bin/ssh", true);
        let mut host = FakeHost::new();
        host.push_run(Ok(output(1, b"", b"grim: no outputs")));
        let key = PathBuf::from("/run/katsuobushi/inst-x/id");

        let err = capture(&host, &spec, "inst-x", &key, 2222).expect_err("nonzero must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("no outputs"), "should surface stderr: {msg}");
    }

    // ---- destination routing: `-` ⇒ stdout, a path ⇒ that file, default name ----

    #[test]
    fn it_routes_a_dash_argument_to_stdout() {
        assert_eq!(
            resolve_destination(Some("-"), "inst-abc", "1751200000"),
            Destination::Stdout
        );
    }

    #[test]
    fn it_routes_an_explicit_path_to_that_file() {
        assert_eq!(
            resolve_destination(Some("shot.png"), "inst-abc", "1751200000"),
            Destination::File(PathBuf::from("shot.png"))
        );
    }

    #[test]
    fn it_defaults_to_a_timestamped_filename_in_cwd() {
        assert_eq!(
            resolve_destination(None, "inst-abc", "1751200000"),
            Destination::File(PathBuf::from("screenshot-inst-abc-1751200000.png"))
        );
    }

    // ---- delivery: a file write goes through the host seam ----

    #[test]
    fn it_writes_the_png_to_a_file_through_the_seam() {
        let host = FakeHost::new();
        let path = PathBuf::from("shot.png");
        deliver(&host, &Destination::File(path.clone()), b"\x89PNGdata")
            .expect("file delivery should succeed");
        assert_eq!(
            host.calls(),
            vec![Call::Write(path, b"\x89PNGdata".to_vec())]
        );
    }

    #[test]
    fn stdout_delivery_does_not_write_through_the_seam() {
        // `-` streams to the host's real stdout, so the seam records no write.
        let host = FakeHost::new();
        deliver(&host, &Destination::Stdout, b"\x89PNG").expect("stdout delivery should succeed");
        assert!(
            host.calls().is_empty(),
            "stdout delivery must not touch the file seam"
        );
    }
}
