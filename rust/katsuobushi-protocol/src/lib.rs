//! Wire types shared by every binary in `katsuobushi-sandbox-control`.
//!
//! The contract is the project's stable Layer-1 surface (see
//! `design/sandbox-agent-mode.md` §4). Keeping it in one lib crate means the
//! host client, the guest server, and the `report` command cannot drift out of
//! sync — a change to a field is a compile error everywhere at once.
//!
//! Every message is one line of newline-delimited JSON on every hop
//! (vsock host↔guest, and the guest-local unix socket `report` writes to).

use serde::{Deserialize, Serialize};

/// Host → guest: inject a new turn into the dormant interactive session.
///
/// `turn_id` is carried for clarity but correlation relies on ordering, not on
/// the model echoing it back (§4): a single serial session means reply-N
/// answers prompt-N in practice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub turn_id: u64,
    pub text: String,
}

/// The agent's coarse-grained status, reported via the `report` command.
///
/// One enum, not several narrow commands — a smaller surface to teach in the
/// agent contract (§4). `working`/`info` are progress; `done`/`blocked` are
/// terminal-for-this-turn signals the host waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Working,
    Done,
    Blocked,
    Info,
}

/// Guest → host: a status update relayed from the in-guest `report` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub status: Status,
    pub text: String,
    /// Optional; the agent is not required to echo the prompt's `turn_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
}

/// Everything the guest server can send back to the host over the held-open
/// vsock connection: relayed `report`s, plus a one-shot `ready` once the
/// server's listeners are up (§4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum GuestMessage {
    Ready,
    Report(Report),
}

/// Everything the host can send to the guest server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HostMessage {
    Prompt(Prompt),
}

/// The line `report` writes to the server's guest-local unix socket. The server
/// stamps it into a [`GuestMessage::Report`] and relays it to the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportLine {
    pub status: Status,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
}

/// The default vsock port the guest server listens on and the host connects to.
/// Arbitrary but fixed; the per-instance discriminator is the guest CID, not
/// the port.
pub const VSOCK_PORT: u32 = 1024;

/// `AF_VSOCK` well-known CID of the host (`VMADDR_CID_HOST`). The guest server
/// accepts a connection only from this CID (§5.9): an in-guest loopback peer
/// presents as `VMADDR_CID_LOCAL` (1), so the unprivileged agent cannot inject
/// prompts into its own session — only the host can.
pub const VMADDR_CID_HOST: u32 = 2;
