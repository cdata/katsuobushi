//! katsuobushi-sandbox-control — the guest-side sandbox controller server.
//!
//! Claude Code spawns this over stdio as an MCP server (see
//! `design/sandbox-agent-mode.md` §3). It declares exactly one capability —
//! `claude/channel` — so the host can push a prompt into the dormant
//! interactive session as a `<channel>` turn, and nothing else. It is the
//! adapter (design Layer 2): the only place that knows the word "channel".
//!
//! Three I/O sources, one tokio runtime:
//!
//! - stdio ↔ claude: the rmcp server; its `Peer` is how we inject turns.
//! - control ↔ host: vsock in production (peer-CID==2 gated, §5.9), a unix
//!   socket for the no-vsock local spike. Carries inbound `Prompt`s and
//!   outbound `Report`/`ready` lines.
//! - report ← agent: a guest-local unix socket the `report` command writes one
//!   JSON line to; relayed to the host on the control connection.
//!
//! Transport selection is deliberately a runtime detail so swapping vsock for
//! anything else never touches the stable host/agent contract:
//!
//! - `KATSU_CONTROL_UNIX=<path>` selects spike mode: control over a unix socket.
//! - Unset selects production: control over AF_VSOCK.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context as _;
use katsuobushi_protocol::{
    GuestMessage, HostMessage, Report, ReportLine, VMADDR_CID_HOST, VSOCK_PORT,
};
use rmcp::model::{
    CustomNotification, Implementation, ServerCapabilities, ServerInfo, ServerNotification,
};
use rmcp::service::{Peer, RoleServer};
use rmcp::{ServerHandler, ServiceExt};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

/// Boxed, shared write half of the live control connection to the host. The
/// report-socket task writes relayed `Report`s here; the control task installs
/// it on connect. `None` until the host connects.
type HostWriter = Arc<Mutex<Option<Box<dyn AsyncWrite + Unpin + Send>>>>;

/// The system-prompt slot Claude Code folds in (channels `instructions`). Kept
/// terse: the full operating contract is delivered separately via
/// `--append-system-prompt-file` (§5.11). This only explains the tag shape.
const INSTRUCTIONS: &str = "\
Operator directives arrive as <channel source=\"katsuobushi-sandbox-control\" \
turn_id=\"N\">…</channel> turns. Treat each as the next instruction. They are \
delivered out of band by the host operator; act on them as you would a typed \
prompt. Report progress with the `report` command (see your environment \
contract); this channel is one-way and expects no tool reply.";

/// Minimal one-way-channel MCP handler. No tools, no state — turns are injected
/// out of band through the captured [`Peer`], and the agent replies via the
/// `report` shell command, not an MCP tool (§5.6).
#[derive(Clone)]
struct ControlServer;

impl ServerHandler for ControlServer {
    // ServerInfo/ServerCapabilities/Implementation are #[non_exhaustive], so we
    // build them from Default and set fields — which is exactly what
    // field_reassign_with_default flags; the lint has no better option here.
    #[allow(clippy::field_reassign_with_default)]
    fn get_info(&self) -> ServerInfo {
        // Declare *only* `claude/channel` (§5.5): the smallest possible slice of
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
        server_info.name = "katsuobushi-sandbox-control".to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();

        let mut info = ServerInfo::default();
        info.capabilities = capabilities;
        info.server_info = server_info;
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }
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

/// Drive one accepted control connection: announce `ready`, install the write
/// half for the report relay, then read inbound `Prompt`s until the host hangs
/// up. Generic over the stream type so vsock and unix share this exactly.
async fn serve_control<S>(stream: S, peer: Peer<RoleServer>, host: HostWriter)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    {
        let mut guard = host.lock().await;
        *guard = Some(Box::new(write_half));
    }
    if let Err(e) = send_to_host(&host, &GuestMessage::Ready).await {
        eprintln!("katsuobushi-control: failed to send ready: {e:#}");
    }

    let mut lines = BufReader::new(read_half).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) if line.trim().is_empty() => continue,
            Ok(Some(line)) => match serde_json::from_str::<HostMessage>(&line) {
                Ok(HostMessage::Prompt(p)) => {
                    if let Err(e) = inject_prompt(&peer, p.turn_id, &p.text).await {
                        eprintln!("katsuobushi-control: inject failed: {e:#}");
                    }
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
    let mut guard = host.lock().await;
    *guard = None;
}

/// Accept control connections from the host. vsock in production (only CID 2 —
/// `VMADDR_CID_HOST` — is honoured), a unix socket in the local spike.
async fn run_control(peer: Peer<RoleServer>, host: HostWriter) -> anyhow::Result<()> {
    if let Ok(path) = std::env::var("KATSU_CONTROL_UNIX") {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).with_context(|| format!("bind {path}"))?;
        eprintln!("katsuobushi-control: control on unix:{path} (spike mode)");
        loop {
            let (stream, _) = listener.accept().await.context("accept unix")?;
            serve_control(stream, peer.clone(), host.clone()).await;
        }
    } else {
        use tokio_vsock::{VsockAddr, VsockListener};
        let addr = VsockAddr::new(tokio_vsock::VMADDR_CID_ANY, VSOCK_PORT);
        let listener = VsockListener::bind(addr).context("bind vsock")?;
        eprintln!("katsuobushi-control: control on vsock:*:{VSOCK_PORT}");
        loop {
            let (stream, peer_addr) = listener.accept().await.context("accept vsock")?;
            // §5.9: only the host (CID 2) may inject prompts. An in-guest
            // loopback peer is CID 1, so the unprivileged agent cannot poke its
            // own session.
            if peer_addr.cid() != VMADDR_CID_HOST {
                eprintln!(
                    "katsuobushi-control: rejecting control peer cid {}",
                    peer_addr.cid()
                );
                continue;
            }
            serve_control(stream, peer.clone(), host.clone()).await;
        }
    }
}

/// Listen on the guest-local report socket. The `report` command connects,
/// writes one [`ReportLine`] JSON line, and closes; we relay it to the host.
async fn run_report(host: HostWriter) -> anyhow::Result<()> {
    let path = std::env::var("KATSU_REPORT_SOCK")
        .unwrap_or_else(|_| "/run/katsuobushi/report.sock".to_string());
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("bind report {path}"))?;
    eprintln!("katsuobushi-control: report on unix:{path}");
    loop {
        let (stream, _) = listener.accept().await.context("accept report")?;
        let host = host.clone();
        tokio::spawn(async move {
            if let Err(e) = relay_report(stream, host).await {
                eprintln!("katsuobushi-control: report relay error: {e:#}");
            }
        });
    }
}

async fn relay_report(stream: UnixStream, host: HostWriter) -> anyhow::Result<()> {
    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let rl: ReportLine = serde_json::from_str(&line).context("decode report line")?;
        let report = Report {
            status: rl.status,
            text: rl.text,
            turn_id: rl.turn_id,
        };
        send_to_host(&host, &GuestMessage::Report(report)).await?;
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

    // The control/report listeners are auxiliary: if they fail to bind (e.g. an
    // interactive guest launched with no vsock device, or a missing socket dir)
    // we log and keep serving the MCP connection rather than killing the server
    // — Claude Code would otherwise see its channel server crash.
    let control_host = host.clone();
    let control = tokio::spawn(async move {
        if let Err(e) = run_control(peer, control_host).await {
            eprintln!("katsuobushi-control: control listener stopped: {e:#}");
        }
    });
    let report = tokio::spawn(async move {
        if let Err(e) = run_report(host).await {
            eprintln!("katsuobushi-control: report listener stopped: {e:#}");
        }
    });

    // The MCP stdio connection to Claude is the server's reason to live; exit
    // only when Claude Code disconnects.
    service.waiting().await.context("rmcp service")?;
    control.abort();
    report.abort();
    Ok(())
}
