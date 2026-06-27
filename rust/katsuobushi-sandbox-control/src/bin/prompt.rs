//! katsuobushi-sandbox-prompt — the host-side sandbox controller client (design §8).
//!
//! Backs the `sandbox:prompt <instance> "<text>"` menu command: connect to a
//! guest's control endpoint, send one `Prompt`, then stream the guest's
//! `Report` lines to stdout until `done`/`blocked` (or the connection closes).
//!
//! Production talks AF_VSOCK to the guest CID; the local spike talks to a unix
//! socket (`--unix <path>`), mirroring the server's transport split so the
//! round-trip is provable without `vhost_vsock`.

use anyhow::{bail, Context as _};
use clap::Parser;
use katsuobushi_protocol::{GuestMessage, HostMessage, Prompt, Status, VSOCK_PORT};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

#[derive(Parser)]
#[command(about = "Push a prompt to a Katsuobushi agent sandbox and stream its reports")]
struct Args {
    /// Prompt text to inject as the next turn. Omitted only with --probe.
    text: Option<String>,
    /// Spike mode: connect to this unix socket instead of vsock.
    #[arg(long)]
    unix: Option<String>,
    /// Production: guest vsock CID to connect to.
    #[arg(long)]
    cid: Option<u32>,
    /// Turn id carried with the prompt (correlation is by ordering; this is
    /// for human-readable logs).
    #[arg(long, default_value_t = 1)]
    turn_id: u64,
    /// Readiness probe: connect, confirm the control server answers, then exit
    /// 0 without sending a prompt. Used by the runner to wait for boot.
    #[arg(long)]
    probe: bool,
}

/// Connect, send the prompt, then print every report until a terminal status
/// arrives. With `text == None` (probe), connect and return immediately.
async fn drive<S>(stream: S, turn_id: u64, text: Option<String>) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);

    let Some(text) = text else {
        return Ok(()); // probe: a successful connect is the signal
    };

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
            Ok(GuestMessage::Ready) => eprintln!("· guest ready"),
            Ok(GuestMessage::Report(r)) => {
                println!("[{:?}] {}", r.status, r.text);
                if matches!(r.status, Status::Done | Status::Blocked) {
                    break;
                }
            }
            Err(e) => eprintln!("· undecodable guest line: {e}"),
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.text.is_none() && !args.probe {
        bail!("provide prompt text, or pass --probe for a readiness check");
    }
    // --probe forces text to None even if some was passed.
    let text = if args.probe { None } else { args.text };

    match (&args.unix, args.cid) {
        (Some(path), _) => {
            let stream = tokio::net::UnixStream::connect(path)
                .await
                .with_context(|| format!("connect unix {path}"))?;
            drive(stream, args.turn_id, text).await
        }
        (None, Some(cid)) => {
            use tokio_vsock::{VsockAddr, VsockStream};
            let stream = VsockStream::connect(VsockAddr::new(cid, VSOCK_PORT))
                .await
                .with_context(|| format!("connect vsock cid {cid}"))?;
            drive(stream, args.turn_id, text).await
        }
        (None, None) => bail!("need --unix <path> (spike) or --cid <cid> (vsock)"),
    }
}
