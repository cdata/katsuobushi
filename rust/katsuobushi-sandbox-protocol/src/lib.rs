//! Wire types shared by every binary in `katsuobushi-sandbox-guest`.
//!
//! This is the project's stable wire-protocol surface. Keeping it in one lib
//! crate means the host client, the guest server, and the `report` command
//! cannot drift out of sync — a change to a field is a compile error everywhere
//! at once.
//!
//! Every message is one line of newline-delimited JSON on every hop
//! (vsock host↔guest, and the guest-local unix socket `report` writes to).

use serde::{Deserialize, Serialize};

/// Host → guest: inject a new turn into the dormant interactive session.
///
/// `turn_id` is carried for clarity but correlation relies on ordering, not on
/// the model echoing it back: a single serial session means reply-N
/// answers prompt-N in practice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prompt {
    pub turn_id: u64,
    pub text: String,
}

/// The agent's coarse-grained status, reported via the `report` command.
///
/// One enum, not several narrow commands — a smaller surface to teach in the
/// agent contract. `working`/`info` are progress; `done`/`blocked` are
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
/// server's listeners are up.
///
/// The variants stay flat and tagged (`{"type":…}`); decoders ignore unknown
/// variants, so adding more here never breaks an older peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum GuestMessage {
    /// Transport accepted: the server's listeners are up (retained).
    Ready,
    /// The agent is armed and idle — the first-turn race gate.
    SessionReady,
    /// Periodic transport-liveness tick the host watchdog tracks.
    Heartbeat { seq: u64 },
    /// Agent self-narration relayed from the `report` command (flattened, as
    /// today).
    Report(Report),
    /// The turn began processing — the delivery ack the host's resend loop
    /// waits on.
    TurnAccepted { turn_id: u64 },
    /// The turn ended (`Stop`). `reported = false` means it stopped *without* a
    /// terminal report — the case the host surfaces as a warning.
    TurnCompleted { turn_id: u64, reported: bool },
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

/// A line on the guest-local socket. Both the `report` command and the hook
/// bridge write here, so the union is **untagged**: resolution is by field
/// presence, not a discriminator tag, which keeps `report`'s existing wire
/// shape (`{"status",…}`) untouched.
///
/// A hook line has no `status`, so [`ReportLine`] (whose `status` is required,
/// no default) fails to deserialize and [`HookLine`] matches. The fields are
/// disjoint, so there is no ambiguity. Hook lines carry **no `turn_id`**; the
/// server stamps the current in-flight id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GuestLocalLine {
    /// `{"status","text","turn_id"?}` from `report` — unchanged.
    Report(ReportLine),
    /// `{"event":…}` from `report hook` — drives the turn-state machine.
    Hook(HookLine),
}

/// The line `report hook` writes: a lifecycle event from a Claude hook, with no
/// `turn_id` (the server stamps the current one).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookLine {
    pub event: HookEvent,
}

/// The lifecycle events the hook bridge forwards from Claude's hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookEvent {
    /// `SessionStart` (`startup` or `resume`): the agent is armed and idle.
    SessionReady,
    /// First activity on a turn — the turn began processing.
    TurnAccepted,
    /// `Stop`: the turn ended.
    TurnEnded,
}

/// The default vsock port the guest server listens on and the host connects to.
/// Arbitrary but fixed; the per-instance discriminator is the guest CID, not
/// the port.
pub const VSOCK_PORT: u32 = 1024;

/// `AF_VSOCK` well-known CID of the host (`VMADDR_CID_HOST`). The guest server
/// accepts a connection only from this CID: an in-guest loopback peer
/// presents as `VMADDR_CID_LOCAL` (1), so the unprivileged agent cannot inject
/// prompts into its own session — only the host can.
pub const VMADDR_CID_HOST: u32 = 2;

/// Fallback heartbeat cadence (seconds) the guest uses when `KATSU_HEARTBEAT_SECS`
/// is unset. Authoritative knob is Nix-driven; this only guards a missing
/// env var.
pub const HEARTBEAT_SECS_DEFAULT: u64 = 10;

/// Fallback grace window (milliseconds) the guest waits after a `Stop` for a late
/// terminal report before resolving the turn, when `KATSU_STOP_GRACE_MS`
/// is unset. Authoritative knob is Nix-driven.
pub const STOP_GRACE_MS_DEFAULT: u64 = 1500;

#[cfg(test)]
mod tests {
    use super::*;

    /// One serialized line must be exactly the NDJSON in — byte-for-byte,
    /// no trailing newline (callers add the `\n` per hop).
    fn assert_line<T: Serialize>(value: &T, expected: &str) {
        assert_eq!(serde_json::to_string(value).unwrap(), expected);
    }

    #[test]
    fn it_serializes_ready_unchanged() {
        assert_line(&GuestMessage::Ready, r#"{"type":"ready"}"#);
    }

    #[test]
    fn it_round_trips_session_ready() {
        let line = r#"{"type":"sessionready"}"#;
        assert_line(&GuestMessage::SessionReady, line);
        assert!(matches!(
            serde_json::from_str::<GuestMessage>(line).unwrap(),
            GuestMessage::SessionReady
        ));
    }

    #[test]
    fn it_round_trips_heartbeat() {
        let line = r#"{"type":"heartbeat","seq":42}"#;
        assert_line(&GuestMessage::Heartbeat { seq: 42 }, line);
        assert!(matches!(
            serde_json::from_str::<GuestMessage>(line).unwrap(),
            GuestMessage::Heartbeat { seq: 42 }
        ));
    }

    #[test]
    fn it_round_trips_report_flattened() {
        let line = r#"{"type":"report","status":"working","text":"building"}"#;
        let msg = GuestMessage::Report(Report {
            status: Status::Working,
            text: "building".into(),
            turn_id: None,
        });
        assert_line(&msg, line);
        assert!(matches!(
            serde_json::from_str::<GuestMessage>(line).unwrap(),
            GuestMessage::Report(Report {
                status: Status::Working,
                ..
            })
        ));
    }

    #[test]
    fn it_round_trips_turn_accepted() {
        let line = r#"{"type":"turnaccepted","turn_id":3}"#;
        assert_line(&GuestMessage::TurnAccepted { turn_id: 3 }, line);
        assert!(matches!(
            serde_json::from_str::<GuestMessage>(line).unwrap(),
            GuestMessage::TurnAccepted { turn_id: 3 }
        ));
    }

    #[test]
    fn it_round_trips_turn_completed() {
        let line = r#"{"type":"turncompleted","turn_id":3,"reported":false}"#;
        assert_line(
            &GuestMessage::TurnCompleted {
                turn_id: 3,
                reported: false,
            },
            line,
        );
        assert!(matches!(
            serde_json::from_str::<GuestMessage>(line).unwrap(),
            GuestMessage::TurnCompleted {
                turn_id: 3,
                reported: false
            }
        ));
    }

    #[test]
    fn it_skips_unknown_guest_message_variants() {
        // A newer peer's variant must decode-and-skip, not error.
        let unknown = r#"{"type":"somethingnew","field":7}"#;
        assert!(serde_json::from_str::<GuestMessage>(unknown).is_err());
        // The host's `drive` treats a decode error as a tolerated unknown and
        // continues; the contract is only that an unknown line never panics the
        // decoder, which a plain `Result::Err` satisfies.
    }

    #[test]
    fn it_decodes_report_line_as_report_arm() {
        // A `{"status":…}` line has the required `status` field → `Report`.
        let line = r#"{"status":"done","text":"shipped"}"#;
        match serde_json::from_str::<GuestLocalLine>(line).unwrap() {
            GuestLocalLine::Report(r) => {
                assert_eq!(r.status, Status::Done);
                assert_eq!(r.text, "shipped");
                assert_eq!(r.turn_id, None);
            }
            GuestLocalLine::Hook(_) => panic!("status line must decode as Report"),
        }
    }

    #[test]
    fn it_decodes_hook_line_as_hook_arm() {
        // A `{"event":…}` line has no `status`, so `ReportLine` fails and the
        // untagged union falls through to `Hook`.
        for (line, want) in [
            (r#"{"event":"turnended"}"#, HookEvent::TurnEnded),
            (r#"{"event":"sessionready"}"#, HookEvent::SessionReady),
            (r#"{"event":"turnaccepted"}"#, HookEvent::TurnAccepted),
        ] {
            match serde_json::from_str::<GuestLocalLine>(line).unwrap() {
                GuestLocalLine::Hook(h) => assert_eq!(h.event, want),
                GuestLocalLine::Report(_) => panic!("event line must decode as Hook"),
            }
        }
    }

    #[test]
    fn it_serializes_hook_lines_to_exact_ndjson() {
        assert_line(
            &GuestLocalLine::Hook(HookLine {
                event: HookEvent::TurnEnded,
            }),
            r#"{"event":"turnended"}"#,
        );
    }
}
