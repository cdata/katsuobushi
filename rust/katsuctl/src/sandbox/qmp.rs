//! Minimal native QMP client over the qemu monitor Unix socket.
//!
//! QMP is newline-delimited JSON over a Unix socket. Only two operations are
//! needed (design §2.3, §14.4), so this stays deliberately tiny — no general
//! client and no command beyond the two below until something needs more:
//!
//! - [`is_alive`] — liveness probe. Connecting to a live qemu monitor always
//!   yields the QMP greeting line; an absent or refused socket means the
//!   instance is gone. Mirrors the old socat connect probe (`isRunning` in
//!   `lib/sandbox/default.nix`).
//! - [`quit`] — the `qmp_capabilities` + `quit` shutdown handshake (the
//!   `sandbox:stop` exchange in `lib/sandbox/default.nix`).
//!
//! The handshake is fixed JSON, so this needs no serializer — `std` sockets
//! keep it synchronous, matching the rest of the crate's sync dispatch.
//!
//! Lands ahead of its callers: `sandbox status` consumes [`is_alive`] and
//! `sandbox stop` consumes [`quit`] in later migration steps (design §12), so
//! the API is unused (but tested) until then.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

/// Bound on the connect/greeting wait (mirrors socat's `-T1`). A live qemu
/// answers instantly; this only caps pathological cases (silent listener).
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Cap on draining qemu's QMP replies after sending `quit` — qemu normally
/// closes the socket within a moment of exiting; this only bounds a wedged
/// monitor so `quit` can't hang.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

/// The two newline-delimited JSON commands of the shutdown handshake: QMP
/// rejects any command before capability negotiation, so `qmp_capabilities`
/// must precede `quit`.
const QUIT_HANDSHAKE: &[u8] = b"{\"execute\":\"qmp_capabilities\"}\n{\"execute\":\"quit\"}\n";

/// True iff a qemu QMP monitor is listening at `sock_path` — i.e. connecting
/// succeeds and the socket yields its greeting line. An absent or refused
/// socket (instance gone) is `false`; this never errors.
pub fn is_alive(sock_path: &Path) -> bool {
    greeting(sock_path).is_some()
}

/// Connect and read the first (greeting) line. Any failure — missing socket,
/// connection refused, timeout, or an empty stream — yields `None`.
fn greeting(sock_path: &Path) -> Option<String> {
    let stream = UnixStream::connect(sock_path).ok()?;
    stream.set_read_timeout(Some(PROBE_TIMEOUT)).ok()?;
    let mut line = String::new();
    let read = BufReader::new(stream).read_line(&mut line).ok()?;
    (read > 0).then_some(line)
}

/// Ask the qemu monitor at `sock_path` to quit, sending the
/// `qmp_capabilities` + `quit` handshake as newline-delimited JSON.
pub fn quit(sock_path: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(sock_path)
        .with_context(|| format!("connecting to QMP socket {}", sock_path.display()))?;
    stream
        .write_all(QUIT_HANDSHAKE)
        .context("sending QMP quit handshake")?;
    stream.flush().context("flushing QMP quit handshake")?;
    // Half-close the write side, then drain qemu's replies to EOF. qemu processes
    // `quit` and exits, which closes the monitor socket — so this blocks just long
    // enough for the shutdown to actually take effect rather than dropping the
    // connection before qemu has read+acted on the command (the old shell held the
    // socket open with `sleep 1`). The read timeout bounds a wedged monitor.
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(SHUTDOWN_DRAIN_TIMEOUT));
    let mut drained = Vec::new();
    let _ = stream.read_to_end(&mut drained);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread;

    /// The greeting qemu emits on connect, before any command is sent.
    const GREETING: &[u8] = b"{\"QMP\": {\"version\": {}, \"capabilities\": []}}\r\n";

    /// A unique, unused socket path under the temp dir (one per test tag).
    fn socket_path(tag: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("katsuctl-qmp-{}-{}.sock", std::process::id(), tag));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn it_reports_alive_when_the_socket_sends_a_greeting() {
        let path = socket_path("alive");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.write_all(GREETING).unwrap();
        });

        assert!(is_alive(&path));

        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn it_reports_not_alive_when_nothing_is_listening() {
        let path = socket_path("dead");
        assert!(!is_alive(&path));
    }

    #[test]
    fn it_writes_the_capabilities_then_quit_handshake() {
        let path = socket_path("quit");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut received = Vec::new();
            conn.read_to_end(&mut received).unwrap();
            received
        });

        quit(&path).unwrap();

        let received = server.join().unwrap();
        assert_eq!(
            received,
            b"{\"execute\":\"qmp_capabilities\"}\n{\"execute\":\"quit\"}\n"
        );
        let _ = std::fs::remove_file(&path);
    }
}
