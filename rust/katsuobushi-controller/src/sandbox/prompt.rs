//! `katsuctl sandbox prompt` — push one prompt to an instance and stream its
//! reports. Absorbs and retires the standalone
//! `katsuobushi-sandbox-prompt` host client (`prompt.rs`): its `drive()`
//! streaming loop and the readiness-wait move here.
//!
//! The flow has three branches, decided in [`prompt_core`] so they are testable
//! against a [`FakeHost`](crate::sandbox::host::FakeHost) without a VM:
//!
//! - **running** (QMP answers): connect over vsock and stream `Report`s until a
//!   terminal status (`done`/`blocked`);
//! - **paused + named** (QMP silent, `instance.json.named`): the VM is powered
//!   off but kept on disk, so resume it by re-running our own `start` subcommand
//!   (`--agent --name <inst>`, boot only — *no* `--prompt`) and `exec`ing the
//!   boot recipe it emits, then fall through to the same vsock delivery the
//!   running branch uses so `katsuctl` itself streams the turn once the channel
//!   arms. (The shell `start` runner no longer delivers `--prompt`; delivering it
//!   here keeps restart self-contained — the turn is never silently dropped.);
//! - **not running + ephemeral**: there is nothing to resume — error clearly.
//!
//! The vsock streaming keeps the proven async/tokio + `tokio-vsock` machinery
//! from the old client (its own current-thread runtime), rather than routing
//! through [`Host::vsock_connect`](crate::sandbox::host::Host::vsock_connect)
//! (whose runtime is private). A freshly-booted instance needs ~30–60s before
//! vsock answers, so the connect retries with backoff (the old runner's
//! `--probe` loop) — a successful connect *is* the
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

use crate::output::{Renderer, ReportKind, Reported};
use crate::sandbox::host::{Host, HostImpl};
use crate::sandbox::instance::{self, Instance};
use crate::sandbox::liveness::{alloc_turn_id, now_rfc3339, Liveness};
use crate::sandbox::report_log;
use crate::sandbox::resolve::resolve_instance;
use crate::sandbox::spec::{load_spec, resolve_roots, ResolvedRoots, Spec};
use crate::Global;

/// How many times [`connect_with_retry`] attempts the vsock connect before giving
/// up, and the backoff cap between tries. With the 250ms→2s schedule this is a
/// ~3-minute readiness budget, matching the old runner's `for _ in $(seq 1 180)`
/// `--probe` loop so a just-booted instance is handled.
const READINESS_TRIES: usize = 90;
const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(2);

/// The host watchdog's three deadlines plus the resend budget, and the phase-0
/// ready-gate bound, resolved from the spec tunables. Carried into [`drive`] so
/// its `select!` timers are driven by data, not magic numbers — and so a test
/// can shrink them.
#[derive(Debug, Clone, Copy)]
struct Watchdog {
    /// `readyGateSecs`: how long [`drive`]'s phase-0 ready-gate waits for the
    /// guest's `SessionReady` before sending the first `Prompt` anyway.
    ready_gate: Duration,
    /// `heartbeatSecs * heartbeatMiss`: no `Heartbeat` within this window ⇒ the
    /// transport is dead (error).
    heartbeat_deadline: Duration,
    /// `progressStallSecs`: no `Report`/lifecycle within this window ⇒ surface a
    /// first "no reports" notice (no break; a second, stronger one follows at
    /// 3x this if the silence continues).
    progress_deadline: Duration,
    /// `deliveryDeadlineSecs`: no `TurnAccepted` within this window ⇒ resend the
    /// identical `Prompt`.
    delivery_deadline: Duration,
    /// `deliveryRetries`: max resends before the delivery fails clearly.
    delivery_retries: u32,
}

impl Watchdog {
    /// Resolve the deadlines from the Nix-rendered spec tunables.
    fn from_spec(spec: &Spec) -> Self {
        Self {
            ready_gate: Duration::from_secs(spec.ready_gate_secs),
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
/// emits zero bytes on a tick. The transport-dead and resend-
/// exhausted verdicts are not here either — they are terminal `Err`s, rendered as
/// `Lost` by [`deliver_over_vsock`].
enum DriveEvent<'a> {
    /// A relayed agent `Report` (working/info/done/blocked).
    Report(&'a Report),
    /// A progress-stall notice: the neutral "no reports for T" first, then a
    /// stronger one at 3T. At most [`STALL_NOTICES`] per silent episode.
    Stalled(&'a str),
    /// The `reported:false` verdict for this `turn_id` — the agent stopped
    /// without a terminal report. Terminal (the drive loop breaks).
    Stopped(u64),
    /// `--until-report`: the agent's turn ended unreported, but the drive loop
    /// stays armed and keeps waiting for a real terminal report rather than
    /// breaking. Emitted once per unreported turn-end in that mode.
    ReArmed(u64),
}

/// Which branch [`prompt_core`] took — returned so seam tests can assert the
/// decision without inspecting side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// The instance was running; the prompt was delivered over vsock to `cid`.
    Delivered { cid: u32 },
    /// The instance was paused but named; it was resumed and then the prompt was
    /// delivered over vsock (restart is self-contained — it does not rely on
    /// `start --prompt`).
    Restarted,
}

/// Production entry point: load the spec, stand up the host seam, then run the
/// branch logic with the real `instance.json` read, the real `start`-subcommand
/// resume (boot only), and the real vsock streaming delivery.
pub fn run(
    config: &Path,
    instance: &str,
    text: Vec<String>,
    until_report: bool,
    redeliver: bool,
    global: Global,
) -> Result<()> {
    let spec = load_spec(config)?;
    let roots = resolve_roots(&spec.roots)?;
    let host = HostImpl::new().context("initializing the host IO seam")?;
    let renderer = Renderer::resolve(global);

    let state_glob = roots.state_glob.as_path();
    // `--redeliver` resolves the instance name first (it accepts an index, like
    // every other subcommand) and then reads that instance's persisted
    // directive. Delivery is otherwise identical to a typed prompt: a fresh turn
    // id from the liveness counter, normal delivery-ack semantics.
    let text = if redeliver {
        let name = crate::sandbox::resolve::resolve_instance(state_glob, &host, instance)?;
        let directive = crate::sandbox::directive::read(state_glob, &name)?;
        eprintln!("redelivering the launch directive to '{name}'");
        directive
    } else {
        text.join(" ")
    };
    let port = spec.vsock_port;
    let watchdog = Watchdog::from_spec(&spec);

    let katsuctl = spec.tools.katsuctl.as_path();
    let bash = spec.tools.bash.as_path();

    prompt_core(
        &host,
        &roots,
        instance,
        &text,
        |inst| instance::read(state_glob, inst),
        |inst| resume_via_start(katsuctl, config, bash, inst),
        |cid, inst| {
            // The turn id is allocated from (and the heartbeat record written to)
            // `liveness.json` beside the instance's `instance.json`.
            let liveness_path = state_glob.join(inst).join("liveness.json");
            let turn_id = alloc_turn_id(&host, &liveness_path)?;
            deliver_over_vsock(
                &host,
                cid,
                port,
                turn_id,
                &text,
                watchdog,
                until_report,
                &liveness_path,
                state_glob,
                inst,
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
/// runner no longer delivers `--prompt`; that lands natively in ), so the
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
    // (mirrors the old `vsock-cid` readability guard).
    let Some(cid) = meta.vsock_cid else {
        bail!("sandbox prompt: no control channel for {inst:?} (is it an --agent instance?)");
    };

    // Liveness is derived from QMP, never stored: a live qemu monitor
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
/// the host seam. A terminal `Err` (transport dead / resend
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
    until_report: bool,
    liveness_path: &Path,
    state_dir: &Path,
    instance_name: &str,
    renderer: &Renderer,
) -> Result<()> {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime for the vsock prompt")?;
    let stream = connect_with_retry(&runtime, cid, port)?;
    set_stream_active(host, liveness_path, true);

    let sink = |event: DriveEvent| -> Result<()> {
        // Journal first, then render. This sink is the single point every
        // streamed event passes through, so appending here is what makes a
        // terminal report's *text* survive the loss of this process's stdout
        // (closed terminal, dead tmux pane, killed orchestrator). Best-effort
        // and additive: `--json` consumers see an unchanged stream.
        journal_event(host, state_dir, instance_name, &event);
        match event {
            DriveEvent::Report(report) => render_report(renderer, report),
            DriveEvent::Stalled(text) => render_note(renderer, ReportKind::Stalled, text),
            DriveEvent::Stopped(turn_id) => {
                render_note(renderer, ReportKind::Stopped, &stopped_message(turn_id))
            }
            DriveEvent::ReArmed(turn_id) => render_rearmed(renderer, turn_id),
        }
    };
    // The heartbeat touch: load-modify-store the liveness record with a fresh
    // timestamp from the clock seam. Best-effort — a failed write never fails the
    // turn — and silent (no render/print).
    let touch = || {
        // A corrupt record skips the touch rather than clobbering it — a
        // defaulted rewrite would rewind the persisted turn-id counter.
        let Ok(mut liveness) = Liveness::load(host, liveness_path) else {
            return;
        };
        if let Ok(stamp) = now_rfc3339(host) {
            liveness.last_heartbeat_at = Some(stamp);
        }
        liveness.stream_active = true;
        let _ = liveness.store(host, liveness_path);
    };

    let result = runtime.block_on(drive(
        stream,
        turn_id,
        instance_name,
        text.to_string(),
        watchdog,
        until_report,
        sink,
        touch,
    ));
    set_stream_active(host, liveness_path, false);

    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            // Render the single `Lost` line here, then hand `main` the marker
            // so it exits nonzero without re-printing anyhow's chain —
            // process exit stays in `main`, not deep in a helper.
            let _ = render_note(renderer, ReportKind::Lost, &format!("{e:#}"));
            Err(anyhow::Error::new(Reported))
        }
    }
}

/// Flip `streamActive` in `liveness.json` (best-effort) so `status` can tell an
/// attached `drive` from a stale record. Preserves the rest of the
/// record via load-modify-store.
fn set_stream_active(host: &impl Host, path: &Path, active: bool) {
    // Best-effort, but never clobber: a corrupt record skips the flip rather
    // than rewriting it from Default (which would rewind the turn-id counter).
    let Ok(mut liveness) = Liveness::load(host, path) else {
        return;
    };
    liveness.stream_active = active;
    let _ = liveness.store(host, path);
}

/// The message for a turn that stopped without a terminal report.
///
/// Only reachable without `--until-report`, which is now the interactive
/// `sandbox prompt` default (the orchestration flows stay armed). So it is
/// addressed to a watching operator, and it carries the two recoveries: re-run
/// armed, or look for a report the guest's auto-nudges land *after* this
/// process exits — the turn is not necessarily over just because the command
/// returned.
fn stopped_message(turn_id: u64) -> String {
    format!(
        "agent stopped without reporting (turn {turn_id}) — possible silent \
         completion or unreported hang; inspect with `sandbox attach` / `sandbox fetch`. \
         The guest keeps auto-nudging it, so a report may still land after this command \
         exits: re-run with `--until-report` to wait for it, or check the instance's \
         `turn-state.json` (and `sandbox status`) for a later `ended-ok`."
    )
}

/// The message for `--until-report`: the turn ended unreported but the drive
/// loop stays armed for a real terminal report (e.g. after the guest's
/// auto-nudges, or once a backgrounded build finishes and the agent reports).
fn rearmed_message(turn_id: u64) -> String {
    format!(
        "turn {turn_id} ended without a terminal report — staying armed \
         (--until-report); still waiting for `report done`/`blocked`"
    )
}

/// Render the `--until-report` re-armed note: a tagged `{"event":"rearmed",…}`
/// line in `--json`, a dim ⚠ line otherwise (reusing the `Stalled` glyph — this
/// is a "still waiting" notice, not a failure).
fn render_rearmed(renderer: &Renderer, turn_id: u64) -> Result<()> {
    #[derive(Serialize)]
    struct Note<'a> {
        event: &'a str,
        text: &'a str,
    }
    let text = rearmed_message(turn_id);
    renderer.emit_progress(
        &Note {
            event: "rearmed",
            text: &text,
        },
        |r| r.report(ReportKind::Stalled, &text),
    )
}

/// Render a watchdog notice (Stalled/Stopped/Lost) through the shared renderer:
/// `--json` emits a tagged `{"event":…,"text":…}` line (the NDJSON stream's
/// out-of-band note), human mode paints the glyph line.
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
    renderer.emit_progress(&Note { event, text }, |r| r.report(kind, text))
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

/// Whether the phase-1 stream loop should keep streaming or break on a terminal.
/// Returned by [`handle_phase1_line`] so the same line-handling serves both a
/// stashed ready-gate line and the `select!` loop's stream reads.
enum LineFlow {
    Continue,
    Break,
}

/// Handle one decoded guest line in the phase-1 stream loop,
/// mutating the watchdog state in place and surfacing events through `sink`:
///
/// - `Heartbeat` is **silent** — reset `last_hb` + a throttled (≤1/s) `touch()`
///   of `liveness.json`, no render.
/// - a `working`/`info` `Report` resets the stall timer, re-arms the
///   notice, and counts as the implicit delivery ack (fallback).
/// - a `done`/`blocked` `Report` relays then [`LineFlow::Break`]s (clean terminal).
/// - `TurnAccepted`/`TurnCompleted` for *this* `turn_id` ack / terminate (the
///   latter warning via `Stopped` when `reported = false`).
/// - everything else is tolerated/diagnostic (a per-connect `ready`, a late
///   `SessionReady`, stale-`turn_id` lifecycle, an unknown variant, undecodable
///   bytes) — none reach the orchestrator.
///
/// Factored out of the `select!` arm so a line stashed by the phase-0 ready-gate
/// (a fast agent's first report) is fed through the *identical* logic, never lost.
#[allow(clippy::too_many_arguments)]
fn handle_phase1_line<Sink, Touch>(
    line: &str,
    turn_id: u64,
    until_report: bool,
    sink: &mut Sink,
    touch: &mut Touch,
    last_hb: &mut Instant,
    last_prog: &mut Instant,
    last_touch: &mut Option<Instant>,
    accepted: &mut bool,
    stalls: &mut u32,
) -> Result<LineFlow>
where
    Sink: FnMut(DriveEvent) -> Result<()>,
    Touch: FnMut(),
{
    match serde_json::from_str::<GuestMessage>(line) {
        Ok(GuestMessage::Heartbeat { .. }) => {
            // invariant: SILENT — reset the deadline and a throttled (≤1/s)
            // liveness touch only. No render, no print.
            *last_hb = Instant::now();
            let due =
                last_touch.is_none_or(|t| last_hb.duration_since(t) >= Duration::from_secs(1));
            if due {
                touch();
                *last_touch = Some(*last_hb);
            }
        }
        Ok(GuestMessage::Report(report)) => {
            // A report naming a *different* turn is stale replay (a late `done`
            // from a prior turn must neither satisfy this turn's delivery ack
            // nor terminate it): diagnostic only, like other stale lifecycle.
            // One with no id keeps the ordering-based correlation.
            if report.turn_id.is_some_and(|id| id != turn_id) {
                eprintln!(
                    "· stale report for turn {} (current turn {turn_id}): {}",
                    report.turn_id.unwrap_or_default(),
                    report.text
                );
                return Ok(LineFlow::Continue);
            }
            match report.status {
                Status::Working | Status::Info => {
                    // Progress: reset the stall timer, re-arm the notice
                    // escalation from the start, and treat it as the implicit
                    // delivery ack (fallback).
                    *last_prog = Instant::now();
                    *stalls = 0;
                    *accepted = true;
                    sink(DriveEvent::Report(&report))?;
                }
                Status::Done | Status::Blocked => {
                    sink(DriveEvent::Report(&report))?;
                    return Ok(LineFlow::Break); // clean terminal
                }
            }
        }
        Ok(GuestMessage::TurnAccepted { turn_id: id }) if id == turn_id => {
            *accepted = true;
        }
        Ok(GuestMessage::TurnCompleted {
            turn_id: id,
            reported,
        }) if id == turn_id => {
            if !reported {
                // Stopped without a terminal report. `--until-report` keeps the
                // stream armed for a real report (the guest's auto-nudges, or a
                // late `report done` once a backgrounded build finishes) rather
                // than breaking; the default surfaces the warning and breaks.
                if until_report {
                    sink(DriveEvent::ReArmed(turn_id))?;
                    return Ok(LineFlow::Continue);
                }
                sink(DriveEvent::Stopped(turn_id))?;
            }
            return Ok(LineFlow::Break);
        }
        // Tolerated/diagnostic — none reach the orchestrator: a per-connect `ready`,
        // a late `SessionReady`, lifecycle for a stale `turn_id`, an unknown newer
        // variant, or undecodable bytes.
        Ok(GuestMessage::Ready) => eprintln!("· guest ready"),
        Ok(GuestMessage::WorkStateTransition {
            work_state,
            is_late,
            label,
            duration_secs,
        }) => {
            let detail = match (label.as_deref(), duration_secs) {
                (Some(l), Some(d)) => format!(" ({l}, {d}s)"),
                (Some(l), None) => format!(" ({l})"),
                _ => String::new(),
            };
            let late = if is_late { ", late" } else { "" };
            eprintln!("· work state: {work_state}{late}{detail}");
        }
        Ok(_) => {}
        Err(e) => eprintln!("· undecodable guest line: {e}"),
    }
    Ok(LineFlow::Continue)
}

/// The host watchdog. Sends `Prompt{turn_id}`
/// over `stream`, then runs a `select!` loop over the guest line stream plus three
/// deadline timers, until a terminal condition:
///
/// - **heartbeat-deadline** (`heartbeat_deadline`): no `Heartbeat` in the window
///   ⇒ the transport is dead ⇒ `Err` (rendered `Lost`).
/// - **progress-deadline** (`progress_deadline`): no `Report`/lifecycle in the
///   window ⇒ surface the `Stalled` notice **once** per episode (no break, no
///   kill); cleared by the next `working`/`info` report.
/// - **delivery-deadline** (`delivery_deadline`): no `TurnAccepted` yet ⇒ resend
///   the identical `Prompt` up to `delivery_retries`, then `Err` clearly.
///
/// A `Heartbeat` is handled **silently** — reset `last_hb` + a throttled (≤1/s)
/// `touch()` of `liveness.json` — and reaches the orchestrator as zero bytes.
/// Terminal breaks: a `done`/`blocked` `Report`, `TurnCompleted{true}`
/// (clean), or `TurnCompleted{false}` (the `Stopped` warning). EOF (`None`)
/// is a transport-closed-mid-turn `Err`.
///
/// A **phase-0 ready-gate** precedes the send: `drive` waits up to
/// `ready_gate` for the guest's `SessionReady` — latched and replayed on each
/// control connect, so an already-armed agent passes instantly — before
/// the first `Prompt`, tolerating `Ready`/`Heartbeat` and stashing any other line
/// (the agent is already moving) into phase-1 so it is not lost. The gate elapsing
/// proceeds anyway; the ack-and-resend is the delivery guarantee. `Ready` is
/// thereby demoted to "transport accepted" — it no longer authorizes a prompt.
/// Generic over the transport so a test drives it with an in-memory duplex.
#[allow(clippy::too_many_arguments)]
async fn drive<S, Sink, Touch>(
    stream: S,
    turn_id: u64,
    instance: &str,
    text: String,
    watchdog: Watchdog,
    until_report: bool,
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

    // Encode once so a resend replays the *identical* Prompt bytes — the
    // guest dedupes on `turn_id`, so a resend racing a slow first delivery is
    // dropped harmlessly.
    let prompt_line = {
        let mut line = serde_json::to_vec(&HostMessage::Prompt(Prompt { turn_id, text }))?;
        line.push(b'\n');
        line
    };

    let Watchdog {
        ready_gate,
        heartbeat_deadline,
        progress_deadline,
        delivery_deadline,
        delivery_retries,
    } = watchdog;

    // Phase 0: the bounded ready-gate. Wait up to `ready_gate` for the
    // guest's `SessionReady` — latched and replayed on each control connect,
    // so a prompt to an already-armed agent passes instantly, while one right after
    // boot waits for real arming (closing the startup race). `Ready`/`Heartbeat`
    // mean "transport up", not "agent armed" — consume and keep waiting; any other
    // line means the agent is already producing turn output, so stash it for phase-1
    // (never lost) and stop waiting. The gate elapsing sends anyway — 's
    // ack-and-resend covers a still-unarmed agent.
    let gate_deadline = Instant::now() + ready_gate;
    let mut stashed: Option<String> = None;
    loop {
        tokio::select! {
            read = lines.next_line() => {
                match read.context("read guest (ready-gate)")? {
                    // The transport died before we even sent — error, never wait.
                    None => bail!(
                        "transport closed during the ready-gate (guest stream EOF before session ready)"
                    ),
                    Some(raw) => {
                        let line = raw.trim();
                        if line.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<GuestMessage>(line) {
                            // The arm we waited for: the agent's REPL is armed + idle.
                            Ok(GuestMessage::SessionReady) => break,
                            // Transport up, not arming — tolerate/consume, keep waiting.
                            Ok(GuestMessage::Ready | GuestMessage::Heartbeat { .. }) => continue,
                            // Anything else (a first report, lifecycle, undecodable):
                            // the agent is already moving — stash it for phase-1 and go.
                            _ => {
                                stashed = Some(line.to_string());
                                break;
                            }
                        }
                    }
                }
            }
            _ = sleep_until(gate_deadline) => break, // proceed anyway
        }
    }

    // Phase 1: deliver the prompt, then stream. The deadlines below are measured
    // from *here* (post-gate), so the ready-wait never counts against them.
    write_half
        .write_all(&prompt_line)
        .await
        .context("send prompt")?;
    write_half.flush().await.ok();

    let mut last_hb = Instant::now();
    let mut last_prog = Instant::now();
    let mut sent = Instant::now();
    let mut last_touch: Option<Instant> = None;
    let mut accepted = false;
    let mut resends: u32 = 0;
    let mut stalls: u32 = 0;

    // A fast agent may have produced its first line *during* the ready-gate;
    // stashed it rather than dropping it. Feed it through the identical handler the
    // stream loop uses before awaiting anything more — it may itself be terminal.
    if let Some(line) = stashed.take() {
        if let LineFlow::Break = handle_phase1_line(
            &line,
            turn_id,
            until_report,
            &mut sink,
            &mut touch,
            &mut last_hb,
            &mut last_prog,
            &mut last_touch,
            &mut accepted,
            &mut stalls,
        )? {
            return Ok(());
        }
    }

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
                        if let LineFlow::Break = handle_phase1_line(
                            line,
                            turn_id,
                            until_report,
                            &mut sink,
                            &mut touch,
                            &mut last_hb,
                            &mut last_prog,
                            &mut last_touch,
                            &mut accepted,
                            &mut stalls,
                        )? {
                            break;
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
            _ = sleep_until(stall_notice_at(last_prog, progress_deadline, stalls)), if stalls < STALL_NOTICES => {
                stalls += 1;
                let note = stall_message(progress_deadline, stalls);
                sink(DriveEvent::Stalled(&note))?;
                // Never a break and never a kill — the escalation only changes
                // what is said, not what is done.
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
                    // Distinguish harness-wedge from transport failure. The
                    // guest confirms every injection, and it now also re-injects
                    // a bounded number of times on its own — so reaching here
                    // means the notification kept landing and the harness kept
                    // not starting a turn. More resends cannot help; only a
                    // fresh session can. Naming that (and the exact commands)
                    // is the difference between a recoverable stall and a
                    // dead-end error.
                    bail!(
                        "turn {turn_id} was delivered to the agent harness but never began \
                         (no hook, no report) — after {delivery_retries} resends and the \
                         guest's bounded re-injections. This is a wedged harness, not a \
                         broken transport, so re-prompting will not help. Restart the \
                         session:\n  sandbox stop {instance}\n  sandbox start --agent --name \
                         {instance}\nthen re-send the turn (`sandbox prompt {instance} \
                         --redeliver` if it was dispatched). The restarted agent is a fresh \
                         session, so the directive must stand on its own."
                    );
                }
            }
        }
    }
    Ok(())
}

/// How many progress notices one silent episode may produce. The first is
/// neutral, the second (much later) is the actual warning; after that the
/// episode goes quiet rather than nagging.
const STALL_NOTICES: u32 = 2;

/// When the next progress notice is due, given how many have already fired in
/// this episode.
///
/// The first lands at the configured window. The second lands at **three times**
/// it, which is what makes escalating honest: only `working`/`info` reports
/// reset the clock (heartbeats are deliberately silent), so an agent inside one
/// long foreground call — a cold workspace compile is routinely 15-25 minutes —
/// looks idle however hard it is working. One window of silence is normal; three
/// consecutive ones is worth a real warning.
fn stall_notice_at(last_prog: Instant, window: Duration, stalls: u32) -> Instant {
    match stalls {
        0 => last_prog + window,
        _ => last_prog + window * 3,
    }
}

/// The text of the `n`-th progress notice in an episode.
///
/// The first fire informs; it must NOT assert the agent may be stuck. In the
/// field the old always-alarming wording fired on essentially every cold launch
/// and was benign every time — which trained the operator toward alarm, and
/// their one alarmed response was the destructive one (killing a live
/// provisioning step). A watchdog that always barks is worse than none.
fn stall_message(window: Duration, n: u32) -> String {
    let secs = window.as_secs();
    if n < STALL_NOTICES {
        format!(
            "no reports for {secs}s — normal during long builds (only reports reset this \
             clock, so a single long tool call looks idle); inspect with `sandbox attach` if \
             unexpected"
        )
    } else {
        format!(
            "still no reports after {}s — the agent may be stuck; inspect with `sandbox attach` \
             (raise `progressStallSecs` if this project's builds are simply this long)",
            secs * 3
        )
    }
}

/// Append one drive event to the instance's report journal.
///
/// Best-effort in both directions: a `Stalled` notice is a *watchdog* opinion
/// rather than something the agent said, so it is deliberately not journaled;
/// and any failure to write warns once on stderr and is otherwise swallowed —
/// the journal must never be able to break a live drive.
fn journal_event(host: &impl Host, state_dir: &Path, name: &str, event: &DriveEvent) {
    let (turn_id, status, text) = match event {
        DriveEvent::Report(r) => (
            r.turn_id,
            match r.status {
                Status::Working => "working",
                Status::Done => "done",
                Status::Blocked => "blocked",
                Status::Info => "info",
            },
            r.text.clone(),
        ),
        DriveEvent::Stopped(id) => (Some(*id), "stopped", stopped_message(*id)),
        DriveEvent::ReArmed(id) => (Some(*id), "re-armed", rearmed_message(*id)),
        DriveEvent::Stalled(_) => return,
    };
    let at = now_rfc3339(host).unwrap_or_default();
    let entry = report_log::Entry {
        at,
        turn_id,
        status: status.to_string(),
        text,
    };
    if let Err(e) = report_log::append(state_dir, name, &entry) {
        eprintln!("sandbox: could not journal the report (continuing): {e:#}");
    }
}

/// Render one streamed report: `--json` emits the `Report` as one line of NDJSON
/// (the existing wire format); human output uses the status glyph/color.
/// Both go through [`Renderer::emit`], which serializes in `--json`
/// mode and paints (gated) otherwise.
fn render_report(renderer: &Renderer, report: &Report) -> Result<()> {
    let kind = match report.status {
        Status::Working => ReportKind::Working,
        Status::Done => ReportKind::Done,
        Status::Blocked => ReportKind::Blocked,
        Status::Info => ReportKind::Info,
    };
    renderer.emit_progress(report, |r| r.report(kind, &report.text))
}

/// The `katsuctl … start` argv that resumes a paused named instance: run the
/// `start` subcommand against this same spec, launching the detached agent VM
/// under its full (already-suffixed) name, with **no** `--prompt`. Passing the
/// verbatim suffixed name resumes that exact instance (rather than minting a fresh
/// one); the turn is delivered separately by the caller's `deliver` over vsock, so
/// `--prompt` must not appear here. Factored out so the argv shape is
/// unit-testable without spawning anything.
fn resume_via_start_args(config: &Path, inst: &str) -> Vec<String> {
    vec![
        "sandbox".to_string(),
        "--config".to_string(),
        config.to_string_lossy().into_owned(),
        "start".to_string(),
        "--agent".to_string(),
        "--name".to_string(),
        inst.to_string(),
    ]
}

/// Resume a paused named instance by re-running our own `start` subcommand and
/// then `exec`ing the recipe it emits — the same emit+exec dance the `sandbox
/// start` menu wrapper performs, done inline so restart depends on **no** menu
/// command being on PATH (the old code shelled out to a `sandbox:start` binary
/// that the 0.2.8 command-tree rename removed). `start --agent` emits only the
/// path of a flat boot recipe on stdout (stderr carries any planning error); we
/// run `bash` on that path to actually detach the VM. Both `katsuctl` and `bash`
/// are the pinned store paths from the spec, so this works from a shell that has
/// neither on PATH.
///
/// No `--prompt`: the boot only launches the detached VM. The no-prompt agent
/// launch returns promptly after detaching, so this spawns-and-waits and then
/// returns to `prompt_core`, which streams the turn over vsock once the channel
/// arms — making restart self-contained rather than depending on `start` to
/// deliver `--prompt` (which it no longer does).
fn resume_via_start(katsuctl: &Path, config: &Path, bash: &Path, inst: &str) -> Result<()> {
    eprintln!(
        "sandbox prompt: {inst:?} is paused — resuming it to deliver this turn \
         (boot + arm ~30-60s)..."
    );
    let out = Command::new(katsuctl)
        .args(resume_via_start_args(config, inst))
        .output()
        .with_context(|| {
            format!("running {katsuctl:?} start to resume paused instance {inst:?}")
        })?;
    if !out.status.success() {
        // Surface katsuctl's own planning error (it wrote nothing to stdout).
        std::io::Write::write_all(&mut std::io::stderr(), &out.stderr).ok();
        bail!(
            "planning the resume of {inst:?} failed (exit {})",
            out.status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string())
        );
    }
    let script = String::from_utf8_lossy(&out.stdout);
    let script = script.trim();
    if script.is_empty() {
        bail!("resume of {inst:?} emitted no boot recipe path");
    }
    let status = Command::new(bash)
        .arg(script)
        .status()
        .with_context(|| format!("running the boot recipe {script:?} to resume {inst:?}"))?;
    if !status.success() {
        bail!(
            "the boot recipe failed to resume {inst:?} (exit {})",
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
            instance_version: crate::sandbox::instance::SUPPORTED_INSTANCE_VERSION,
            name: name.to_string(),
            mode: Mode::Agent,
            named,
            ssh_port: 2222,
            vsock_cid: Some(4242),
            graphics: None,
            seed: None,
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
        // dropped waiting on `start --prompt` (which no longer delivers).
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
        // The resume argv re-invokes our own `start` subcommand against this same
        // spec (--config), and must NOT carry the prompt: start only boots,
        // katsuctl delivers. Asserting the exact args guards against
        // re-introducing --prompt (which the shell start runner silently drops,
        // dropping the turn) and against regressing to the removed `sandbox:start`
        // menu binary.
        let args = resume_via_start_args(Path::new("/spec.json"), "katsuobushi-20260627-abc123");
        assert_eq!(
            args,
            vec![
                "sandbox".to_string(),
                "--config".to_string(),
                "/spec.json".to_string(),
                "start".to_string(),
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
        ReArmed(u64),
    }

    /// Deadlines so wide the watchdog timers never fire during a canned feed; the
    /// ready-gate is short so a feed with no `SessionReady` reaches phase-1 promptly
    /// (the gate's own behavior is exercised separately below).
    fn relaxed_watchdog() -> Watchdog {
        Watchdog {
            ready_gate: Duration::from_millis(20),
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
                "inst-a",
                prompt.to_string(),
                relaxed_watchdog(),
                false,
                |event: DriveEvent| -> Result<()> {
                    events.borrow_mut().push(match event {
                        DriveEvent::Report(r) => Ev::Report(r.status),
                        DriveEvent::Stalled(_) => Ev::Stalled,
                        DriveEvent::Stopped(id) => Ev::Stopped(id),
                        DriveEvent::ReArmed(id) => Ev::ReArmed(id),
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
    fn it_ignores_a_stale_report_naming_a_previous_turn() {
        // A late `done` from turn 1 replayed while driving turn 2 must neither
        // terminate the stream nor reach the sink as this turn's output; the
        // id-less `done` (ordering-correlated) still ends it cleanly.
        let (result, events, _, _) = drive_over_canned(
            "go",
            2,
            &[
                r#"{"type":"report","status":"done","text":"old turn","turn_id":1}"#,
                r#"{"type":"report","status":"working","text":"now","turn_id":2}"#,
                r#"{"type":"report","status":"done","text":"ok"}"#,
            ],
        );
        result.expect("the stale done must not fail or end the live turn");
        assert_eq!(
            events,
            vec![Ev::Report(Status::Working), Ev::Report(Status::Done)]
        );
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
    fn a_work_state_transition_produces_no_orchestrator_event() {
        // AC1 + AC4: a `WorkStateTransition` is a diagnostic printed to stderr;
        // it must NOT reach the sink (not journaled, not rendered).
        let (result, events, _, _) = drive_over_canned(
            "go",
            1,
            &[
                r#"{"type":"workstatetransition","work_state":"active","is_late":false,"label":"agent-work","duration_secs":42}"#,
                r#"{"type":"report","status":"done","text":"ok"}"#,
            ],
        );
        result.expect("a WorkStateTransition followed by done is not an error");
        assert_eq!(
            events,
            vec![Ev::Report(Status::Done)],
            "WorkStateTransition must not surface an orchestrator event"
        );
    }

    #[test]
    fn a_heartbeat_produces_zero_orchestrator_events_but_touches_liveness() {
        // invariant: a `Heartbeat` surfaces NO event to the sink (zero
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
        // `TurnCompleted{reported:false}` for our turn → a single `Stopped`
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
    fn it_stays_armed_across_an_unreported_stop_with_until_report() {
        // `--until-report`: an unreported turn-end emits a `ReArmed` note and
        // keeps streaming instead of breaking; a later terminal report (e.g. the
        // guest's auto-nudge landing, or a backgrounded build finishing) then
        // breaks cleanly. Uses a direct `drive` call so `until_report` is set.
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let events: RefCell<Vec<Ev>> = RefCell::new(Vec::new());
        let result = runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);
            let driver = drive(
                client,
                1,
                "inst-a",
                "go".into(),
                relaxed_watchdog(),
                true, // --until-report
                |event: DriveEvent| -> Result<()> {
                    events.borrow_mut().push(match event {
                        DriveEvent::Report(r) => Ev::Report(r.status),
                        DriveEvent::Stalled(_) => Ev::Stalled,
                        DriveEvent::Stopped(id) => Ev::Stopped(id),
                        DriveEvent::ReArmed(id) => Ev::ReArmed(id),
                    });
                    Ok(())
                },
                || {},
            );
            let ctrl = async {
                let _ = read_chunk(&mut server).await;
                // Unreported turn-end: re-arms rather than breaking.
                server
                    .write_all(br#"{"type":"turncompleted","turn_id":1,"reported":false}"#)
                    .await
                    .unwrap();
                server.write_all(b"\n").await.unwrap();
                server.flush().await.unwrap();
                // A late terminal report finally lands → clean break.
                server
                    .write_all(br#"{"type":"report","status":"done","text":"finally"}"#)
                    .await
                    .unwrap();
                server.write_all(b"\n").await.unwrap();
                server.flush().await.unwrap();
                server
            };
            let (result, _server) = tokio::join!(driver, ctrl);
            result
        });
        result.expect("a re-armed turn that later reports is clean");
        assert_eq!(
            events.into_inner(),
            vec![Ev::ReArmed(1), Ev::Report(Status::Done)],
            "the unreported stop re-arms, then the late done breaks cleanly"
        );
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
                "inst-a",
                "go".into(),
                relaxed_watchdog(),
                false,
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

    /// A watchdog with the named deadlines in milliseconds and a short ready-gate
    /// (these tests feed no `SessionReady`, so the gate just times out into phase-1).
    fn wd_ms(heartbeat: u64, progress: u64, delivery: u64, retries: u32) -> Watchdog {
        Watchdog {
            ready_gate: Duration::from_millis(20),
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
                "inst-a",
                "go".into(),
                wd_ms(60, LONG_MS, LONG_MS, 3),
                false,
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
    fn it_escalates_a_progress_stall_at_most_twice_then_keeps_streaming() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let events: RefCell<Vec<Ev>> = RefCell::new(Vec::new());
        let notes: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let result = runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);
            let driver = drive(
                client,
                1,
                "inst-a",
                "go".into(),
                wd_ms(LONG_MS, 60, LONG_MS, 3),
                false,
                |event: DriveEvent| -> Result<()> {
                    if let DriveEvent::Stalled(text) = event {
                        notes.borrow_mut().push(text.to_string());
                    }
                    events.borrow_mut().push(match event {
                        DriveEvent::Report(r) => Ev::Report(r.status),
                        DriveEvent::Stalled(_) => Ev::Stalled,
                        DriveEvent::Stopped(id) => Ev::Stopped(id),
                        DriveEvent::ReArmed(id) => Ev::ReArmed(id),
                    });
                    Ok(())
                },
                || {},
            );
            let ctrl = async {
                let _ = read_chunk(&mut server).await;
                // Accept the turn (disable delivery), then go quiet well past
                // both notice deadlines (1x and 3x the window): the episode must
                // produce exactly two notices, not one per window.
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
            2,
            "an episode escalates once and then goes quiet: {events:?}"
        );

        // The wording is the point of the escalation: the first fire must not
        // assert the agent may be stuck (it fires on essentially every cold
        // launch and is benign), and only the second one warns.
        let notes = notes.into_inner();
        assert!(!notes[0].contains("may be stuck"), "{notes:?}");
        assert!(notes[0].contains("normal during long builds"), "{notes:?}");
        assert!(notes[1].contains("may be stuck"), "{notes:?}");
        assert!(notes[1].contains("progressStallSecs"), "{notes:?}");
    }

    #[test]
    fn the_unreported_stop_warning_names_both_recoveries() {
        // Reachable only without `--until-report`, which is now the interactive
        // default — so it addresses a watching operator. AC 619259/2: it must
        // say the turn may still conclude after this command exits, and how to
        // find that out.
        let msg = stopped_message(7);
        assert!(msg.contains("turn 7"), "{msg}");
        assert!(msg.contains("--until-report"), "{msg}");
        assert!(msg.contains("turn-state.json"), "{msg}");
        assert!(
            msg.contains("auto-nudging") && msg.contains("after this command"),
            "it must say a report can still land post-exit: {msg}"
        );
    }

    #[test]
    fn the_first_stall_notice_informs_and_only_the_second_warns() {
        // Pure over the message builder, so the wording contract is pinned
        // without waiting on real timers.
        let window = Duration::from_secs(300);
        let first = stall_message(window, 1);
        assert!(first.contains("no reports for 300s"), "{first}");
        assert!(!first.contains("may be stuck"), "{first}");

        let second = stall_message(window, 2);
        assert!(second.contains("may be stuck"), "{second}");
        // The second names the *elapsed* silence, not the window.
        assert!(second.contains("900s"), "{second}");
    }

    #[test]
    fn the_second_stall_notice_is_due_three_windows_in() {
        let start = Instant::now();
        let window = Duration::from_secs(300);
        assert_eq!(stall_notice_at(start, window, 0), start + window);
        assert_eq!(stall_notice_at(start, window, 1), start + window * 3);
    }

    #[test]
    fn it_resends_the_prompt_until_the_turn_is_accepted() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let (result, p1, p2) = runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);
            let driver = drive(
                client,
                7,
                "inst-a",
                "go".into(),
                wd_ms(LONG_MS, LONG_MS, 80, 3),
                false,
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
        // turn_id dedupe makes the resend safe).
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
                "inst-a",
                "go".into(),
                wd_ms(LONG_MS, LONG_MS, 50, 2),
                false,
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
        let msg = format!("{err:#}");
        // The verdict must distinguish a *wedged harness* from a broken
        // transport — the two look identical from here but only one is fixable
        // by re-prompting — and name the recovery, with the instance in it.
        assert!(msg.contains("never began"), "{msg}");
        assert!(
            msg.contains("wedged harness, not a broken transport"),
            "{msg}"
        );
        assert!(msg.contains("sandbox stop inst-a"), "{msg}");
        assert!(msg.contains("sandbox start --agent --name inst-a"), "{msg}");
        assert!(msg.contains("--redeliver"), "{msg}");
        // A fresh session cannot see the old conversation, so the caller has to
        // be told the directive must stand alone.
        assert!(msg.contains("stand on its own"), "{msg}");
    }

    // ---- phase-0 ready-gate (canned channel, tier 2) ----
    //
    // As with the watchdog-timer tests above, these use *small real* durations
    // (no paused clock — `test-util` is not enabled on the shared `tokio` dep).
    // The wall-clock the whole run took distinguishes "broke at once on
    // SessionReady" from "waited the gate out", deterministically: the gate is set
    // either seconds-wide (so an early break is unmistakable) or tens-of-ms (so the
    // timeout path is quick).

    /// Run `drive` over an in-memory duplex with a caller-supplied `watchdog`,
    /// making `guest_lines` available *before* the driver sends — so they feed the
    /// phase-0 ready-gate (a `SessionReady` that clears it, or a first line it
    /// stashes) and then phase-1. Returns `drive`'s result, the surfaced events, the
    /// prompt bytes the driver sent once the gate cleared, and the run's wall-clock
    /// (to assert an immediate break vs. a gate timeout). Every caller must include
    /// a terminal line (the relaxed timers never break on their own).
    fn drive_gate_lines(
        prompt: &str,
        turn_id: u64,
        watchdog: Watchdog,
        guest_lines: &[&str],
    ) -> (Result<()>, Vec<Ev>, String, std::time::Duration) {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let events: RefCell<Vec<Ev>> = RefCell::new(Vec::new());
        let sent = RefCell::new(String::new());

        let started = std::time::Instant::now();
        let result = runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);
            let driver = drive(
                client,
                turn_id,
                "inst-a",
                prompt.to_string(),
                watchdog,
                false,
                |event: DriveEvent| -> Result<()> {
                    events.borrow_mut().push(match event {
                        DriveEvent::Report(r) => Ev::Report(r.status),
                        DriveEvent::Stalled(_) => Ev::Stalled,
                        DriveEvent::Stopped(id) => Ev::Stopped(id),
                        DriveEvent::ReArmed(id) => Ev::ReArmed(id),
                    });
                    Ok(())
                },
                || {},
            );
            let feeder = async {
                // The lines are buffered *before* the prompt is sent: they reach the
                // ready-gate first, then phase-1.
                for line in guest_lines {
                    server.write_all(line.as_bytes()).await.unwrap();
                    server.write_all(b"\n").await.unwrap();
                }
                server.flush().await.unwrap();
                // Drain the prompt the driver emits once the gate clears, then hold
                // the stream open so the driver's terminal break is not raced by EOF.
                *sent.borrow_mut() = read_chunk(&mut server).await;
                server
            };
            let (result, _server) = tokio::join!(driver, feeder);
            result
        });
        let elapsed = started.elapsed();
        (result, events.into_inner(), sent.into_inner(), elapsed)
    }

    #[test]
    fn it_sends_immediately_when_session_ready_precedes_the_deadline() {
        // A seconds-wide gate, but `SessionReady` arrives first → the gate breaks at
        // once and the prompt is sent without waiting out `ready_gate`.
        let watchdog = Watchdog {
            ready_gate: Duration::from_secs(30),
            ..relaxed_watchdog()
        };
        let (result, events, sent, elapsed) = drive_gate_lines(
            "go",
            1,
            watchdog,
            &[
                r#"{"type":"sessionready"}"#,
                r#"{"type":"report","status":"done","text":"ok"}"#,
            ],
        );
        result.expect("a SessionReady-cleared gate is not an error");
        assert_eq!(events, vec![Ev::Report(Status::Done)]);
        // The prompt was actually sent (post-gate), carrying the right turn.
        let HostMessage::Prompt(p) =
            serde_json::from_str::<HostMessage>(sent.trim()).expect("a Prompt was sent");
        assert_eq!(p.turn_id, 1);
        assert!(
            elapsed < Duration::from_secs(5),
            "SessionReady must clear the gate immediately, not wait it out: {elapsed:?}"
        );
    }

    #[test]
    fn it_passes_the_gate_at_once_on_a_latched_session_ready_replay() {
        // The server latches `SessionReady` and replays it on every control connect,
        // so a prompt to an already-armed agent sees it as the very first
        // line — the gate clears with ~no wait even though it is seconds-wide.
        let watchdog = Watchdog {
            ready_gate: Duration::from_secs(30),
            ..relaxed_watchdog()
        };
        let (result, events, _sent, elapsed) = drive_gate_lines(
            "go",
            2,
            watchdog,
            &[
                r#"{"type":"sessionready"}"#,
                r#"{"type":"turncompleted","turn_id":2,"reported":true}"#,
            ],
        );
        result.expect("a latched-replay SessionReady clears the gate cleanly");
        assert!(
            events.is_empty(),
            "clean completion surfaces nothing: {events:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the latched replay must clear the gate at once: {elapsed:?}"
        );
    }

    #[test]
    fn it_feeds_a_stashed_first_line_into_phase_one() {
        // A fast agent's first `working` arrives *during* the gate, before any
        // SessionReady. The gate must stash it (agent already moving) and phase-1
        // must still process it — so it is not lost.
        let watchdog = Watchdog {
            ready_gate: Duration::from_secs(30),
            ..relaxed_watchdog()
        };
        let (result, events, sent, _elapsed) = drive_gate_lines(
            "go",
            3,
            watchdog,
            &[
                r#"{"type":"report","status":"working","text":"already building"}"#,
                r#"{"type":"report","status":"done","text":"ok"}"#,
            ],
        );
        result.expect("a stashed first line is not an error");
        assert_eq!(
            events,
            vec![Ev::Report(Status::Working), Ev::Report(Status::Done)],
            "the stashed `working` must reach the sink, then the `done` terminal"
        );
        // The prompt is still sent (the stashed line breaks the gate, then we send).
        let HostMessage::Prompt(p) =
            serde_json::from_str::<HostMessage>(sent.trim()).expect("a Prompt was sent");
        assert_eq!(p.turn_id, 3);
    }

    #[test]
    fn it_proceeds_after_the_ready_gate_without_session_ready() {
        // No `SessionReady` ever arrives: the gate must time out and send the prompt
        // anyway — 's ack-and-resend is then the guarantee. A short gate
        // keeps the test quick; the prompt only arrives once the gate elapses.
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let sent = RefCell::new(String::new());
        let started = std::time::Instant::now();
        let result = runtime.block_on(async {
            let (client, mut server) = tokio::io::duplex(4096);
            let watchdog = Watchdog {
                ready_gate: Duration::from_millis(80),
                ..relaxed_watchdog()
            };
            let driver = drive(
                client,
                1,
                "inst-a",
                "go".into(),
                watchdog,
                false,
                |_ev: DriveEvent| -> Result<()> { Ok(()) },
                || {},
            );
            let feeder = async {
                // Nothing is fed to the gate, so the prompt only lands once the gate
                // elapses; then a clean terminal so the loop ends.
                *sent.borrow_mut() = read_chunk(&mut server).await;
                server
                    .write_all(br#"{"type":"turncompleted","turn_id":1,"reported":true}"#)
                    .await
                    .unwrap();
                server.write_all(b"\n").await.unwrap();
                server.flush().await.unwrap();
                server
            };
            let (result, _server) = tokio::join!(driver, feeder);
            result
        });
        let elapsed = started.elapsed();
        result.expect("the gate proceeds without SessionReady (resend covers an unarmed agent)");
        let HostMessage::Prompt(p) =
            serde_json::from_str::<HostMessage>(sent.borrow().trim()).expect("a Prompt was sent");
        assert_eq!(p.turn_id, 1);
        assert!(
            elapsed >= Duration::from_millis(80),
            "the prompt must wait out the gate before proceeding: {elapsed:?}"
        );
    }

    // ---- the report journal (card 60b91e) ----

    /// A `FakeHost` whose clock seam answers with a fixed timestamp.
    fn host_with_clock(stamp: &str) -> FakeHost {
        let mut host = FakeHost::new();
        // One `date` per journaled event; queue plenty.
        for _ in 0..8 {
            host.push_run(Ok(std::process::Output {
                status: std::os::unix::process::ExitStatusExt::from_raw(0),
                stdout: format!("{stamp}\n").into_bytes(),
                stderr: Vec::new(),
            }));
        }
        host
    }

    fn journal_root(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("katsu-prompt-journal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn the_drive_sink_journals_reports_stops_and_rearms() {
        // The AC: a report, a `Stopped` verdict, and a `ReArmed` notice all land
        // in `reports.ndjson`, so "what did it conclude?" is answerable after
        // the streaming process and its terminal are gone.
        let root = journal_root("all-events");
        let host = host_with_clock("2026-08-01T12:00:00Z");

        let done = Report {
            status: Status::Done,
            text: "VERDICT: accept\n\nNo blocking findings.".to_string(),
            turn_id: Some(7),
        };
        journal_event(&host, &root, "inst-a", &DriveEvent::Report(&done));
        journal_event(&host, &root, "inst-a", &DriveEvent::Stopped(8));
        journal_event(&host, &root, "inst-a", &DriveEvent::ReArmed(9));

        let entries = report_log::read(&root, "inst-a");
        assert_eq!(entries.len(), 3, "{entries:?}");

        assert_eq!(entries[0].status, "done");
        assert_eq!(entries[0].turn_id, Some(7));
        assert!(
            entries[0].text.starts_with("VERDICT: accept"),
            "{entries:?}"
        );
        assert_eq!(entries[0].at, "2026-08-01T12:00:00Z");

        assert_eq!(entries[1].status, "stopped");
        assert_eq!(entries[1].turn_id, Some(8));
        assert_eq!(entries[2].status, "re-armed");
        assert_eq!(entries[2].turn_id, Some(9));

        // The terminal conclusion is what `sandbox status` surfaces; a later
        // `re-armed` notice must not displace it.
        let last = report_log::latest_terminal_at(&root, "inst-a").unwrap();
        assert_eq!(last.status, "stopped");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stall_notice_is_not_journaled_as_something_the_agent_said() {
        // `Stalled` is the watchdog's opinion, not an agent report — journaling
        // it would put words in the agent's mouth.
        let root = journal_root("stall");
        let host = host_with_clock("2026-08-01T12:00:00Z");
        journal_event(
            &host,
            &root,
            "inst-a",
            &DriveEvent::Stalled("no progress for 300s"),
        );
        assert!(report_log::read(&root, "inst-a").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_relayed_report_status_is_journaled_under_its_own_name() {
        let root = journal_root("statuses");
        let host = host_with_clock("2026-08-01T12:00:00Z");
        for (status, name) in [
            (Status::Working, "working"),
            (Status::Info, "info"),
            (Status::Done, "done"),
            (Status::Blocked, "blocked"),
        ] {
            let r = Report {
                status,
                text: format!("a {name} report"),
                turn_id: Some(1),
            };
            journal_event(&host, &root, "inst-a", &DriveEvent::Report(&r));
        }
        let got: Vec<String> = report_log::read(&root, "inst-a")
            .into_iter()
            .map(|e| e.status)
            .collect();
        assert_eq!(got, vec!["working", "info", "done", "blocked"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_journal_write_failure_never_breaks_the_drive() {
        // Best-effort semantics: point the journal at a path that cannot be
        // created (a file where the state dir should be) and confirm the call
        // still returns normally.
        let root = journal_root("unwritable");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("inst-a"), b"not a directory").unwrap();
        let host = host_with_clock("2026-08-01T12:00:00Z");
        let r = Report {
            status: Status::Done,
            text: "done".to_string(),
            turn_id: Some(1),
        };
        journal_event(&host, &root, "inst-a", &DriveEvent::Report(&r)); // must not panic
        assert!(report_log::read(&root, "inst-a").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
