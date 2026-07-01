//! katsuobushi-sandbox-guest — the guest-side sandbox controller server.
//!
//! Claude Code spawns this over stdio as an MCP server. It declares exactly one
//! capability — `claude/channel` — so the host can push a prompt into the
//! dormant interactive session as a `<channel>` turn, and nothing else. It is
//! the adapter: the only place that knows the word "channel".
//!
//! Three I/O sources, one tokio runtime:
//!
//! - stdio ↔ claude: the rmcp server; its `Peer` is how we inject turns.
//! - control ↔ host: vsock in production (peer-CID==2 gated), a unix
//!   socket for the no-vsock local spike. Carries inbound `Prompt`s and
//!   outbound `Report`/`ready` lines.
//! - report ← agent: a guest-local unix socket the `report` command (and the
//!   `report hook` bridge) write one JSON line to; relayed to the host.
//!
//! Transport selection is deliberately a runtime detail so swapping vsock for
//! anything else never touches the stable host/agent contract:
//!
//! - `KATSU_CONTROL_UNIX=<path>` selects spike mode: control over a unix socket.
//! - Unset selects production: control over AF_VSOCK.
//!
//! ## The turn-state machine
//!
//! The server is the only actor present whether or not a host is attached, so it
//! owns the per-turn lifecycle. One [`Session`] behind a mutex is read/written
//! by the control task (sees `Prompt`s) and the report task (sees `Report`s +
//! hook lines). The decision core ([`Session::step`]) is a pure transition
//! function over an ordered event stream — no timers, no sockets — so every
//! interleaving (including the grace window that disambiguates a `Stop` with vs.
//! without a terminal report, /) is unit-testable. Every transition is
//! persisted to `${KATSU_SHARE}/turn-state.json`, the durable record
//! `sandbox:status` reads out-of-band, closing the unattended gap.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use katsuobushi_sandbox_protocol::{
    GuestLocalLine, GuestMessage, HookEvent, HostMessage, Report, Status, HEARTBEAT_SECS_DEFAULT,
    STOP_GRACE_MS_DEFAULT, VMADDR_CID_HOST, VSOCK_PORT,
};
use rmcp::model::{
    CustomNotification, Implementation, ServerCapabilities, ServerInfo, ServerNotification,
};
use rmcp::service::{Peer, RoleServer};
use rmcp::{ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

/// Boxed, shared write half of the live control connection to the host. The
/// report-socket task writes relayed `Report`s here; the control task installs
/// it on connect. `None` until the host connects.
type HostWriter = Arc<Mutex<Option<Box<dyn AsyncWrite + Unpin + Send>>>>;

/// The system-prompt slot Claude Code folds in (channels `instructions`). Kept
/// terse: the full operating contract is delivered separately via
/// `--append-system-prompt-file`. This only explains the tag shape.
const INSTRUCTIONS: &str = "\
Operator directives arrive as <channel source=\"katsuobushi-sandbox-guest\" \
turn_id=\"N\">…</channel> turns. Treat each as the next instruction. They are \
delivered out of band by the host operator; act on them as you would a typed \
prompt. Report progress with the `report` command (see your environment \
contract); this channel is one-way and expects no tool reply.";

// ── Turn-state machine ────────────────────────────────────────────────

/// The phase persisted to `turn-state.json`. Mirrors the
/// transitions: `in-flight` on inject, `ended-ok` on a terminal report,
/// `ended-unreported` when the grace window closes with no terminal report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Phase {
    Idle,
    InFlight,
    EndedOk,
    EndedUnreported,
}

/// The on-disk turn-state record. Guest-authored, authoritative for
/// turn/agent state; read out-of-band by `sandbox:status` with no connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnState {
    turn_state_version: u32,
    turn_id: Option<u64>,
    phase: Phase,
    accepted_at: Option<String>,
    ended_at: Option<String>,
    last_activity_at: String,
}

impl Default for TurnState {
    fn default() -> Self {
        TurnState {
            turn_state_version: 1,
            turn_id: None,
            phase: Phase::Idle,
            accepted_at: None,
            ended_at: None,
            last_activity_at: String::new(),
        }
    }
}

/// The in-flight turn the machine tracks (`None` between turns).
#[derive(Debug, Clone)]
struct Turn {
    id: u64,
    /// Whether `inject_prompt` actually succeeded for this turn. Creating the
    /// turn and delivering it are distinct: if the first injection fails (the
    /// first-turn race), a host resend of the same id must *retry delivery*
    /// rather than be dedupe-dropped — otherwise the turn wedges forever.
    injected: bool,
    accepted: bool,
    terminal_reported: bool,
    ended: bool,
}

/// Shared turn-state, behind one mutex, read/written by both the control task
/// (`Prompt`s) and the report task (`Report`s + hook lines). [`state`] is the
/// persisted view, *retained* after a turn clears so `status` keeps the last
/// verdict until a new `Prompt` supersedes it.
///
/// [`state`]: Session::state
#[derive(Debug, Default)]
struct Session {
    turn: Option<Turn>,
    ready_latched: bool,
    state: TurnState,
}

/// An ordered event the state machine consumes. The grace window is modeled as
/// an explicit [`GraceExpired`] event so the whole machine — including the
/// `ended-unreported` resolution — is a pure function testable without timers.
///
/// [`GraceExpired`]: Event::GraceExpired
#[derive(Debug, Clone)]
enum Event {
    /// Inbound `Prompt{turn_id}` on the control connection.
    Prompt { turn_id: u64 },
    /// `inject_prompt` succeeded for this turn (driven by the control task
    /// after the async injection completes, so the machine can tell a
    /// delivered turn from one whose injection failed).
    Injected { turn_id: u64 },
    /// A relayed `report <status> <text>` line.
    Report(Report),
    /// A `report hook <event>` lifecycle line.
    Hook(HookEvent),
    /// The grace-window timer fired for `turn_id`.
    GraceExpired { turn_id: u64 },
}

/// Whether the `turn-state.json` write is forced (a lifecycle transition) or
/// throttled (a bare `lastActivityAt` bump, capped at ≤1/s per).
#[derive(Debug, Clone, Copy)]
enum PersistMode {
    Force,
    Throttled,
}

/// The side effects [`Session::step`] asks the async layer to perform. Keeping
/// them data (not IO) is what makes the transition core pure.
#[derive(Debug, Default)]
struct Outcome {
    /// Only meaningful for `Prompt`: inject this turn into the live session.
    inject: bool,
    /// `GuestMessage`s to forward to the host (no-ops when none is attached).
    messages: Vec<GuestMessage>,
    /// Spawn the grace-window delayed check for this turn id.
    schedule_grace: Option<u64>,
    /// Persist the snapshot below, if set.
    persist: Option<PersistMode>,
    /// The `TurnState` to write (cloned when `persist` is set).
    snapshot: Option<TurnState>,
}

impl Session {
    /// The pure transition core. Given the current state, an event, and a
    /// timestamp (`now`, injected so the core stays clock-free), mutate the
    /// state and return the [`Outcome`] for the async layer to execute.
    fn step(&mut self, event: Event, now: &str) -> Outcome {
        let mut out = Outcome::default();
        match event {
            Event::Prompt { turn_id } => {
                // A resend of the current turn: retry delivery if the first
                // injection never succeeded (the wedge this distinction
                // exists for), otherwise drop it — including during the
                // grace window, where re-injecting an already-executed turn
                // would run it twice.
                if let Some(t) = self.turn.as_ref() {
                    if t.id == turn_id {
                        if !t.injected && !t.ended {
                            self.state.last_activity_at = now.to_string();
                            out.inject = true;
                            out.persist = Some(PersistMode::Throttled);
                            // Early return skips the common snapshot attach.
                            out.snapshot = Some(self.state.clone());
                        }
                        return out;
                    }
                }
                // A genuinely new (or superseding) turn.
                self.turn = Some(Turn {
                    id: turn_id,
                    injected: false,
                    accepted: false,
                    terminal_reported: false,
                    ended: false,
                });
                self.state = TurnState {
                    turn_state_version: 1,
                    turn_id: Some(turn_id),
                    phase: Phase::InFlight,
                    accepted_at: None,
                    ended_at: None,
                    last_activity_at: now.to_string(),
                };
                out.inject = true;
                out.persist = Some(PersistMode::Force);
            }
            Event::Injected { turn_id } => {
                // In-memory bookkeeping only: the turn's delivery is confirmed,
                // so future resends of this id dedupe instead of re-injecting.
                if let Some(t) = self.turn.as_mut() {
                    if t.id == turn_id {
                        t.injected = true;
                    }
                }
            }
            Event::Report(report) => {
                let terminal = matches!(report.status, Status::Done | Status::Blocked);
                self.state.last_activity_at = now.to_string();
                let mut force = false;
                let mut clear_turn = false;
                if let Some(t) = self.turn.as_mut() {
                    let id = t.id;
                    if terminal {
                        // `done`/`blocked` → terminal_reported; phase ended-ok.
                        t.terminal_reported = true;
                        t.accepted = true;
                        let was_ended = t.ended;
                        self.state.phase = Phase::EndedOk;
                        if self.state.ended_at.is_none() {
                            self.state.ended_at = Some(now.to_string());
                        }
                        if self.state.accepted_at.is_none() {
                            self.state.accepted_at = Some(now.to_string());
                        }
                        force = true;
                        // A late terminal report arriving during the grace window
                        // resolves the turn cleanly, so the delayed check
                        // becomes a no-op.
                        if was_ended {
                            out.messages.push(GuestMessage::TurnCompleted {
                                turn_id: id,
                                reported: true,
                            });
                            clear_turn = true;
                        }
                    } else if !t.accepted {
                        // First non-terminal activity: mark accepted + emit
                        // the delivery ack once.
                        t.accepted = true;
                        self.state.accepted_at = Some(now.to_string());
                        out.messages
                            .push(GuestMessage::TurnAccepted { turn_id: id });
                        force = true;
                    }
                }
                // Relay the report as today, regardless of turn state.
                out.messages.push(GuestMessage::Report(report));
                if clear_turn {
                    self.turn = None;
                }
                out.persist = Some(if force {
                    PersistMode::Force
                } else {
                    PersistMode::Throttled
                });
            }
            Event::Hook(HookEvent::SessionReady) => {
                // Session-lifetime latch; emitted now and replayed on each new
                // control connect. `send_to_host` no-ops with no host.
                self.ready_latched = true;
                self.state.last_activity_at = now.to_string();
                out.messages.push(GuestMessage::SessionReady);
                out.persist = Some(PersistMode::Throttled);
            }
            Event::Hook(HookEvent::TurnAccepted) => {
                self.state.last_activity_at = now.to_string();
                let mut force = false;
                if let Some(t) = self.turn.as_mut() {
                    if !t.accepted {
                        t.accepted = true;
                        let id = t.id;
                        self.state.accepted_at = Some(now.to_string());
                        out.messages
                            .push(GuestMessage::TurnAccepted { turn_id: id });
                        force = true;
                    }
                }
                out.persist = Some(if force {
                    PersistMode::Force
                } else {
                    PersistMode::Throttled
                });
            }
            Event::Hook(HookEvent::TurnEnded) => {
                self.state.last_activity_at = now.to_string();
                let mut clear_turn = false;
                if let Some(t) = self.turn.as_mut() {
                    let id = t.id;
                    if t.terminal_reported {
                        // Clean stop: corroborate the terminal report and clear.
                        self.state.phase = Phase::EndedOk;
                        if self.state.ended_at.is_none() {
                            self.state.ended_at = Some(now.to_string());
                        }
                        out.messages.push(GuestMessage::TurnCompleted {
                            turn_id: id,
                            reported: true,
                        });
                        clear_turn = true;
                    } else {
                        // Stop with no terminal report yet → arm the grace window
                        //; phase stays in-flight until it resolves.
                        t.ended = true;
                        self.state.ended_at = Some(now.to_string());
                        out.schedule_grace = Some(id);
                    }
                    out.persist = Some(PersistMode::Force);
                }
                if clear_turn {
                    self.turn = None;
                }
            }
            Event::GraceExpired { turn_id } => {
                // Re-check under the lock: only resolve if the *same* turn
                // is still ended and still unreported — otherwise a late terminal
                // report already cleared it, or a new turn superseded it.
                let resolve = self
                    .turn
                    .as_ref()
                    .is_some_and(|t| t.id == turn_id && t.ended && !t.terminal_reported);
                if resolve {
                    self.state.phase = Phase::EndedUnreported;
                    if self.state.ended_at.is_none() {
                        self.state.ended_at = Some(now.to_string());
                    }
                    out.messages.push(GuestMessage::TurnCompleted {
                        turn_id,
                        reported: false,
                    });
                    self.turn = None;
                    out.persist = Some(PersistMode::Force);
                }
            }
        }
        if out.persist.is_some() {
            out.snapshot = Some(self.state.clone());
        }
        out
    }
}

/// Everything the async tasks share to drive the machine: the live host writer,
/// the session state, the share path for `turn-state.json`, the grace window,
/// and the persist throttle (held across each write so writes are serialized).
struct Control {
    host: HostWriter,
    session: Mutex<Session>,
    share: PathBuf,
    stop_grace: Duration,
    last_persist: Mutex<Option<Instant>>,
}

/// Step the machine for one event under the lock, returning the [`Outcome`].
async fn drive_event(ctl: &Arc<Control>, event: Event) -> Outcome {
    let now = now_rfc3339();
    let mut session = ctl.session.lock().await;
    session.step(event, &now)
}

/// Execute the non-injection effects of an [`Outcome`]: forward messages, then
/// persist. Inject is handled by the caller (only `serve_control` holds a peer).
async fn send_and_persist(ctl: &Arc<Control>, outcome: &Outcome) {
    for msg in &outcome.messages {
        if let Err(e) = send_to_host(&ctl.host, msg).await {
            eprintln!("katsuobushi-control: send to host failed: {e:#}");
        }
    }
    if let (Some(mode), Some(snapshot)) = (outcome.persist, outcome.snapshot.as_ref()) {
        persist(ctl, snapshot, mode).await;
    }
}

/// Apply an [`Outcome`], spawning the grace-window delayed check if asked.
/// `GraceExpired` never schedules another grace, so the spawned task uses the
/// non-recursive [`send_and_persist`] directly.
async fn execute_outcome(ctl: &Arc<Control>, outcome: Outcome) {
    send_and_persist(ctl, &outcome).await;
    if let Some(turn_id) = outcome.schedule_grace {
        let ctl = ctl.clone();
        let grace = ctl.stop_grace;
        tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let out = drive_event(&ctl, Event::GraceExpired { turn_id }).await;
            send_and_persist(&ctl, &out).await;
        });
    }
}

/// Atomically write `turn-state.json` to the share (temp + rename; 9p2000.L
/// supports rename). `Throttled` writes are dropped if the last write was <1s
/// ago; the lock is held across the write so concurrent writers serialize.
async fn persist(ctl: &Arc<Control>, snapshot: &TurnState, mode: PersistMode) {
    let mut guard = ctl.last_persist.lock().await;
    if let PersistMode::Throttled = mode {
        if let Some(prev) = *guard {
            if prev.elapsed() < Duration::from_secs(1) {
                return;
            }
        }
    }
    if let Err(e) = write_turn_state(&ctl.share, snapshot) {
        eprintln!("katsuobushi-control: turn-state write failed: {e:#}");
        return;
    }
    *guard = Some(Instant::now());
}

/// The atomic write itself (temp + rename), factored out for testability.
fn write_turn_state(share: &Path, snapshot: &TurnState) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(snapshot).context("encode turn-state")?;
    bytes.push(b'\n');
    let tmp = share.join(".turn-state.json.tmp");
    let path = share.join("turn-state.json");
    std::fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// Current wall-clock time as a seconds-precision RFC3339 UTC timestamp. The
/// guest stamps `turn-state.json` with its own clock; no `chrono` dep, so
/// the civil-date conversion is done locally (and unit-tested).
fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_rfc3339(secs)
}

/// Format unix epoch seconds as `YYYY-MM-DDTHH:MM:SSZ` (UTC).
fn format_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Days-since-1970-01-01 → (year, month, day), Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Read a `u64` env knob, falling back to the protocol const when unset or
/// unparseable.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ── MCP / control / report plumbing ────────────────────────────────────────

/// Minimal one-way-channel MCP handler. No tools, no state — turns are injected
/// out of band through the captured [`Peer`], and the agent replies via the
/// `report` shell command, not an MCP tool.
#[derive(Clone)]
struct ControlServer;

impl ServerHandler for ControlServer {
    // ServerInfo/ServerCapabilities/Implementation are #[non_exhaustive], so we
    // build them from Default and set fields — which is exactly what
    // field_reassign_with_default flags; the lint has no better option here.
    #[allow(clippy::field_reassign_with_default)]
    fn get_info(&self) -> ServerInfo {
        // Declare *only* `claude/channel`: the smallest possible slice of
        // the research-preview API. `ExperimentalCapabilities` is a
        // `BTreeMap<String, JsonObject>`; presence of the key registers the
        // listener, the value is always `{}`.
        let mut experimental = BTreeMap::new();
        experimental.insert("claude/channel".to_string(), serde_json::Map::new());

        // Both ServerInfo and ServerCapabilities are #[non_exhaustive] (no struct
        // literals), but their fields are public — start from Default and set
        // only what we need.
        let mut capabilities = ServerCapabilities::default();
        capabilities.experimental = Some(experimental);

        // Name ourselves explicitly: Implementation::from_build_env() captures
        // rmcp's own build env (it reports "rmcp"), and Claude Code's <channel
        // source="…"> attribute — referenced by the agent contract — is least
        // ambiguous when serverInfo.name matches the registered server name.
        let mut server_info = Implementation::default();
        server_info.name = "katsuobushi-sandbox-guest".to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();

        let mut info = ServerInfo::default();
        info.capabilities = capabilities;
        info.server_info = server_info;
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }
}

/// Cap on a single inbound line, on both the control and report sockets.
/// `lines()` would otherwise buffer a newline-less flood without bound — and
/// the report socket is reachable by the unprivileged in-guest agent, so an
/// unbounded read is an in-guest OOM an untrusted turn could trigger. Real
/// lines are small (a `Prompt` or a one-line report); 1 MiB is generous.
const MAX_LINE_BYTES: u64 = 1024 * 1024;

/// Read one newline-terminated line of at most [`MAX_LINE_BYTES`], through a
/// reusable buffer. `Ok(None)` on EOF; an oversized line is an error so the
/// caller drops the connection instead of buffering forever.
async fn next_bounded_line<R>(reader: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Option<String>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    buf.clear();
    let n = (&mut *reader)
        .take(MAX_LINE_BYTES)
        .read_until(b'\n', buf)
        .await?;
    if n == 0 {
        return Ok(None); // EOF
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if n as u64 == MAX_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("line exceeds {MAX_LINE_BYTES} bytes; dropping the connection"),
        ));
    }
    // else: EOF with no trailing newline — treat the tail as the final line.
    Ok(Some(String::from_utf8_lossy(buf).into_owned()))
}

/// Push one operator directive into the live session as a `<channel>` turn.
async fn inject_prompt(peer: &Peer<RoleServer>, turn_id: u64, text: &str) -> anyhow::Result<()> {
    // `meta` keys must be `[A-Za-z0-9_]` (hyphens are silently dropped by Claude
    // Code) and values are strings — hence `turn_id` as a string attribute.
    let note = CustomNotification::new(
        "notifications/claude/channel",
        Some(serde_json::json!({
            "content": text,
            "meta": { "turn_id": turn_id.to_string() },
        })),
    );
    peer.send_notification(ServerNotification::CustomNotification(note))
        .await
        .context("send channel notification")
}

/// Write one `GuestMessage` as a JSON line to the host, if connected.
async fn send_to_host(host: &HostWriter, msg: &GuestMessage) -> anyhow::Result<()> {
    let mut guard = host.lock().await;
    if let Some(w) = guard.as_mut() {
        let mut line = serde_json::to_vec(msg).context("encode guest message")?;
        line.push(b'\n');
        w.write_all(&line).await.context("write to host")?;
        w.flush().await.ok();
    }
    Ok(())
}

/// The heartbeat task: emit `GuestMessage::Heartbeat{seq}` every `period`
/// with a monotonic `seq`. A no-op when no host is attached, so `seq` may gap —
/// the host judges by cadence, not continuity. **Silent per tick**: no
/// `println!`/`eprintln!`, so a backgrounded `drive` is never woken by a tick.
async fn run_heartbeat(host: HostWriter, period: Duration) {
    let mut ticker = tokio::time::interval(period);
    let mut seq: u64 = 0;
    loop {
        ticker.tick().await;
        seq += 1;
        let _ = send_to_host(&host, &GuestMessage::Heartbeat { seq }).await;
    }
}

/// Drive one accepted control connection: announce `ready`, replay a latched
/// `SessionReady`, install the write half for the report relay, then read
/// inbound `Prompt`s — applying the dedupe/new-turn machine before any
/// inject — until the host hangs up. Generic over the stream type so vsock and
/// unix share this exactly.
async fn serve_control<S>(stream: S, peer: Peer<RoleServer>, ctl: Arc<Control>)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    {
        let mut guard = ctl.host.lock().await;
        *guard = Some(Box::new(write_half));
    }
    if let Err(e) = send_to_host(&ctl.host, &GuestMessage::Ready).await {
        eprintln!("katsuobushi-control: failed to send ready: {e:#}");
    }
    // The latch is replayed on every new connect so a prompt to an already-armed
    // agent clears the host's ready-gate instantly.
    if ctl.session.lock().await.ready_latched {
        if let Err(e) = send_to_host(&ctl.host, &GuestMessage::SessionReady).await {
            eprintln!("katsuobushi-control: failed to replay session-ready: {e:#}");
        }
    }

    let mut reader = BufReader::new(read_half);
    let mut buf = Vec::new();
    loop {
        match next_bounded_line(&mut reader, &mut buf).await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => match serde_json::from_str::<HostMessage>(&line) {
                Ok(HostMessage::Prompt(p)) => {
                    let outcome = drive_event(&ctl, Event::Prompt { turn_id: p.turn_id }).await;
                    if outcome.inject {
                        match inject_prompt(&peer, p.turn_id, &p.text).await {
                            Ok(()) => {
                                // Confirm delivery so resends of this id dedupe.
                                let confirmed =
                                    drive_event(&ctl, Event::Injected { turn_id: p.turn_id })
                                        .await;
                                execute_outcome(&ctl, confirmed).await;
                            }
                            // Leave `injected` unset: the host's delivery-
                            // deadline resend will retry the injection.
                            Err(e) => eprintln!(
                                "katsuobushi-control: inject failed (host resend will retry): {e:#}"
                            ),
                        }
                    } else {
                        // dedupe: a resend for an already-delivered turn.
                        eprintln!(
                            "katsuobushi-control: dropping resend for in-flight turn {}",
                            p.turn_id
                        );
                    }
                    execute_outcome(&ctl, outcome).await;
                }
                Err(e) => eprintln!("katsuobushi-control: bad host line: {e}"),
            },
            Ok(None) => break, // host hung up
            Err(e) => {
                eprintln!("katsuobushi-control: control read error: {e}");
                break;
            }
        }
    }
    // Drop the stale writer so a reconnect installs a fresh one.
    let mut guard = ctl.host.lock().await;
    *guard = None;
}

/// Accept control connections from the host. vsock in production (only CID 2 —
/// `VMADDR_CID_HOST` — is honoured), a unix socket in the local spike.
async fn run_control(peer: Peer<RoleServer>, ctl: Arc<Control>) -> anyhow::Result<()> {
    if let Ok(path) = std::env::var("KATSU_CONTROL_UNIX") {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).with_context(|| format!("bind {path}"))?;
        eprintln!("katsuobushi-control: control on unix:{path} (spike mode)");
        loop {
            let (stream, _) = listener.accept().await.context("accept unix")?;
            serve_control(stream, peer.clone(), ctl.clone()).await;
        }
    } else {
        use tokio_vsock::{VsockAddr, VsockListener};
        let addr = VsockAddr::new(tokio_vsock::VMADDR_CID_ANY, VSOCK_PORT);
        let listener = VsockListener::bind(addr).context("bind vsock")?;
        eprintln!("katsuobushi-control: control on vsock:*:{VSOCK_PORT}");
        loop {
            let (stream, peer_addr) = listener.accept().await.context("accept vsock")?;
            // Only the host (CID 2) may inject prompts. An in-guest
            // loopback peer is CID 1, so the unprivileged agent cannot poke its
            // own session.
            if peer_addr.cid() != VMADDR_CID_HOST {
                eprintln!(
                    "katsuobushi-control: rejecting control peer cid {}",
                    peer_addr.cid()
                );
                continue;
            }
            serve_control(stream, peer.clone(), ctl.clone()).await;
        }
    }
}

/// Listen on the guest-local report socket. The `report` command (and the
/// `report hook` bridge) connects, writes one [`GuestLocalLine`] JSON line, and
/// closes; we drive the machine with it.
async fn run_report(ctl: Arc<Control>) -> anyhow::Result<()> {
    let path = std::env::var("KATSU_REPORT_SOCK")
        .unwrap_or_else(|_| "/run/katsuobushi/report.sock".to_string());
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("bind report {path}"))?;
    eprintln!("katsuobushi-control: report on unix:{path}");
    loop {
        let (stream, _) = listener.accept().await.context("accept report")?;
        let ctl = ctl.clone();
        tokio::spawn(async move {
            if let Err(e) = relay_report(stream, ctl).await {
                eprintln!("katsuobushi-control: report relay error: {e:#}");
            }
        });
    }
}

/// Parse each guest-local line as an untagged [`GuestLocalLine`] and drive the
/// machine: a `Report` relays + marks accepted/terminal; a `Hook` latches
/// readiness, acks the turn, or resolves the grace window.
async fn relay_report(stream: UnixStream, ctl: Arc<Control>) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    while let Some(line) = next_bounded_line(&mut reader, &mut buf).await? {
        if line.trim().is_empty() {
            continue;
        }
        let event = match serde_json::from_str::<GuestLocalLine>(&line) {
            Ok(GuestLocalLine::Report(rl)) => Event::Report(Report {
                status: rl.status,
                text: rl.text,
                turn_id: rl.turn_id,
            }),
            Ok(GuestLocalLine::Hook(h)) => Event::Hook(h.event),
            Err(e) => {
                eprintln!("katsuobushi-control: bad report line: {e}");
                continue;
            }
        };
        let outcome = drive_event(&ctl, event).await;
        execute_outcome(&ctl, outcome).await;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Serve the MCP side over stdio; Claude Code is the client. The running
    // service hands us a `Peer` we use to inject channel turns out of band.
    let service = ControlServer
        .serve(rmcp::transport::stdio())
        .await
        .context("start rmcp stdio server")?;
    let peer = service.peer().clone();

    let host: HostWriter = Arc::new(Mutex::new(None));
    let heartbeat_secs = env_u64("KATSU_HEARTBEAT_SECS", HEARTBEAT_SECS_DEFAULT).max(1);
    let stop_grace = Duration::from_millis(env_u64("KATSU_STOP_GRACE_MS", STOP_GRACE_MS_DEFAULT));
    let share = PathBuf::from(
        std::env::var("KATSU_SHARE").unwrap_or_else(|_| "/mnt/katsuobushi".to_string()),
    );

    let ctl = Arc::new(Control {
        host: host.clone(),
        session: Mutex::new(Session::default()),
        share,
        stop_grace,
        last_persist: Mutex::new(None),
    });

    // Seed the durable record with an `idle` baseline so `status` has something
    // to read before the first turn.
    {
        let snapshot = {
            let mut session = ctl.session.lock().await;
            session.state.last_activity_at = now_rfc3339();
            session.state.clone()
        };
        persist(&ctl, &snapshot, PersistMode::Force).await;
    }

    // The heartbeat task: silent transport-liveness ticks.
    let heartbeat = tokio::spawn(run_heartbeat(
        ctl.host.clone(),
        Duration::from_secs(heartbeat_secs),
    ));

    // The control/report listeners are auxiliary: if they fail to bind (e.g. an
    // interactive guest launched with no vsock device, or a missing socket dir)
    // we log and keep serving the MCP connection rather than killing the server
    // — Claude Code would otherwise see its channel server crash.
    let control_ctl = ctl.clone();
    let control = tokio::spawn(async move {
        if let Err(e) = run_control(peer, control_ctl).await {
            eprintln!("katsuobushi-control: control listener stopped: {e:#}");
        }
    });
    let report_ctl = ctl.clone();
    let report = tokio::spawn(async move {
        if let Err(e) = run_report(report_ctl).await {
            eprintln!("katsuobushi-control: report listener stopped: {e:#}");
        }
    });

    // The MCP stdio connection to Claude is the server's reason to live; exit
    // only when Claude Code disconnects.
    service.waiting().await.context("rmcp service")?;
    control.abort();
    report.abort();
    heartbeat.abort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Tier-1: the pure state machine over an ordered event stream ─────────

    /// A fixed timestamp so transitions are deterministic; the clock is injected.
    const T: &str = "2026-06-28T00:00:00Z";

    fn step(session: &mut Session, event: Event) -> Outcome {
        session.step(event, T)
    }

    fn working() -> Event {
        Event::Report(Report {
            status: Status::Working,
            text: "building".into(),
            turn_id: None,
        })
    }

    fn done() -> Event {
        Event::Report(Report {
            status: Status::Done,
            text: "shipped".into(),
            turn_id: None,
        })
    }

    // ── The bounded line reader (both socket loops) ─────────────────────────

    #[tokio::test]
    async fn it_reads_lines_and_signals_eof_within_the_bound() {
        let (client, server) = tokio::io::duplex(4096);
        let mut writer = client;
        writer.write_all(b"one\ntwo\n").await.unwrap();
        drop(writer);

        let mut reader = BufReader::new(server);
        let mut buf = Vec::new();
        assert_eq!(
            next_bounded_line(&mut reader, &mut buf).await.unwrap(),
            Some("one".to_string())
        );
        assert_eq!(
            next_bounded_line(&mut reader, &mut buf).await.unwrap(),
            Some("two".to_string())
        );
        assert_eq!(next_bounded_line(&mut reader, &mut buf).await.unwrap(), None);
    }

    #[tokio::test]
    async fn it_errors_on_a_line_exceeding_the_bound() {
        // A newline-less flood (the report socket is agent-reachable, so this
        // is the in-guest OOM vector) must error at the cap, not buffer on.
        let (client, server) = tokio::io::duplex((MAX_LINE_BYTES as usize) + 1024);
        let mut writer = client;
        writer
            .write_all(&vec![b'A'; (MAX_LINE_BYTES as usize) + 8])
            .await
            .unwrap();
        drop(writer);

        let mut reader = BufReader::new(server);
        let mut buf = Vec::new();
        let err = next_bounded_line(&mut reader, &mut buf)
            .await
            .expect_err("an oversized line must error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn it_treats_an_unterminated_tail_as_the_final_line() {
        let (client, server) = tokio::io::duplex(4096);
        let mut writer = client;
        writer.write_all(b"tail-no-newline").await.unwrap();
        drop(writer);

        let mut reader = BufReader::new(server);
        let mut buf = Vec::new();
        assert_eq!(
            next_bounded_line(&mut reader, &mut buf).await.unwrap(),
            Some("tail-no-newline".to_string())
        );
    }

    #[test]
    fn it_injects_and_marks_in_flight_on_a_new_prompt() {
        let mut s = Session::default();
        let out = step(&mut s, Event::Prompt { turn_id: 1 });
        assert!(out.inject);
        assert_eq!(s.state.phase, Phase::InFlight);
        assert_eq!(s.state.turn_id, Some(1));
        assert!(matches!(out.persist, Some(PersistMode::Force)));
        assert!(s.turn.is_some());
    }

    #[test]
    fn it_dedupes_a_resend_of_a_delivered_in_flight_turn() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 1 });
        step(&mut s, Event::Injected { turn_id: 1 });
        let out = step(&mut s, Event::Prompt { turn_id: 1 });
        // Dropped: no re-inject, no transition, no persist.
        assert!(!out.inject);
        assert!(out.persist.is_none());
        assert!(out.messages.is_empty());
    }

    #[test]
    fn it_reinjects_a_resend_when_the_first_injection_never_succeeded() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 1 });
        // No `Injected` confirmation: the first inject failed (first-turn
        // race). The host's delivery-deadline resend must retry delivery
        // rather than wedge the turn behind the dedupe.
        let out = step(&mut s, Event::Prompt { turn_id: 1 });
        assert!(out.inject, "an undelivered turn's resend re-injects");
        assert!(out.messages.is_empty());
        // Same logical turn: state is not reset.
        assert_eq!(s.state.turn_id, Some(1));
        assert_eq!(s.state.phase, Phase::InFlight);
        assert!(!s.turn.as_ref().unwrap().accepted);
    }

    #[test]
    fn it_dedupes_a_resend_during_the_grace_window() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 1 });
        step(&mut s, Event::Injected { turn_id: 1 });
        step(&mut s, Event::Hook(HookEvent::TurnEnded)); // arms the grace window
        // A resend of the SAME id while the turn is ended-but-unresolved must
        // not create a fresh turn — re-injecting an already-executed turn
        // would run it twice.
        let out = step(&mut s, Event::Prompt { turn_id: 1 });
        assert!(!out.inject);
        assert!(
            s.turn.as_ref().unwrap().ended,
            "the ended turn is left in place for the grace resolution"
        );
    }

    #[test]
    fn it_supersedes_an_in_flight_turn_with_a_new_id() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 1 });
        let out = step(&mut s, Event::Prompt { turn_id: 2 });
        assert!(out.inject);
        assert_eq!(s.turn.as_ref().unwrap().id, 2);
        assert_eq!(s.state.turn_id, Some(2));
    }

    #[test]
    fn it_acks_on_first_working_report_and_relays_it() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 1 });
        let out = step(&mut s, working());
        assert!(matches!(
            out.messages.first(),
            Some(GuestMessage::TurnAccepted { turn_id: 1 })
        ));
        assert!(matches!(out.messages.get(1), Some(GuestMessage::Report(_))));
        assert!(s.turn.as_ref().unwrap().accepted);
        assert_eq!(s.state.accepted_at.as_deref(), Some(T));
    }

    #[test]
    fn it_acks_only_once_across_repeated_working_reports() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 1 });
        step(&mut s, working());
        let out = step(&mut s, working());
        // Second working report: relayed, but no second ack.
        assert_eq!(out.messages.len(), 1);
        assert!(matches!(
            out.messages.first(),
            Some(GuestMessage::Report(_))
        ));
    }

    #[test]
    fn it_marks_ended_ok_on_a_terminal_report_without_a_second_ack() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 1 });
        let out = step(&mut s, done());
        assert_eq!(s.state.phase, Phase::EndedOk);
        assert!(s.turn.as_ref().unwrap().terminal_reported);
        // A terminal report relays the Report but does not emit TurnAccepted.
        assert!(out
            .messages
            .iter()
            .all(|m| matches!(m, GuestMessage::Report(_))));
    }

    #[test]
    fn it_acks_on_the_turn_accepted_hook() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 1 });
        let out = step(&mut s, Event::Hook(HookEvent::TurnAccepted));
        assert!(matches!(
            out.messages.first(),
            Some(GuestMessage::TurnAccepted { turn_id: 1 })
        ));
        // Idempotent.
        let again = step(&mut s, Event::Hook(HookEvent::TurnAccepted));
        assert!(again.messages.is_empty());
    }

    #[test]
    fn it_latches_session_ready_and_emits_it() {
        let mut s = Session::default();
        let out = step(&mut s, Event::Hook(HookEvent::SessionReady));
        assert!(s.ready_latched);
        assert!(matches!(
            out.messages.first(),
            Some(GuestMessage::SessionReady)
        ));
    }

    #[test]
    fn it_completes_cleanly_on_stop_after_a_terminal_report() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 5 });
        step(&mut s, working());
        step(&mut s, done());
        let out = step(&mut s, Event::Hook(HookEvent::TurnEnded));
        assert!(matches!(
            out.messages.first(),
            Some(GuestMessage::TurnCompleted {
                turn_id: 5,
                reported: true
            })
        ));
        // No grace window needed; turn cleared, phase ended-ok retained.
        assert!(out.schedule_grace.is_none());
        assert!(s.turn.is_none());
        assert_eq!(s.state.phase, Phase::EndedOk);
    }

    #[test]
    fn it_arms_the_grace_window_on_stop_without_a_terminal_report() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 9 });
        let out = step(&mut s, Event::Hook(HookEvent::TurnEnded));
        // Stop with no terminal report: grace armed, no verdict yet.
        assert_eq!(out.schedule_grace, Some(9));
        assert!(s.turn.as_ref().unwrap().ended);
        // Phase stays in-flight until the grace window resolves.
        assert_eq!(s.state.phase, Phase::InFlight);
        assert!(out
            .messages
            .iter()
            .all(|m| !matches!(m, GuestMessage::TurnCompleted { .. })));
    }

    #[test]
    fn it_resolves_ended_unreported_when_the_grace_window_closes() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 9 });
        step(&mut s, Event::Hook(HookEvent::TurnEnded));
        let out = step(&mut s, Event::GraceExpired { turn_id: 9 });
        assert!(matches!(
            out.messages.first(),
            Some(GuestMessage::TurnCompleted {
                turn_id: 9,
                reported: false
            })
        ));
        assert_eq!(s.state.phase, Phase::EndedUnreported);
        assert!(s.turn.is_none());
    }

    #[test]
    fn it_treats_a_late_terminal_report_during_grace_as_clean() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 9 });
        step(&mut s, Event::Hook(HookEvent::TurnEnded)); // grace armed
        let out = step(&mut s, done()); // late terminal report
        assert!(out.messages.iter().any(|m| matches!(
            m,
            GuestMessage::TurnCompleted {
                turn_id: 9,
                reported: true
            }
        )));
        assert_eq!(s.state.phase, Phase::EndedOk);
        assert!(s.turn.is_none());
        // The now-stale grace check is a no-op.
        let grace = step(&mut s, Event::GraceExpired { turn_id: 9 });
        assert!(grace.messages.is_empty());
        assert!(grace.persist.is_none());
        assert_eq!(s.state.phase, Phase::EndedOk);
    }

    #[test]
    fn it_ignores_a_grace_check_for_a_stale_turn_id() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 9 });
        step(&mut s, Event::Hook(HookEvent::TurnEnded));
        // A grace check for a different (superseded) id must not resolve turn 9.
        let out = step(&mut s, Event::GraceExpired { turn_id: 8 });
        assert!(out.messages.is_empty());
        assert!(out.persist.is_none());
        assert_eq!(s.state.phase, Phase::InFlight);
        assert!(s.turn.is_some());
    }

    #[test]
    fn it_walks_a_full_clean_turn_interleaving() {
        let mut s = Session::default();
        assert!(step(&mut s, Event::Prompt { turn_id: 3 }).inject);
        assert_eq!(s.state.phase, Phase::InFlight);
        step(&mut s, working());
        assert_eq!(s.state.accepted_at.as_deref(), Some(T));
        step(&mut s, done());
        assert_eq!(s.state.phase, Phase::EndedOk);
        let out = step(&mut s, Event::Hook(HookEvent::TurnEnded));
        assert!(matches!(
            out.messages.first(),
            Some(GuestMessage::TurnCompleted { reported: true, .. })
        ));
        assert_eq!(s.state.phase, Phase::EndedOk);
        assert!(s.turn.is_none());
    }

    #[test]
    fn it_throttle_flags_bare_activity_and_forces_lifecycle_edges() {
        let mut s = Session::default();
        step(&mut s, Event::Prompt { turn_id: 1 });
        // First working: an ack edge → forced.
        assert!(matches!(
            step(&mut s, working()).persist,
            Some(PersistMode::Force)
        ));
        // Subsequent working: bare activity → throttled.
        assert!(matches!(
            step(&mut s, working()).persist,
            Some(PersistMode::Throttled)
        ));
    }

    // ── RFC3339 timestamp formatting ────────────────────────────────────────

    #[test]
    fn it_formats_epoch_seconds_as_rfc3339_utc() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_609_459_200), "2021-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_751_068_800), "2025-06-28T00:00:00Z");
        assert_eq!(format_rfc3339(1_614_211_323), "2021-02-25T00:02:03Z");
    }

    // ── Async: the durable record + heartbeat ───────────────────────────────

    fn unique_tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("katsu-turn-state-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_control(share: PathBuf, stop_grace: Duration) -> Arc<Control> {
        Arc::new(Control {
            host: Arc::new(Mutex::new(None)),
            session: Mutex::new(Session::default()),
            share,
            stop_grace,
            last_persist: Mutex::new(None),
        })
    }

    fn read_state(dir: &Path) -> TurnState {
        let raw = std::fs::read_to_string(dir.join("turn-state.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[tokio::test]
    async fn it_persists_ended_unreported_after_grace_with_no_host_attached() {
        let dir = unique_tmp();
        let ctl = test_control(dir.clone(), Duration::from_millis(30));

        let out = drive_event(&ctl, Event::Prompt { turn_id: 7 }).await;
        execute_outcome(&ctl, out).await;
        assert_eq!(read_state(&dir).phase, Phase::InFlight);

        // Stop with no terminal report and no host writer installed (case).
        let out = drive_event(&ctl, Event::Hook(HookEvent::TurnEnded)).await;
        execute_outcome(&ctl, out).await;

        // The grace-window task is server-side; it runs and persists even though
        // nothing is delivered live.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let state = read_state(&dir);
        assert_eq!(state.phase, Phase::EndedUnreported);
        assert_eq!(state.turn_id, Some(7));
        assert!(state.ended_at.is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn it_persists_ended_ok_on_a_terminal_report() {
        let dir = unique_tmp();
        let ctl = test_control(dir.clone(), Duration::from_millis(30));

        for event in [
            Event::Prompt { turn_id: 4 },
            working(),
            done(),
            Event::Hook(HookEvent::TurnEnded),
        ] {
            let out = drive_event(&ctl, event).await;
            execute_outcome(&ctl, out).await;
        }
        let state = read_state(&dir);
        assert_eq!(state.phase, Phase::EndedOk);
        assert_eq!(state.turn_id, Some(4));
        assert!(state.accepted_at.is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn it_emits_heartbeats_with_monotonic_seq_on_cadence() {
        let host: HostWriter = Arc::new(Mutex::new(None));
        let (reader, writer) = tokio::io::duplex(4096);
        *host.lock().await = Some(Box::new(writer));

        let task = tokio::spawn(run_heartbeat(host.clone(), Duration::from_millis(5)));

        let mut lines = BufReader::new(reader).lines();
        let mut seqs = Vec::new();
        for _ in 0..3 {
            let line = tokio::time::timeout(Duration::from_secs(2), lines.next_line())
                .await
                .expect("heartbeat within deadline")
                .unwrap()
                .unwrap();
            match serde_json::from_str::<GuestMessage>(&line).unwrap() {
                GuestMessage::Heartbeat { seq } => seqs.push(seq),
                other => panic!("expected heartbeat, got {other:?}"),
            }
        }
        task.abort();
        // Monotonic from 1, on cadence — and nothing but heartbeats on the wire.
        assert_eq!(seqs, vec![1, 2, 3]);
    }
}
