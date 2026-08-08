//! Async heartbeat runner: loads each heartbeat from disk, runs its `check`
//! body on its own interval, tracks beat state via the pure core in
//! [`crate::heartbeat`], and reports parse errors to stderr exactly once.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, MissedTickBehavior};

use crate::heartbeat::{
    apply_check, load_heartbeats, BeatState, BeatStatus, CheckOutcome, Heartbeat, HeartbeatError,
    TurnBeat,
};

/// A single beat-status update sent from a heartbeat task to the coordinator.
pub struct BeatStatusUpdate {
    /// The heartbeat's label; `TURN_BEAT_LABEL` for the turn heartbeat.
    pub label: String,
    /// The status produced by this tick.
    pub status: BeatStatus,
}

/// Label used by the turn heartbeat in [`BeatStatusUpdate`] messages.
pub const TURN_BEAT_LABEL: &str = "$turn";

/// Merge heartbeats from all directories in `dirs`. Errors are collected
/// without stopping remaining directories from loading.
pub fn collect_heartbeats(dirs: &[PathBuf]) -> (Vec<Heartbeat>, Vec<HeartbeatError>) {
    let mut heartbeats = Vec::new();
    let mut errors = Vec::new();
    for dir in dirs {
        let (hbs, errs) = load_heartbeats(dir);
        heartbeats.extend(hbs);
        errors.extend(errs);
    }
    (heartbeats, errors)
}

/// Load all heartbeats from every directory in `hb_dirs`, report any parse
/// errors to stderr exactly once, then run each successfully-parsed heartbeat
/// on its own interval with `cwd` as the working directory. Also spawns the
/// turn heartbeat, which beats for the life of the current turn. Never returns
/// normally.
///
/// Each heartbeat task sends [`BeatStatusUpdate`] values to `beat_tx` on every
/// tick so the server can aggregate them into a [`WorkState`] and detect
/// transitions.
///
/// Heartbeats run concurrently — one tokio task per heartbeat — so a slow
/// check on one heartbeat does not delay another.
///
/// [`WorkState`]: crate::heartbeat::WorkState
pub async fn run_heartbeat_set(
    hb_dirs: Vec<PathBuf>,
    cwd: PathBuf,
    turn_armed: Arc<AtomicBool>,
    beat_tx: tokio::sync::mpsc::UnboundedSender<BeatStatusUpdate>,
) {
    let (heartbeats, errors) = collect_heartbeats(&hb_dirs);
    for err in &errors {
        eprintln!("katsuobushi-heartbeat: parse error: {err}");
    }

    for heartbeat in heartbeats {
        let cwd = cwd.clone();
        let tx = beat_tx.clone();
        tokio::spawn(async move {
            run_one(heartbeat, cwd, tx).await;
        });
    }

    // The turn heartbeat joins the same set: one task, armed/silenced by the
    // server via `turn_armed`.
    tokio::spawn(run_turn_heartbeat(turn_armed, beat_tx));
}

/// Run the turn heartbeat indefinitely: ticks every [`TurnBeat::INTERVAL`],
/// reflecting the server's armed/silenced state via `armed`. Sends a
/// [`BeatStatusUpdate`] on every tick so the work-state coordinator can
/// aggregate it with the file heartbeats.
pub async fn run_turn_heartbeat(
    armed: Arc<AtomicBool>,
    beat_tx: tokio::sync::mpsc::UnboundedSender<BeatStatusUpdate>,
) {
    let mut ticker = tokio::time::interval(TurnBeat::INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut beat = TurnBeat::default();

    loop {
        ticker.tick().await;
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Sync armed flag from the shared atomic, then tick.
        beat.armed = armed.load(Ordering::Relaxed);
        let status = beat.tick(now_secs);
        let _ = beat_tx.send(BeatStatusUpdate {
            label: TURN_BEAT_LABEL.to_string(),
            status,
        });
    }
}

/// Run one heartbeat indefinitely: tick every `hb.interval`, run the check,
/// optionally run the detail body, update the beat state via [`apply_check`],
/// and send the resulting [`BeatStatusUpdate`] to the coordinator.
async fn run_one(
    hb: Heartbeat,
    cwd: PathBuf,
    beat_tx: tokio::sync::mpsc::UnboundedSender<BeatStatusUpdate>,
) {
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

        let (new_state, status) = apply_check(state, outcome, now_secs, timeout_secs);
        let _ = beat_tx.send(BeatStatusUpdate {
            label: hb.label.clone(),
            status,
        });
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
    use std::fs;
    use std::path::Path;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir()
                .join(format!("katsuobushi-runner-{tag}-{}", std::process::id()));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Heartbeats from two directories — shipped and project — must both appear
    /// in the merged result with no errors when both directories contain valid files.
    #[test]
    fn it_merges_heartbeats_from_shipped_and_project_dirs() {
        let shipped = TempDir::new("shipped");
        let project = TempDir::new("project");
        fs::write(
            shipped.path().join("agent-work.yaml"),
            "label: Agent work\ntimeout: 45m\ncheck: true\n",
        )
        .unwrap();
        fs::write(
            project.path().join("custom.yaml"),
            "label: Custom\ntimeout: 10m\ncheck: true\n",
        )
        .unwrap();

        let dirs = vec![shipped.path().to_owned(), project.path().to_owned()];
        let (heartbeats, errors) = collect_heartbeats(&dirs);
        assert_eq!(heartbeats.len(), 2, "expected one heartbeat per directory");
        assert!(errors.is_empty(), "expected no errors: {errors:?}");
        let labels: Vec<&str> = heartbeats.iter().map(|h| h.label.as_str()).collect();
        assert!(labels.contains(&"Agent work"), "{labels:?}");
        assert!(labels.contains(&"Custom"), "{labels:?}");
    }

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
