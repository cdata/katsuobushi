//! Async heartbeat runner: loads each heartbeat from disk, runs its `check`
//! body on its own interval, tracks beat state via the pure core in
//! [`crate::heartbeat`], and reports parse errors to stderr exactly once.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// Run `check` as `sh -c <check>` in `cwd`, bounded by `budget`. Returns
/// `true` iff the process exits zero within the budget. A process that
/// outlives the budget is killed and `false` is returned.
async fn spawn_check(check: &str, cwd: &Path, budget: Duration) -> bool {
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(check)
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("katsuobushi-heartbeat: failed to spawn check: {e}");
            return false;
        }
    };

    match timeout(budget, child.wait()).await {
        Ok(Ok(status)) => status.success(),
        Ok(Err(e)) => {
            eprintln!("katsuobushi-heartbeat: check wait failed: {e}");
            false
        }
        // Check outlived its interval: kill it and do not beat.
        Err(_elapsed) => {
            let _ = child.kill().await;
            false
        }
    }
}

/// Run `body` as `sh -c <body>` in `cwd`, bounded by `budget`. Returns the
/// first non-empty trimmed line of stdout if the body exits zero within the
/// budget. A failing or timing-out detail body returns `None` without
/// affecting the beat.
async fn spawn_detail(body: &str, cwd: &Path, budget: Duration) -> Option<String> {
    // `kill_on_drop(true)` ensures the child is killed if the future is
    // dropped on timeout — `wait_with_output` takes ownership of the child,
    // so an explicit `kill()` after the timeout is not possible.
    let child = Command::new("sh")
        .arg("-c")
        .arg(body)
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;

    match timeout(budget, child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.lines()
                .next()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
        }
        _ => None,
    }
}
