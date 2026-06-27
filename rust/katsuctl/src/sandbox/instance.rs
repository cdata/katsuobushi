//! Per-instance state — `instance.json` (design/katsuctl.md §6).
//!
//! Today the runner scatters scalar per-instance metadata across tiny files
//! (`instance`, `mode`, `vsock-cid`, `ssh-port`, and the `.named` marker —
//! lib/sandbox/default.nix:1341, 1343-1344, 1365, 1493). This folds them into a
//! single versioned `instance.json` that `katsuctl` owns and writes, living at
//! `<stateGlob>/<name>/instance.json`.
//!
//! Rust owns the schema (Nix/guest produce JSON to match), so every struct is
//! `#[serde(deny_unknown_fields)]` and [`Instance::instance_version`] is checked
//! on read — a stale reader fails loud rather than silently misbehaving, exactly
//! as the spec loader does (design §5.1, §14.6 — no migration, sandboxes are
//! ephemeral).
//!
//! Non-scalar artifacts stay as real files/dirs and are **not** modelled here:
//! `authorized_keys` (:1481), `console.log` (:1556), `sync.git/` (:1374), and the
//! disk images (:1510). Guest-side readers move to `instance.json` separately
//! (#016). Liveness is never stored — it is derived from QMP (`isRunning`,
//! :1606-1615).

// Read/write land ahead of the subcommands that consume them (design §12
// phasing), so they read as dead code until each command migrates.
#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The `instance.json` schema version this build of `katsuctl` understands.
/// Bumped in lockstep with any reader/writer; [`read`] fails loud on any
/// mismatch (design §6, §14.6 — no multi-version support, no migration).
pub const SUPPORTED_INSTANCE_VERSION: u32 = 1;

/// The mode a sandbox instance was started in (design §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// An interactive instance: a PTY handed to `ssh`+`tmux` (:1514-1546).
    Interactive,
    /// A detached agent instance, driven over the vsock control channel (:1550).
    Agent,
}

/// The consolidated scalar metadata for one sandbox instance (design §6).
///
/// Lives at `<stateGlob>/<name>/instance.json`; written by `katsuctl`, read by
/// later commands and (eventually, #016) the guest over 9p.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Instance {
    /// Schema version; checked against [`SUPPORTED_INSTANCE_VERSION`] on read.
    pub instance_version: u32,
    /// The full suffixed instance name (lib/sandbox/default.nix:1314).
    pub name: String,
    /// Whether the instance is interactive or an agent (:1344).
    pub mode: Mode,
    /// Persistent (`--name`d) instance; replaces the `.named` marker (:1341).
    pub named: bool,
    /// The host-side ssh port forwarded to the guest (:1493).
    pub ssh_port: u16,
    /// The allocated vsock CID — agent mode only, so `None` otherwise (:1365).
    pub vsock_cid: Option<u32>,
}

/// The `instance.json` path for `name` under the durable state root `state_dir`
/// (`<state_dir>/<name>/instance.json`, design §6).
fn instance_path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(name).join("instance.json")
}

/// Write `instance` to `<state_dir>/<instance.name>/instance.json`, creating the
/// per-instance directory if needed. Serialized pretty so the on-disk file stays
/// human-inspectable (it is the one source of truth for the instance's metadata).
pub fn write(state_dir: &Path, instance: &Instance) -> Result<()> {
    let path = instance_path(state_dir, &instance.name);
    let dir = path.parent().expect("instance_path always has a parent");
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating instance state dir {}", dir.display()))?;
    let json = serde_json::to_vec_pretty(instance).context("serializing instance.json")?;
    std::fs::write(&path, json)
        .with_context(|| format!("writing instance state to {}", path.display()))?;
    Ok(())
}

/// Read, parse, and version-check `<state_dir>/<name>/instance.json`.
///
/// Fails loud on an `instanceVersion` mismatch with a "rebuild your devshell"
/// hint (design §6) — sandboxes are ephemeral, so there is no migration path.
pub fn read(state_dir: &Path, name: &str) -> Result<Instance> {
    let path = instance_path(state_dir, name);
    let bytes = std::fs::read(&path)
        .with_context(|| format!("reading instance state at {}", path.display()))?;
    from_json_bytes(&bytes).with_context(|| format!("loading instance state at {}", path.display()))
}

/// Parse + version-check `instance.json` bytes. Split out from [`read`] so the
/// schema and skew checks are unit-testable without touching the filesystem.
fn from_json_bytes(bytes: &[u8]) -> Result<Instance> {
    let instance: Instance =
        serde_json::from_slice(bytes).context("parsing instance.json")?;
    if instance.instance_version != SUPPORTED_INSTANCE_VERSION {
        bail!(
            "instance.json version {}, this katsuctl supports {} — rebuild your devshell \
             (the instance state is stale; sandboxes are ephemeral — make a new one)",
            instance.instance_version,
            SUPPORTED_INSTANCE_VERSION,
        );
    }
    Ok(instance)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique, freshly-created temp dir under `std::env::temp_dir()`, removed on
    /// drop. No `tempfile` crate is vendored, so we roll a minimal RAII dir keyed
    /// on the test name + pid to avoid collisions between concurrent tests.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("katsuctl-instance-test-{}-{}", tag, std::process::id()));
            // Clear any stale leftover from a previous crashed run, then create.
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn agent_instance() -> Instance {
        Instance {
            instance_version: SUPPORTED_INSTANCE_VERSION,
            name: "katsuobushi-20260627-abc123".to_string(),
            mode: Mode::Agent,
            named: false,
            ssh_port: 2222,
            vsock_cid: Some(4242),
        }
    }

    #[test]
    fn it_round_trips_write_then_read() {
        let dir = TempDir::new("round-trip");
        let instance = agent_instance();
        write(dir.path(), &instance).expect("write should succeed");
        let read_back = read(dir.path(), &instance.name).expect("read should succeed");
        assert_eq!(read_back, instance);
    }

    #[test]
    fn it_round_trips_an_interactive_instance_without_a_cid() {
        // Interactive instances carry no vsock CID (design §6).
        let dir = TempDir::new("interactive");
        let instance = Instance {
            name: "katsuobushi-20260627-def456".to_string(),
            mode: Mode::Interactive,
            named: true,
            vsock_cid: None,
            ..agent_instance()
        };
        write(dir.path(), &instance).expect("write should succeed");
        let read_back = read(dir.path(), &instance.name).expect("read should succeed");
        assert_eq!(read_back, instance);
        assert_eq!(read_back.mode, Mode::Interactive);
        assert!(read_back.named);
        assert_eq!(read_back.vsock_cid, None);
    }

    #[test]
    fn it_writes_instance_json_at_the_per_instance_path() {
        // The file lands at <state_dir>/<name>/instance.json (design §6).
        let dir = TempDir::new("path");
        let instance = agent_instance();
        write(dir.path(), &instance).expect("write should succeed");
        let expected = dir.path().join(&instance.name).join("instance.json");
        assert!(expected.exists(), "instance.json missing at {}", expected.display());
    }

    #[test]
    fn it_serializes_with_camel_case_field_names() {
        // Nix/guest readers see camelCase keys and snake_case mode values.
        let json = serde_json::to_string(&agent_instance()).expect("serialize");
        assert!(json.contains("\"instanceVersion\""), "json: {json}");
        assert!(json.contains("\"sshPort\""), "json: {json}");
        assert!(json.contains("\"vsockCid\""), "json: {json}");
        assert!(json.contains("\"mode\":\"agent\""), "json: {json}");
    }

    #[test]
    fn it_rejects_a_bad_instance_version() {
        let json = serde_json::to_string(&Instance {
            instance_version: 999,
            ..agent_instance()
        })
        .expect("serialize");
        let err = from_json_bytes(json.as_bytes()).expect_err("version skew must fail loud");
        let msg = format!("{err:#}");
        assert!(msg.contains("999"), "should name the bad version: {msg}");
        assert!(
            msg.contains("rebuild your devshell"),
            "should hint the fix: {msg}"
        );
    }

    #[test]
    fn it_rejects_an_unknown_field() {
        let json = r#"{
            "instanceVersion": 1,
            "name": "katsuobushi-x",
            "mode": "agent",
            "named": false,
            "sshPort": 2222,
            "vsockCid": 4242,
            "surpriseField": "boom"
        }"#;
        let err = from_json_bytes(json.as_bytes()).expect_err("deny_unknown_fields must fire");
        let msg = format!("{err:#}");
        assert!(msg.contains("surpriseField"), "should name the field: {msg}");
    }

    #[test]
    fn it_errors_when_reading_a_missing_instance() {
        let dir = TempDir::new("missing");
        let err = read(dir.path(), "nope").expect_err("missing instance must error");
        assert!(format!("{err:#}").contains("reading instance state"));
    }
}
