//! `katsuctl sandbox prompt` — push one prompt to an instance and stream its
//! reports (design/katsuctl.md §11). Absorbs and retires the standalone
//! `katsuobushi-sandbox-prompt` host client (`prompt.rs`): its `drive()`
//! streaming loop and the readiness-wait move here.
//!
//! The flow has three branches, decided in [`prompt_core`] so they are testable
//! against a [`FakeHost`](crate::sandbox::host::FakeHost) without a VM:
//!
//! - **running** (QMP answers): connect over vsock and stream `Report`s until a
//!   terminal status (`done`/`blocked`);
//! - **paused + named** (QMP silent, `instance.json.named`): the VM is powered
//!   off but kept on disk, so resume it via the `sandbox:start` menu command
//!   (`--agent --name <inst>`, boot only — *no* `--prompt`), then fall through to
//!   the same vsock delivery the running branch uses so `katsuctl` itself streams
//!   the turn once the channel arms. (The shell `start` runner no longer delivers
//!   `--prompt`; that lands natively in #015. Delivering here keeps restart
//!   self-contained — the turn is never silently dropped.);
//! - **not running + ephemeral**: there is nothing to resume — error clearly.
//!
//! The vsock streaming keeps the proven async/tokio + `tokio-vsock` machinery
//! from the old client (its own current-thread runtime), rather than routing
//! through [`Host::vsock_connect`](crate::sandbox::host::Host::vsock_connect)
//! (whose runtime is private). A freshly-booted instance needs ~30–60s before
//! vsock answers, so the connect retries with backoff (the old runner's
//! `--probe` loop, lib/sandbox/default.nix) — a successful connect *is* the
//! readiness signal.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use katsuobushi_sandbox_protocol::{GuestMessage, HostMessage, Prompt, Report, Status};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::runtime::Builder;
use tokio_vsock::{VsockAddr, VsockStream};

use crate::sandbox::host::{Host, HostImpl};
use crate::sandbox::instance::{self, Instance};
use crate::sandbox::output::{Renderer, ReportKind};
use crate::sandbox::resolve::resolve_instance;
use crate::sandbox::spec::{load_spec, resolve_roots, ResolvedRoots};
use crate::Global;

/// The turn id carried with the prompt. Correlation is by ordering (a single
/// serial session), so this is just a human-readable tag (protocol §4); the old
/// client defaulted it to 1 (`prompt.rs`).
const TURN_ID: u64 = 1;

/// How many times [`connect_with_retry`] attempts the vsock connect before giving
/// up, and the backoff cap between tries. With the 250ms→2s schedule this is a
/// ~3-minute readiness budget, matching the old runner's `for _ in $(seq 1 180)`
/// `--probe` loop (lib/sandbox/default.nix) so a just-booted instance is handled.
const READINESS_TRIES: usize = 90;
const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(2);

/// Which branch [`prompt_core`] took — returned so seam tests can assert the
/// decision without inspecting side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// The instance was running; the prompt was delivered over vsock to `cid`.
    Delivered { cid: u32 },
    /// The instance was paused but named; it was resumed and then the prompt was
    /// delivered over vsock (restart is self-contained — it does not rely on
    /// `sandbox:start --prompt`).
    Restarted,
}

/// Production entry point: load the spec, stand up the host seam, then run the
/// branch logic with the real `instance.json` read, the real `sandbox:start`
/// resume (boot only), and the real vsock streaming delivery.
pub fn run(config: &Path, instance: &str, text: Vec<String>, global: Global) -> Result<()> {
    let spec = load_spec(config)?;
    let roots = resolve_roots(&spec.roots)?;
    let host = HostImpl::new().context("initializing the host IO seam")?;
    let renderer = Renderer::resolve(global);

    let state_glob = roots.state_glob.as_path();
    let text = text.join(" ");
    let port = spec.vsock_port;

    prompt_core(
        &host,
        &roots,
        instance,
        &text,
        |inst| instance::read(state_glob, inst),
        resume_via_start,
        |cid| deliver_over_vsock(cid, port, &text, &renderer),
    )
    .map(|_action| ())
}

/// The testable core: resolve the instance, read its `instance.json`, probe QMP
/// liveness through the seam, and dispatch to the right branch.
///
/// `read_instance` learns the instance's `vsock_cid`/`named`; `boot` resumes a
/// paused named instance (launch only — no prompt); `deliver` streams a prompt to
/// a now-reachable instance (given its CID). All three are injected so a FakeHost
/// test drives the whole decision without a VM, a real `instance.json`, or a real
/// vsock.
///
/// A paused named instance is **booted then delivered to**: the boot only
/// launches the detached VM (it does not carry the prompt — the shell start
/// runner no longer delivers `--prompt`; that lands natively in #015), so the
/// restart path falls through to the *same* `deliver` the running path uses, and
/// `katsuctl` itself streams the turn over vsock once the channel arms. This
/// keeps restart self-contained: it never drops the turn waiting on `start`.
fn prompt_core(
    host: &impl Host,
    roots: &ResolvedRoots,
    instance: &str,
    text: &str,
    read_instance: impl FnOnce(&str) -> Result<Instance>,
    boot: impl FnOnce(&str) -> Result<()>,
    deliver: impl FnOnce(u32) -> Result<()>,
) -> Result<Action> {
    if text.is_empty() {
        bail!("usage: sandbox prompt <instance|#> \"<text>\"");
    }
    let inst = resolve_instance(&roots.state_glob, host, instance)?;
    let meta = read_instance(&inst)?;

    // No CID means no control channel: an interactive instance can't be prompted
    // (mirrors the old `vsock-cid` readability guard, lib/sandbox/default.nix).
    let Some(cid) = meta.vsock_cid else {
        bail!("sandbox prompt: no control channel for {inst:?} (is it an --agent instance?)");
    };

    // Liveness is derived from QMP, never stored (design §6): a live qemu monitor
    // answers at <runtimeGlob>/<inst>/katsuobushi.sock.
    let sock = roots.runtime_glob.join(&inst).join("katsuobushi.sock");
    if host.qmp_alive(&sock) {
        deliver(cid)?;
        Ok(Action::Delivered { cid })
    } else if meta.named {
        // Paused but kept on disk: resume it, then deliver. The live conversation
        // does not survive a pause (the VM's RAM is gone) — only the branch does —
        // so the resumed agent reads its committed work, not the prior in-VM
        // context. A resumed named instance keeps its recorded vsock CID, so the
        // CID from instance.json is still the one to stream to.
        boot(&inst)?;
        deliver(cid)?;
        Ok(Action::Restarted)
    } else {
        bail!(
            "sandbox prompt: {inst:?} is not running and isn't a kept instance, \
             so it can't be resumed"
        );
    }
}

/// Stand up a current-thread tokio runtime, wait for the guest control channel to
/// come up (retry/backoff connect), then send the prompt and stream the reports.
/// Keeps the old client's own-runtime approach rather than routing through the
/// host seam (whose runtime is private).
fn deliver_over_vsock(cid: u32, port: u32, text: &str, renderer: &Renderer) -> Result<()> {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime for the vsock prompt")?;
    let stream = connect_with_retry(&runtime, cid, port)?;
    runtime.block_on(drive(stream, TURN_ID, text.to_string(), |report| {
        render_report(renderer, report)
    }))
}

/// Connect to the guest control server over vsock, retrying with capped
/// exponential backoff so a freshly-booted instance (vsock not yet listening) is
/// handled. A successful connect is the readiness signal (the old `--probe`
/// semantics). The sleep is `std::thread::sleep` because the workspace's tokio
/// has no `time` feature, and there is nothing else for the runtime to do while
/// we wait.
fn connect_with_retry(
    runtime: &tokio::runtime::Runtime,
    cid: u32,
    port: u32,
) -> Result<VsockStream> {
    let mut delay = BACKOFF_START;
    for attempt in 0..READINESS_TRIES {
        match runtime.block_on(VsockStream::connect(VsockAddr::new(cid, port))) {
            Ok(stream) => return Ok(stream),
            Err(e) if attempt + 1 == READINESS_TRIES => {
                return Err(e).with_context(|| {
                    format!("connecting to the guest control channel (cid {cid}) timed out")
                });
            }
            Err(_) => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(BACKOFF_CAP);
            }
        }
    }
    unreachable!("the loop returns on the final attempt")
}

/// Send one `Prompt` over `stream`, then read guest lines and hand each `Report`
/// to `on_report`, stopping at a terminal status (`done`/`blocked`). Ported from
/// the old client's `drive()` (`prompt.rs`), with the rendering factored out
/// behind `on_report` so the streaming loop is unit-testable over a canned
/// channel. Generic over the transport (`AsyncRead + AsyncWrite`) so a test can
/// drive it with an in-memory duplex instead of a real vsock.
async fn drive<S, F>(stream: S, turn_id: u64, text: String, mut on_report: F) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(&Report) -> Result<()>,
{
    let (read_half, mut write_half) = tokio::io::split(stream);

    let mut line = serde_json::to_vec(&HostMessage::Prompt(Prompt { turn_id, text }))?;
    line.push(b'\n');
    write_half.write_all(&line).await.context("send prompt")?;
    write_half.flush().await.ok();

    let mut lines = BufReader::new(read_half).lines();
    while let Some(line) = lines.next_line().await.context("read guest")? {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<GuestMessage>(&line) {
            // Ready/undecodable lines are diagnostics, not reports: keep them off
            // stdout (which is the human stream / NDJSON) by logging to stderr.
            Ok(GuestMessage::Ready) => eprintln!("· guest ready"),
            Ok(GuestMessage::Report(report)) => {
                on_report(&report)?;
                if matches!(report.status, Status::Done | Status::Blocked) {
                    break;
                }
            }
            // New liveness variants (§4) decode-and-skip here: this is the
            // pure-additive phase, so `drive` keeps its current behavior until
            // the host watchdog rework wires them up. Tolerating them keeps the
            // decoder forward-compatible with a newer guest.
            Ok(_) => {}
            Err(e) => eprintln!("· undecodable guest line: {e}"),
        }
    }
    Ok(())
}

/// Render one streamed report: `--json` emits the `Report` as one line of NDJSON
/// (the existing wire format); human output uses the #007 status glyph/color
/// (design §13). Both go through [`Renderer::emit`], which serializes in `--json`
/// mode and paints (gated) otherwise.
fn render_report(renderer: &Renderer, report: &Report) -> Result<()> {
    let kind = match report.status {
        Status::Working => ReportKind::Working,
        Status::Done => ReportKind::Done,
        Status::Blocked => ReportKind::Blocked,
        Status::Info => ReportKind::Info,
    };
    renderer.emit(report, |r| r.report(kind, &report.text))
}

/// The `sandbox:start` arguments that resume a paused named instance: launch the
/// detached agent VM under its full (already-suffixed) name, with **no**
/// `--prompt`. Passing the verbatim suffixed name resumes that exact instance
/// (rather than minting a fresh one); the turn is delivered separately by the
/// caller's `deliver` over vsock, so `--prompt` must not appear here. Factored out
/// so the argv shape is unit-testable without spawning anything.
fn resume_via_start_args(inst: &str) -> Vec<String> {
    vec![
        "--agent".to_string(),
        "--name".to_string(),
        inst.to_string(),
    ]
}

/// Resume a paused named instance by running the `sandbox:start` menu command on
/// PATH (`--agent --name <inst>`, no prompt). The no-prompt agent launch returns
/// promptly after detaching the VM, so this spawns-and-waits (not `exec`) and then
/// returns to `prompt_core`, which streams the turn over vsock once the channel
/// arms — making restart self-contained rather than depending on `start` to
/// deliver `--prompt` (which it no longer does; that lands natively in #015).
fn resume_via_start(inst: &str) -> Result<()> {
    eprintln!(
        "sandbox prompt: {inst:?} is paused — resuming it to deliver this turn \
         (boot + arm ~30-60s)..."
    );
    let status = Command::new("sandbox:start")
        .args(resume_via_start_args(inst))
        .status()
        .with_context(|| format!("running sandbox:start to resume paused instance {inst:?}"))?;
    if !status.success() {
        bail!(
            "sandbox:start failed to resume {inst:?} (exit {})",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string())
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host::FakeHost;
    use crate::sandbox::instance::Mode;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const STATE: &str = "/state/cdata/katsuobushi";
    const RUNTIME: &str = "/run/cdata/katsuobushi";

    /// Token-free roots, so `resolve_instance`/path joins are deterministic.
    fn roots() -> ResolvedRoots {
        ResolvedRoots {
            state_glob: PathBuf::from(STATE),
            runtime_glob: PathBuf::from(RUNTIME),
        }
    }

    /// A FakeHost whose literal instance state dir exists, so `resolve_instance`
    /// accepts the name through the seam.
    fn host_with_instance(inst: &str) -> FakeHost {
        let mut host = FakeHost::new();
        host.with_existing(PathBuf::from(STATE).join(inst));
        host
    }

    /// An agent `Instance` with a CID (the prompt-able shape).
    fn agent_instance(name: &str, named: bool) -> Instance {
        Instance {
            instance_version: 1,
            name: name.to_string(),
            mode: Mode::Agent,
            named,
            ssh_port: 2222,
            vsock_cid: Some(4242),
        }
    }

    /// Drive `prompt_core` recording which seams fired, returning the outcome plus
    /// the instance names `boot` was asked to resume and the CIDs `deliver` was
    /// asked to stream to (so a test can prove a resumed instance is *also*
    /// delivered to, not just booted).
    #[allow(clippy::type_complexity)]
    fn run_core(
        host: &FakeHost,
        instance: &str,
        text: &str,
        meta: Result<Instance>,
    ) -> (Result<Action>, Vec<String>, Vec<u32>) {
        let booted: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let delivered: RefCell<Vec<u32>> = RefCell::new(Vec::new());
        let meta = RefCell::new(Some(meta));
        let outcome = prompt_core(
            host,
            &roots(),
            instance,
            text,
            |_| meta.borrow_mut().take().expect("read_instance called once"),
            |inst| {
                booted.borrow_mut().push(inst.to_string());
                Ok(())
            },
            |cid| {
                delivered.borrow_mut().push(cid);
                Ok(())
            },
        );
        (outcome, booted.into_inner(), delivered.into_inner())
    }

    // ---- branch logic (seam tests) ----

    #[test]
    fn it_delivers_to_a_running_instance() {
        // QMP answers -> the running branch streams (deliver), with the CID from
        // instance.json; no boot/resume happens.
        let mut host = host_with_instance("inst-run");
        host.with_alive_sock(
            PathBuf::from(RUNTIME)
                .join("inst-run")
                .join("katsuobushi.sock"),
        );

        let (outcome, booted, delivered) = run_core(
            &host,
            "inst-run",
            "hello",
            Ok(agent_instance("inst-run", true)),
        );

        assert_eq!(outcome.unwrap(), Action::Delivered { cid: 4242 });
        assert_eq!(delivered, vec![4242], "deliver fires with the instance CID");
        assert!(booted.is_empty(), "a running instance is never resumed");
    }

    #[test]
    fn it_resumes_then_delivers_to_a_paused_named_instance() {
        // QMP silent + named -> boot (resume) the instance, THEN fall through to
        // the same vsock delivery the running path uses, so the turn is not
        // dropped waiting on `sandbox:start --prompt` (which no longer delivers).
        let host = host_with_instance("inst-kept");

        let (outcome, booted, delivered) = run_core(
            &host,
            "inst-kept",
            "resume please",
            Ok(agent_instance("inst-kept", true)),
        );

        assert_eq!(outcome.unwrap(), Action::Restarted);
        assert_eq!(
            booted,
            vec!["inst-kept".to_string()],
            "the paused instance is resumed by its full name (boot only)"
        );
        assert_eq!(
            delivered,
            vec![4242],
            "and the turn is then delivered over vsock to the kept CID"
        );
    }

    #[test]
    fn it_resumes_with_agent_name_and_no_prompt() {
        // The resume argv must NOT carry the prompt: start only boots, katsuctl
        // delivers. Asserting the exact args guards against re-introducing
        // --prompt (which the shell start runner silently drops, dropping the turn).
        let args = resume_via_start_args("katsuobushi-20260627-abc123");
        assert_eq!(
            args,
            vec![
                "--agent".to_string(),
                "--name".to_string(),
                "katsuobushi-20260627-abc123".to_string(),
            ]
        );
        assert!(
            !args.iter().any(|a| a == "--prompt"),
            "resume must not pass --prompt: {args:?}"
        );
    }

    #[test]
    fn it_errors_on_a_paused_ephemeral_instance() {
        // QMP silent + not named -> nothing to resume.
        let host = host_with_instance("inst-eph");

        let (outcome, booted, delivered) = run_core(
            &host,
            "inst-eph",
            "hi",
            Ok(agent_instance("inst-eph", false)),
        );

        let err = outcome.expect_err("an ephemeral paused instance can't be resumed");
        assert!(format!("{err:#}").contains("can't be resumed"), "{err:#}");
        assert!(booted.is_empty() && delivered.is_empty());
    }

    #[test]
    fn it_errors_when_the_instance_has_no_control_channel() {
        // An interactive instance has no CID -> not prompt-able.
        let host = host_with_instance("inst-int");
        let interactive = Instance {
            mode: Mode::Interactive,
            vsock_cid: None,
            ..agent_instance("inst-int", true)
        };

        let (outcome, booted, delivered) = run_core(&host, "inst-int", "hi", Ok(interactive));

        let err = outcome.expect_err("no CID means no control channel");
        assert!(format!("{err:#}").contains("no control channel"), "{err:#}");
        assert!(booted.is_empty() && delivered.is_empty());
    }

    #[test]
    fn it_rejects_empty_prompt_text() {
        let host = FakeHost::new();
        let (outcome, _, _) = run_core(&host, "inst-x", "", Ok(agent_instance("inst-x", true)));
        let err = outcome.expect_err("empty prompt text is a usage error");
        assert!(format!("{err:#}").contains("usage"), "{err:#}");
        // The guard fires before any seam interaction.
        assert!(host.calls().is_empty());
    }

    // ---- streaming loop (canned channel) ----

    /// Run `drive` over an in-memory duplex: feed canned guest lines from one end,
    /// collect the statuses `drive` reports, and capture the prompt it sent.
    fn drive_over_canned(prompt: &str, guest_lines: &[&str]) -> (Vec<Status>, String) {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let collected: RefCell<Vec<Status>> = RefCell::new(Vec::new());
        let sent = RefCell::new(String::new());

        runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);

            let driver = drive(client, TURN_ID, prompt.to_string(), |report| {
                collected.borrow_mut().push(report.status);
                Ok(())
            });

            let feeder = async {
                // Drain the prompt the driver sends so its write side stays open,
                // then push the canned guest lines.
                let mut buf = vec![0u8; 256];
                let n = server.read(&mut buf).await.unwrap();
                *sent.borrow_mut() = String::from_utf8_lossy(&buf[..n]).into_owned();
                for line in guest_lines {
                    server.write_all(line.as_bytes()).await.unwrap();
                    server.write_all(b"\n").await.unwrap();
                }
                server.flush().await.unwrap();
            };

            let (result, ()) = tokio::join!(driver, feeder);
            result.unwrap();
        });

        (collected.into_inner(), sent.into_inner())
    }

    #[test]
    fn it_streams_reports_until_done() {
        // The terminal `done` stops the loop: the trailing `info` is never seen.
        let (statuses, sent) = drive_over_canned(
            "do it",
            &[
                r#"{"type":"report","status":"working","text":"building"}"#,
                r#"{"type":"report","status":"done","text":"shipped"}"#,
                r#"{"type":"report","status":"info","text":"after the end"}"#,
            ],
        );
        assert_eq!(statuses, vec![Status::Working, Status::Done]);

        // The driver sent exactly one Prompt carrying the text.
        let msg: HostMessage = serde_json::from_str(sent.trim()).expect("a HostMessage was sent");
        let HostMessage::Prompt(prompt) = msg;
        assert_eq!(prompt.text, "do it");
        assert_eq!(prompt.turn_id, TURN_ID);
    }

    #[test]
    fn it_stops_streaming_on_blocked() {
        let (statuses, _) = drive_over_canned(
            "go",
            &[
                r#"{"type":"report","status":"working","text":"trying"}"#,
                r#"{"type":"report","status":"blocked","text":"need a token"}"#,
                r#"{"type":"report","status":"working","text":"never reached"}"#,
            ],
        );
        assert_eq!(statuses, vec![Status::Working, Status::Blocked]);
    }

    #[test]
    fn it_skips_blank_and_ready_lines_then_streams() {
        // Blank lines are ignored; a `ready` is a diagnostic, not a report; only
        // the actual reports reach the sink.
        let (statuses, _) = drive_over_canned(
            "x",
            &[
                "",
                r#"{"type":"ready"}"#,
                r#"{"type":"report","status":"info","text":"fyi"}"#,
                r#"{"type":"report","status":"done","text":"ok"}"#,
            ],
        );
        assert_eq!(statuses, vec![Status::Info, Status::Done]);
    }
}
