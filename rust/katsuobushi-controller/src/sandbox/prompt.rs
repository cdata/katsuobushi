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
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::runtime::Builder;
use tokio::time::{sleep_until, Instant};
use tokio_vsock::{VsockAddr, VsockStream};

use crate::sandbox::host::{Host, HostImpl};
use crate::sandbox::instance::{self, Instance};
use crate::sandbox::liveness::{alloc_turn_id, now_rfc3339, Liveness};
use crate::sandbox::output::{Renderer, ReportKind};
use crate::sandbox::resolve::resolve_instance;
use crate::sandbox::spec::{load_spec, resolve_roots, ResolvedRoots, Spec};
use crate::Global;

/// How many times [`connect_with_retry`] attempts the vsock connect before giving
/// up, and the backoff cap between tries. With the 250ms→2s schedule this is a
/// ~3-minute readiness budget, matching the old runner's `for _ in $(seq 1 180)`
/// `--probe` loop (lib/sandbox/default.nix) so a just-booted instance is handled.
const READINESS_TRIES: usize = 90;
const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(2);

/// The host watchdog's three deadlines plus the resend budget (design
/// /sandbox-liveness.md §8, §18), resolved from the spec tunables. Carried into
/// [`drive`] so its `select!` timers are driven by data, not magic numbers — and
/// so a test can shrink them.
#[derive(Debug, Clone, Copy)]
struct Watchdog {
    /// `heartbeatSecs * heartbeatMiss`: no `Heartbeat` within this window ⇒ the
    /// transport is dead (error).
    heartbeat_deadline: Duration,
    /// `progressStallSecs`: no `Report`/lifecycle within this window ⇒ surface a
    /// one-shot "no progress" notice (no break).
    progress_deadline: Duration,
    /// `deliveryDeadlineSecs`: no `TurnAccepted` within this window ⇒ resend the
    /// identical `Prompt`.
    delivery_deadline: Duration,
    /// `deliveryRetries`: max resends before the delivery fails clearly.
    delivery_retries: u32,
}

impl Watchdog {
    /// Resolve the deadlines from the Nix-rendered spec tunables (design §18).
    fn from_spec(spec: &Spec) -> Self {
        Self {
            heartbeat_deadline: Duration::from_secs(
                spec.heartbeat_secs
                    .saturating_mul(u64::from(spec.heartbeat_miss)),
            ),
            progress_deadline: Duration::from_secs(spec.progress_stall_secs),
            delivery_deadline: Duration::from_secs(spec.delivery_deadline_secs),
            delivery_retries: spec.delivery_retries,
        }
    }
}

/// What [`drive`] surfaces to its sink — everything that should reach the host
/// orchestrator. A `Heartbeat` is deliberately **absent**: it is handled silently
/// (timer reset + a throttled `liveness.json` touch), so a backgrounded `drive`
/// emits zero bytes on a tick (design §8.1). The transport-dead and resend-
/// exhausted verdicts are not here either — they are terminal `Err`s, rendered as
/// `Lost` by [`deliver_over_vsock`].
enum DriveEvent<'a> {
    /// A relayed agent `Report` (working/info/done/blocked).
    Report(&'a Report),
    /// The one-shot progress-stall notice (§8): "no progress for T".
    Stalled(&'a str),
    /// The §6.2 `reported:false` verdict for this `turn_id` — the agent stopped
    /// without a terminal report.
    Stopped(u64),
}

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
    let watchdog = Watchdog::from_spec(&spec);

    prompt_core(
        &host,
        &roots,
        instance,
        &text,
        |inst| instance::read(state_glob, inst),
        resume_via_start,
        |cid, inst| {
            // The turn id is allocated from (and the heartbeat record written to)
            // `liveness.json` beside the instance's `instance.json` (design §9).
            let liveness_path = state_glob.join(inst).join("liveness.json");
            let turn_id = alloc_turn_id(&host, &liveness_path)?;
            deliver_over_vsock(
                &host,
                cid,
                port,
                turn_id,
                &text,
                watchdog,
                &liveness_path,
                &renderer,
            )
        },
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
    deliver: impl FnOnce(u32, &str) -> Result<()>,
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
        deliver(cid, &inst)?;
        Ok(Action::Delivered { cid })
    } else if meta.named {
        // Paused but kept on disk: resume it, then deliver. The live conversation
        // does not survive a pause (the VM's RAM is gone) — only the branch does —
        // so the resumed agent reads its committed work, not the prior in-VM
        // context. A resumed named instance keeps its recorded vsock CID, so the
        // CID from instance.json is still the one to stream to.
        boot(&inst)?;
        deliver(cid, &inst)?;
        Ok(Action::Restarted)
    } else {
        bail!(
            "sandbox prompt: {inst:?} is not running and isn't a kept instance, \
             so it can't be resumed"
        );
    }
}

/// Stand up a current-thread tokio runtime, wait for the guest control channel to
/// come up (retry/backoff connect), then run the watchdog [`drive`] loop. Keeps
/// the old client's own-runtime approach rather than routing through the host
/// seam (whose runtime is private).
///
/// The streaming sink renders agent `Report`s and the watchdog's `Stalled`/
/// `Stopped` notices; the silent heartbeat touch writes `liveness.json` through
/// the host seam (design §8.1/§9). A terminal `Err` (transport dead / resend
/// exhausted) is rendered once as the `Lost` ✗ verdict, then the process exits
/// nonzero — short-circuiting `anyhow`'s noisier top-level chain.
#[allow(clippy::too_many_arguments)]
fn deliver_over_vsock(
    host: &impl Host,
    cid: u32,
    port: u32,
    turn_id: u64,
    text: &str,
    watchdog: Watchdog,
    liveness_path: &Path,
    renderer: &Renderer,
) -> Result<()> {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime for the vsock prompt")?;
    let stream = connect_with_retry(&runtime, cid, port)?;
    set_stream_active(host, liveness_path, true);

    let sink = |event: DriveEvent| -> Result<()> {
        match event {
            DriveEvent::Report(report) => render_report(renderer, report),
            DriveEvent::Stalled(text) => render_note(renderer, ReportKind::Stalled, text),
            DriveEvent::Stopped(turn_id) => {
                render_note(renderer, ReportKind::Stopped, &stopped_message(turn_id))
            }
        }
    };
    // The heartbeat touch: load-modify-store the liveness record with a fresh
    // timestamp from the clock seam. Best-effort — a failed write never fails the
    // turn (design §8.1) — and silent (no render/print).
    let touch = || {
        let mut liveness = Liveness::load(host, liveness_path);
        if let Ok(stamp) = now_rfc3339(host) {
            liveness.last_heartbeat_at = Some(stamp);
        }
        liveness.stream_active = true;
        let _ = liveness.store(host, liveness_path);
    };

    let result = runtime.block_on(drive(
        stream,
        turn_id,
        text.to_string(),
        watchdog,
        sink,
        touch,
    ));
    set_stream_active(host, liveness_path, false);

    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = render_note(renderer, ReportKind::Lost, &format!("{e:#}"));
            std::process::exit(1);
        }
    }
}

/// Flip `streamActive` in `liveness.json` (best-effort) so `status` can tell an
/// attached `drive` from a stale record (design §9). Preserves the rest of the
/// record via load-modify-store.
fn set_stream_active(host: &impl Host, path: &Path, active: bool) {
    let mut liveness = Liveness::load(host, path);
    liveness.stream_active = active;
    let _ = liveness.store(host, path);
}

/// The §6.2 message for a turn that stopped without a terminal report.
fn stopped_message(turn_id: u64) -> String {
    format!(
        "agent stopped without reporting (turn {turn_id}) — possible silent \
         completion or unreported hang; inspect with `sandbox:attach` / `sandbox:fetch`"
    )
}

/// Render a watchdog notice (Stalled/Stopped/Lost) through the shared renderer:
/// `--json` emits a tagged `{"event":…,"text":…}` line (the NDJSON stream's
/// out-of-band note), human mode paints the glyph line (design §13/§16).
fn render_note(renderer: &Renderer, kind: ReportKind, text: &str) -> Result<()> {
    #[derive(Serialize)]
    struct Note<'a> {
        event: &'a str,
        text: &'a str,
    }
    let event = match kind {
        ReportKind::Stalled => "stalled",
        ReportKind::Stopped => "stopped",
        ReportKind::Lost => "lost",
        _ => "note",
    };
    renderer.emit(&Note { event, text }, |r| r.report(kind, text))
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

/// The host watchdog (design/sandbox-liveness.md §8, §16). Sends `Prompt{turn_id}`
/// over `stream`, then runs a `select!` loop over the guest line stream plus three
/// deadline timers, until a terminal condition:
///
/// - **heartbeat-deadline** (`heartbeat_deadline`): no `Heartbeat` in the window
///   ⇒ the transport is dead ⇒ `Err` (rendered `Lost`).
/// - **progress-deadline** (`progress_deadline`): no `Report`/lifecycle in the
///   window ⇒ surface the `Stalled` notice **once** per episode (no break, no
///   kill — §8); cleared by the next `working`/`info` report.
/// - **delivery-deadline** (`delivery_deadline`): no `TurnAccepted` yet ⇒ resend
///   the identical `Prompt` up to `delivery_retries`, then `Err` clearly (§7.2).
///
/// A `Heartbeat` is handled **silently** — reset `last_hb` + a throttled (≤1/s)
/// `touch()` of `liveness.json` — and reaches the orchestrator as zero bytes
/// (§8.1). Terminal breaks: a `done`/`blocked` `Report`, `TurnCompleted{true}`
/// (clean), or `TurnCompleted{false}` (the §6.2 `Stopped` warning). EOF (`None`)
/// is a transport-closed-mid-turn `Err`. Excludes the §7.1 ready-gate (that is
/// #029). Generic over the transport so a test drives it with an in-memory duplex.
async fn drive<S, Sink, Touch>(
    stream: S,
    turn_id: u64,
    text: String,
    watchdog: Watchdog,
    mut sink: Sink,
    mut touch: Touch,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    Sink: FnMut(DriveEvent) -> Result<()>,
    Touch: FnMut(),
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();

    // Encode once so a resend (§7.2) replays the *identical* Prompt bytes — the
    // guest dedupes on `turn_id`, so a resend racing a slow first delivery is
    // dropped harmlessly.
    let prompt_line = {
        let mut line = serde_json::to_vec(&HostMessage::Prompt(Prompt { turn_id, text }))?;
        line.push(b'\n');
        line
    };
    write_half
        .write_all(&prompt_line)
        .await
        .context("send prompt")?;
    write_half.flush().await.ok();

    let Watchdog {
        heartbeat_deadline,
        progress_deadline,
        delivery_deadline,
        delivery_retries,
    } = watchdog;

    let mut last_hb = Instant::now();
    let mut last_prog = Instant::now();
    let mut sent = Instant::now();
    let mut last_touch: Option<Instant> = None;
    let mut accepted = false;
    let mut resends: u32 = 0;
    let mut stall_surfaced = false;

    loop {
        tokio::select! {
            read = lines.next_line() => {
                match read.context("read guest")? {
                    // EOF mid-turn: the held-open control stream closed before any
                    // terminal report. The transport is gone — error, never wait.
                    None => bail!("transport closed mid-turn (guest stream EOF before a terminal report)"),
                    Some(raw) => {
                        let line = raw.trim();
                        if line.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<GuestMessage>(line) {
                            Ok(GuestMessage::Heartbeat { .. }) => {
                                // §8.1 invariant: SILENT — reset the deadline and a
                                // throttled (≤1/s) liveness touch only. No render,
                                // no print, so a backgrounded drive is never woken.
                                last_hb = Instant::now();
                                let due = last_touch
                                    .is_none_or(|t| last_hb.duration_since(t) >= Duration::from_secs(1));
                                if due {
                                    touch();
                                    last_touch = Some(last_hb);
                                }
                            }
                            Ok(GuestMessage::Report(report)) => match report.status {
                                Status::Working | Status::Info => {
                                    // Progress: reset the stall timer, (re)arm the
                                    // one-shot notice, and treat it as the implicit
                                    // delivery ack (§7.2 fallback for no turn-start hook).
                                    last_prog = Instant::now();
                                    stall_surfaced = false;
                                    accepted = true;
                                    sink(DriveEvent::Report(&report))?;
                                }
                                Status::Done | Status::Blocked => {
                                    sink(DriveEvent::Report(&report))?;
                                    break; // clean terminal — the host breaks immediately (§8)
                                }
                            },
                            Ok(GuestMessage::TurnAccepted { turn_id: id }) if id == turn_id => {
                                accepted = true;
                            }
                            Ok(GuestMessage::TurnCompleted { turn_id: id, reported }) if id == turn_id => {
                                if !reported {
                                    // §6.2: stopped without a terminal report — warn.
                                    sink(DriveEvent::Stopped(turn_id))?;
                                }
                                break;
                            }
                            // Tolerated/diagnostic — none reach the orchestrator: a
                            // per-connect `ready`, a late `SessionReady`, lifecycle
                            // for a stale `turn_id`, or an unknown newer variant.
                            Ok(GuestMessage::Ready) => eprintln!("· guest ready"),
                            Ok(_) => {}
                            Err(e) => eprintln!("· undecodable guest line: {e}"),
                        }
                    }
                }
            }
            _ = sleep_until(last_hb + heartbeat_deadline) => {
                bail!(
                    "transport dead — no heartbeat for {}s (the VM or guest server is gone)",
                    heartbeat_deadline.as_secs()
                );
            }
            _ = sleep_until(last_prog + progress_deadline), if !stall_surfaced => {
                let note = format!(
                    "no progress for {}s — the agent may be stuck (inspect with `sandbox:attach`)",
                    progress_deadline.as_secs()
                );
                sink(DriveEvent::Stalled(&note))?;
                stall_surfaced = true; // one-shot per episode; no break, no kill (§8)
            }
            _ = sleep_until(sent + delivery_deadline), if !accepted => {
                if resends < delivery_retries {
                    write_half
                        .write_all(&prompt_line)
                        .await
                        .context("resend prompt")?;
                    write_half.flush().await.ok();
                    resends += 1;
                    sent = Instant::now();
                } else {
                    bail!(
                        "turn {turn_id} not accepted after {delivery_retries} resends — \
                         delivery failed (the agent never began the turn)"
                    );
                }
            }
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
            |cid, _inst| {
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

    // ---- streaming loop / watchdog (canned channel, tier 2) ----

    use tokio::io::DuplexStream;

    /// The flavor of a [`DriveEvent`] the sink saw, flattened for assertions.
    #[derive(Debug, PartialEq, Eq)]
    enum Ev {
        Report(Status),
        Stalled,
        Stopped(u64),
    }

    /// Deadlines so wide the watchdog timers never fire during a canned feed — the
    /// timer behavior itself is exercised separately under a *paused* clock.
    fn relaxed_watchdog() -> Watchdog {
        Watchdog {
            heartbeat_deadline: Duration::from_secs(3600),
            progress_deadline: Duration::from_secs(3600),
            delivery_deadline: Duration::from_secs(3600),
            delivery_retries: 3,
        }
    }

    /// Read whatever bytes are pending on `server`, trimmed (one short NDJSON line
    /// per write, so a single read suffices); empty string on EOF.
    async fn read_chunk(server: &mut DuplexStream) -> String {
        let mut buf = vec![0u8; 512];
        let n = server.read(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf[..n]).trim().to_string()
    }

    /// Run `drive` over an in-memory duplex with relaxed timers: feed canned guest
    /// lines, returning `drive`'s result, the events it surfaced, the prompt it
    /// sent, and how many silent liveness touches the heartbeats triggered. Every
    /// caller must feed a terminal line (relaxed timers never break on their own).
    fn drive_over_canned(
        prompt: &str,
        turn_id: u64,
        guest_lines: &[&str],
    ) -> (Result<()>, Vec<Ev>, String, usize) {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let events: RefCell<Vec<Ev>> = RefCell::new(Vec::new());
        let touches = RefCell::new(0usize);
        let sent = RefCell::new(String::new());

        let result = runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);

            let driver = drive(
                client,
                turn_id,
                prompt.to_string(),
                relaxed_watchdog(),
                |event: DriveEvent| -> Result<()> {
                    events.borrow_mut().push(match event {
                        DriveEvent::Report(r) => Ev::Report(r.status),
                        DriveEvent::Stalled(_) => Ev::Stalled,
                        DriveEvent::Stopped(id) => Ev::Stopped(id),
                    });
                    Ok(())
                },
                || *touches.borrow_mut() += 1,
            );

            let feeder = async {
                // Drain the prompt so the driver's write side stays open, then
                // push the canned lines and hold `server` open (dropping it would
                // EOF the stream and race the driver's terminal break).
                *sent.borrow_mut() = read_chunk(&mut server).await;
                for line in guest_lines {
                    server.write_all(line.as_bytes()).await.unwrap();
                    server.write_all(b"\n").await.unwrap();
                }
                server.flush().await.unwrap();
                server
            };

            let (result, _server) = tokio::join!(driver, feeder);
            result
        });

        (
            result,
            events.into_inner(),
            sent.into_inner(),
            touches.into_inner(),
        )
    }

    #[test]
    fn it_streams_reports_until_done() {
        // The terminal `done` stops the loop: the trailing `info` is never seen.
        let (result, events, sent, _) = drive_over_canned(
            "do it",
            1,
            &[
                r#"{"type":"report","status":"working","text":"building"}"#,
                r#"{"type":"report","status":"done","text":"shipped"}"#,
                r#"{"type":"report","status":"info","text":"after the end"}"#,
            ],
        );
        result.expect("a clean done is not an error");
        assert_eq!(
            events,
            vec![Ev::Report(Status::Working), Ev::Report(Status::Done)]
        );

        // The driver sent exactly one Prompt carrying the text + allocated id.
        let msg: HostMessage = serde_json::from_str(sent.trim()).expect("a HostMessage was sent");
        let HostMessage::Prompt(prompt) = msg;
        assert_eq!(prompt.text, "do it");
        assert_eq!(prompt.turn_id, 1);
    }

    #[test]
    fn it_stops_streaming_on_blocked() {
        let (result, events, _, _) = drive_over_canned(
            "go",
            1,
            &[
                r#"{"type":"report","status":"working","text":"trying"}"#,
                r#"{"type":"report","status":"blocked","text":"need a token"}"#,
                r#"{"type":"report","status":"working","text":"never reached"}"#,
            ],
        );
        result.expect("blocked is a clean terminal");
        assert_eq!(
            events,
            vec![Ev::Report(Status::Working), Ev::Report(Status::Blocked)]
        );
    }

    #[test]
    fn it_skips_blank_and_ready_lines_then_streams() {
        // Blank lines are ignored; a `ready` is a diagnostic, not a report; only
        // the actual reports reach the sink.
        let (result, events, _, _) = drive_over_canned(
            "x",
            1,
            &[
                "",
                r#"{"type":"ready"}"#,
                r#"{"type":"report","status":"info","text":"fyi"}"#,
                r#"{"type":"report","status":"done","text":"ok"}"#,
            ],
        );
        result.expect("ok");
        assert_eq!(
            events,
            vec![Ev::Report(Status::Info), Ev::Report(Status::Done)]
        );
    }

    #[test]
    fn a_heartbeat_produces_zero_orchestrator_events_but_touches_liveness() {
        // §8.1 invariant: a `Heartbeat` surfaces NO event to the sink (zero
        // orchestrator-facing bytes); it only resets the timer and silently
        // touches `liveness.json`.
        let (result, events, _, touches) = drive_over_canned(
            "go",
            1,
            &[
                r#"{"type":"heartbeat","seq":1}"#,
                r#"{"type":"report","status":"done","text":"ok"}"#,
            ],
        );
        result.expect("ok");
        assert_eq!(
            events,
            vec![Ev::Report(Status::Done)],
            "heartbeat surfaced no event"
        );
        assert_eq!(
            touches, 1,
            "the heartbeat triggered exactly one silent touch"
        );
    }

    #[test]
    fn it_renders_a_stopped_warning_on_turn_completed_unreported() {
        // §6.2: `TurnCompleted{reported:false}` for our turn → a single `Stopped`
        // verdict, then a clean break (not an error).
        let (result, events, _, _) = drive_over_canned(
            "go",
            5,
            &[r#"{"type":"turncompleted","turn_id":5,"reported":false}"#],
        );
        result.expect("an unreported stop is terminal, not an error");
        assert_eq!(events, vec![Ev::Stopped(5)]);
    }

    #[test]
    fn it_treats_turn_completed_reported_as_clean_success() {
        // `TurnCompleted{reported:true}` breaks cleanly with no extra event.
        let (result, events, _, _) = drive_over_canned(
            "go",
            5,
            &[r#"{"type":"turncompleted","turn_id":5,"reported":true}"#],
        );
        result.expect("a reported completion is success");
        assert!(
            events.is_empty(),
            "no warning on a clean completion: {events:?}"
        );
    }

    #[test]
    fn it_errors_when_the_stream_closes_mid_turn() {
        // EOF before any terminal: dropping the feeder's `server` closes the
        // stream → `next_line` yields None → transport-closed error.
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let result = runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);
            let driver = drive(
                client,
                1,
                "go".into(),
                relaxed_watchdog(),
                |_ev: DriveEvent| -> Result<()> { Ok(()) },
                || {},
            );
            let feeder = async move {
                // Drain the prompt, then let `server` drop at the end of this
                // block so the client side sees EOF.
                let _ = read_chunk(&mut server).await;
            };
            let (result, ()) = tokio::join!(driver, feeder);
            result
        });
        let err = result.expect_err("a mid-turn EOF must error");
        assert!(
            format!("{err:#}").contains("transport closed mid-turn"),
            "{err:#}"
        );
    }

    // ---- watchdog timers (canned channel, tier 2) ----
    //
    // These exercise the `select!` deadlines with *small real* durations rather
    // than a mocked clock: `tokio::time::{pause,advance}` need the `test-util`
    // feature, which the shared workspace `tokio` dep does not enable (and is out
    // of this issue's file scope to add). The deadline under test is shrunk to
    // tens of ms while the others are kept seconds away, so each test isolates one
    // timer deterministically.

    /// A watchdog with the named deadlines in milliseconds.
    fn wd_ms(heartbeat: u64, progress: u64, delivery: u64, retries: u32) -> Watchdog {
        Watchdog {
            heartbeat_deadline: Duration::from_millis(heartbeat),
            progress_deadline: Duration::from_millis(progress),
            delivery_deadline: Duration::from_millis(delivery),
            delivery_retries: retries,
        }
    }

    const LONG_MS: u64 = 30_000;

    #[test]
    fn it_errors_when_the_heartbeat_deadline_passes() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let result = runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);
            // Heartbeat deadline is the only short one; a `working` report accepts
            // the turn (disabling delivery) so the heartbeat timer is isolated.
            let driver = drive(
                client,
                1,
                "go".into(),
                wd_ms(60, LONG_MS, LONG_MS, 3),
                |_ev: DriveEvent| -> Result<()> { Ok(()) },
                || {},
            );
            let ctrl = async {
                let _ = read_chunk(&mut server).await;
                server
                    .write_all(br#"{"type":"report","status":"working","text":"x"}"#)
                    .await
                    .unwrap();
                server.write_all(b"\n").await.unwrap();
                server.flush().await.unwrap();
                // Hold the stream open (no heartbeat) past the deadline.
                tokio::time::sleep(Duration::from_millis(400)).await;
                server
            };
            let (result, _server) = tokio::join!(driver, ctrl);
            result
        });
        let err = result.expect_err("a missed heartbeat deadline must error");
        assert!(format!("{err:#}").contains("transport dead"), "{err:#}");
    }

    #[test]
    fn it_surfaces_a_progress_stall_once_then_keeps_streaming() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let events: RefCell<Vec<Ev>> = RefCell::new(Vec::new());
        let result = runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);
            let driver = drive(
                client,
                1,
                "go".into(),
                wd_ms(LONG_MS, 60, LONG_MS, 3),
                |event: DriveEvent| -> Result<()> {
                    events.borrow_mut().push(match event {
                        DriveEvent::Report(r) => Ev::Report(r.status),
                        DriveEvent::Stalled(_) => Ev::Stalled,
                        DriveEvent::Stopped(id) => Ev::Stopped(id),
                    });
                    Ok(())
                },
                || {},
            );
            let ctrl = async {
                let _ = read_chunk(&mut server).await;
                // Accept the turn (disable delivery), then go quiet for two stall
                // windows: the notice must surface exactly once, not per window.
                server
                    .write_all(br#"{"type":"report","status":"working","text":"x"}"#)
                    .await
                    .unwrap();
                server.write_all(b"\n").await.unwrap();
                server.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(300)).await;
                server
                    .write_all(br#"{"type":"turncompleted","turn_id":1,"reported":true}"#)
                    .await
                    .unwrap();
                server.write_all(b"\n").await.unwrap();
                server.flush().await.unwrap();
                server
            };
            let (result, _server) = tokio::join!(driver, ctrl);
            result
        });
        result.expect("a stall surfaces a notice, it does not error");
        let events = events.into_inner();
        assert_eq!(
            events.iter().filter(|e| matches!(e, Ev::Stalled)).count(),
            1,
            "the stall must surface exactly once per episode: {events:?}"
        );
    }

    #[test]
    fn it_resends_the_prompt_until_the_turn_is_accepted() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let (result, p1, p2) = runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);
            let driver = drive(
                client,
                7,
                "go".into(),
                wd_ms(LONG_MS, LONG_MS, 80, 3),
                |_ev: DriveEvent| -> Result<()> { Ok(()) },
                || {},
            );
            let ctrl = async {
                let p1 = read_chunk(&mut server).await; // first delivery
                let p2 = read_chunk(&mut server).await; // the resent (identical) prompt
                                                        // Now accept, so no further resend fires, and end cleanly.
                server
                    .write_all(br#"{"type":"turnaccepted","turn_id":7}"#)
                    .await
                    .unwrap();
                server.write_all(b"\n").await.unwrap();
                server
                    .write_all(br#"{"type":"turncompleted","turn_id":7,"reported":true}"#)
                    .await
                    .unwrap();
                server.write_all(b"\n").await.unwrap();
                server.flush().await.unwrap();
                (p1, p2, server)
            };
            let (result, (p1, p2, _server)) = tokio::join!(driver, ctrl);
            (result, p1, p2)
        });
        result.expect("a turn accepted after a resend is success");
        // Both deliveries are the identical Prompt{turn_id:7} (so the guest's
        // turn_id dedupe makes the resend safe, design §7.2).
        for raw in [&p1, &p2] {
            let HostMessage::Prompt(p) =
                serde_json::from_str::<HostMessage>(raw.trim()).expect("a Prompt");
            assert_eq!(p.turn_id, 7);
            assert_eq!(p.text, "go");
        }
    }

    #[test]
    fn it_fails_clearly_when_resends_are_exhausted() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let result = runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);
            // Never accept: two resends, then the third deadline fails clearly.
            let driver = drive(
                client,
                9,
                "go".into(),
                wd_ms(LONG_MS, LONG_MS, 50, 2),
                |_ev: DriveEvent| -> Result<()> { Ok(()) },
                || {},
            );
            let ctrl = async {
                let _ = read_chunk(&mut server).await; // first delivery
                tokio::time::sleep(Duration::from_millis(500)).await;
                server
            };
            let (result, _server) = tokio::join!(driver, ctrl);
            result
        });
        let err = result.expect_err("exhausted resends must fail");
        assert!(format!("{err:#}").contains("not accepted"), "{err:#}");
    }
}
