//! Async heartbeat runner: loads each heartbeat from disk, runs its `check`
//! body on its own interval, tracks beat state via the pure core in
//! [`crate::heartbeat`], and reports parse errors to stderr exactly once.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, MissedTickBehavior};

use crate::heartbeat::{apply_check, load_heartbeats, BeatState, CheckOutcome, Heartbeat};

/// Load all heartbeats from `hb_dir`, report any parse errors to stderr
/// exactly once, then run each successfully-parsed heartbeat on its own
/// interval with `cwd` as the working directory. Never returns normally.
///
/// Heartbeats run concurrently — one tokio task per heartbeat — so a slow
/// check on one heartbeat does not delay another.
pub async fn run_heartbeat_set(hb_dir: PathBuf, cwd: PathBuf) {
    let (heartbeats, errors) = load_heartbeats(&hb_dir);
    for err in &errors {
        eprintln!("katsuobushi-heartbeat: parse error: {err}");
    }

    for heartbeat in heartbeats {
        let cwd = cwd.clone();
        tokio::spawn(async move {
            run_one(heartbeat, cwd).await;
        });
    }
}

/// Run one heartbeat indefinitely: tick every `hb.interval`, run the check,
/// optionally run the detail body, and update the beat state via [`apply_check`].
async fn run_one(hb: Heartbeat, cwd: PathBuf) {
    let mut ticker = tokio::time::interval(hb.interval);
    // If a check + detail pair takes longer than the interval, skip the
    // missed ticks and wait a full interval before trying again — this keeps
    // the runner from firing back-to-back checks with no breathing room.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut state = BeatState::default();
    let timeout_secs = hb.timeout.as_secs();

    loop {
        ticker.tick().await;

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let beat = spawn_check(&hb.check, &cwd, hb.interval).await;

        let detail = if beat {
            match &hb.detail {
                Some(body) => spawn_detail(body, &cwd, hb.interval).await,
                None => None,
            }
        } else {
            None
        };

        let outcome = if beat {
            CheckOutcome::Beat { detail }
        } else {
            CheckOutcome::Miss
        };

        let (new_state, _status) = apply_check(state, outcome, now_secs, timeout_secs);
        // _status is consumed by a later card for work-state reporting.
        state = new_state;
    }
}

/// Kill every process in process group `pgid` with SIGKILL.
///
/// Called on timeout so that grandchildren spawned by the shell (e.g. a
/// `curl | jq` pipeline in a user-written check body) die alongside the
/// shell, not as long-lived orphans reparented to PID 1.
fn kill_pgroup(pgid: i32) {
    // Safety: kill(2) is always safe to call; a stale or already-reaped pgid
    // is benign — the kernel just returns ESRCH which we ignore.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

/// Run `check` as `sh -c <check>` in `cwd`, bounded by `budget`. Returns
/// `true` iff the process exits zero within the budget. A process that
/// outlives the budget has its entire process group killed; `false` is returned.
async fn spawn_check(check: &str, cwd: &Path, budget: Duration) -> bool {
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(check)
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("katsuobushi-heartbeat: failed to spawn check: {e}");
            return false;
        }
    };

    // child.id() returns None only after wait() has been polled — safe here.
    let pgid = match child.id() {
        Some(pid) => pid as i32,
        None => return matches!(child.wait().await, Ok(s) if s.success()),
    };

    match timeout(budget, child.wait()).await {
        Ok(Ok(status)) => status.success(),
        Ok(Err(e)) => {
            eprintln!("katsuobushi-heartbeat: check wait failed: {e}");
            false
        }
        // Check outlived its interval: kill the whole process group and reap.
        Err(_elapsed) => {
            kill_pgroup(pgid);
            let _ = child.wait().await;
            false
        }
    }
}

/// Run `body` as `sh -c <body>` in `cwd`, bounded by `budget`. Returns the
/// first non-empty trimmed line of stdout if the body exits zero within the
/// budget. A failing or timing-out detail body returns `None` without
/// affecting the beat.
async fn spawn_detail(body: &str, cwd: &Path, budget: Duration) -> Option<String> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(body)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;

    // child.id() returns None only after wait() has been polled — safe here.
    let pgid = child.id()? as i32;
    let mut stdout = child.stdout.take()?;

    // Drain stdout in a separate task so the pipe never fills and stalls the
    // shell, even for a detail body that writes unexpectedly large output.
    let drain = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    });

    match timeout(budget, child.wait()).await {
        Ok(Ok(status)) if status.success() => {
            let output = drain.await.unwrap_or_default();
            let text = String::from_utf8_lossy(&output);
            text.lines()
                .next()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
        }
        Ok(_) => {
            drain.abort();
            None
        }
        // Detail body outlived its budget: kill the whole process group and reap.
        Err(_elapsed) => {
            kill_pgroup(pgid);
            let _ = child.wait().await;
            drain.abort();
            None
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A check whose body sleeps far beyond the budget must return `false`
    /// promptly — the process group (shell + any grandchildren) is killed on
    /// timeout so no orphans accumulate.
    #[tokio::test]
    async fn it_returns_false_and_kills_the_process_group_when_the_check_times_out() {
        let result = spawn_check("sleep 10", Path::new("/tmp"), Duration::from_millis(50)).await;
        assert!(!result, "a timed-out check must return false");
    }

    /// A detail body that exits non-zero must yield `None` so the beat
    /// remains intact with its label alone — the runner maps `None` to
    /// `Beat { detail: None }`, carrying no narration.
    #[tokio::test]
    async fn it_returns_none_when_the_detail_body_exits_nonzero() {
        let result = spawn_detail("exit 1", Path::new("/tmp"), Duration::from_millis(500)).await;
        assert!(result.is_none(), "a failing detail body must yield None");
    }
}
