//! The Nix→Rust spec contract.
//!
//! Nix renders a single JSON instance spec at flake-eval time (`builtins.toJSON`)
//! and hands it to `katsuctl` via `--config <path>`; these types are the
//! authoritative schema (Rust owns the schema, Nix produces JSON to match).
//! Every struct is `#[serde(deny_unknown_fields)]` so a field added
//! on the Nix side but not here fails loudly rather than being silently dropped,
//! and [`Spec::spec_version`] is checked on load so a stale `nix develop` shell
//! fails loud with a "rebuild your devshell" message instead of misbehaving.

// The schema and loader land ahead of the subcommands that consume them
// (phasing); each `Spec`/`Roots`/loader item is wired in as its
// command migrates, so they read as dead code until then.
#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The spec schema version this build of `katsuctl` understands. Bumped in
/// lockstep with the Nix renderer; [`load_spec`] fails loud on any mismatch
/// — no multi-version support, no migration.
pub const SUPPORTED_SPEC_VERSION: u32 = 4;

/// The complete Nix-rendered instance spec — the one source of truth for
/// everything Nix-derived.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Spec {
    /// Schema version; checked against [`SUPPORTED_SPEC_VERSION`] on load.
    pub spec_version: u32,
    /// e.g. `"cdata/katsuobushi"`.
    pub project_id: String,
    /// The unprivileged in-guest user, e.g. `"agent"`.
    pub agent_user: String,
    /// Whether to snapshot the host Nix DB into the instance.
    pub import_host_store_db: bool,

    /// State/runtime root templates (still carrying `$XDG_*`/`$HOME`).
    pub roots: Roots,
    /// Every pinned store-path binary `katsuctl` shells out to.
    pub tools: Tools,
    /// `${runner}/bin/microvm-run`.
    pub runner: PathBuf,
    /// e.g. `["rw-store.img", "nix-db.img", "scratch.img"]`.
    pub disk_images: Vec<String>,
    /// `validatedContext` relative paths to stage into the instance.
    pub context: Vec<String>,
    /// Declared secrets to stage to the runtime tmpfs.
    pub secrets: Vec<SecretSpec>,
    /// vsock port for the control channel (`protocol::VSOCK_PORT`, 1024).
    pub vsock_port: u32,
    /// Host CID (`protocol::VMADDR_CID_HOST`, 2).
    pub host_cid: u32,

    // Liveness tunables. Rendered from a
    // single set of Nix let-bindings into both this
    // spec and the guest env, so the two sides can never drift. Inert
    // until a later issue's consumer reads them — knob plumbing only.
    /// Heartbeat cadence in seconds (H); also exported as `KATSU_HEARTBEAT_SECS`.
    pub heartbeat_secs: u64,
    /// Missed heartbeats tolerated before "dead" (N): silence ≥ N·H is dead.
    pub heartbeat_miss: u32,
    /// Seconds of no progress before surfacing a "no progress" note (no break).
    pub progress_stall_secs: u64,
    /// Seconds to wait for `TurnAccepted` before resending a prompt.
    pub delivery_deadline_secs: u64,
    /// Max prompt resends before failing the delivery (K).
    pub delivery_retries: u32,
    /// Seconds to wait for `SessionReady` before sending anyway (G).
    pub ready_gate_secs: u64,
    /// Grace in ms to absorb a late terminal report after Stop; also exported as
    /// `KATSU_STOP_GRACE_MS`.
    pub stop_grace_ms: u64,

    /// Graphics capability; absent/`enable=false` ⇒ no GPU device, no compositor.
    #[serde(default)]
    pub graphics: GraphicsSpec,
}

/// State/runtime directory roots, carrying the `$XDG_*`/`$HOME` templates the
/// shell expands at runtime rather than baked absolute paths — `katsuctl` does
/// the same expansion in Rust via [`resolve_roots`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Roots {
    /// Durable state root, e.g. `"$XDG_STATE_HOME/katsuobushi/<projectId>"`.
    pub state_glob: PathBuf,
    /// Ephemeral runtime root, e.g. `"$XDG_RUNTIME_DIR/katsuobushi/<projectId>"`.
    pub runtime_glob: PathBuf,
}

/// The pinned store-path binaries `katsuctl` orchestrates — shell out to the
/// exact paths Nix supplies, native Rust only for sockets.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Tools {
    pub git: PathBuf,
    pub ssh: PathBuf,
    pub ssh_keygen: PathBuf,
    pub tmux: PathBuf,
    pub rsync: PathBuf,
    /// Only present when [`Spec::import_host_store_db`] is set.
    pub sqlite3: Option<PathBuf>,
    /// Interpreter for the emitted `start`/`attach` scripts.
    pub bash: PathBuf,
    /// The controller binary itself. The agent-mode `start` recipe tail-calls
    /// `prompt` (see `start::agent_tail`), and that emitted line runs in a child
    /// shell that may not have `katsuctl` on its PATH — so the recipe references
    /// this absolute path instead of a bare name.
    pub katsuctl: PathBuf,
}

/// One declared secret to stage into the instance.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretSpec {
    /// Guest-side credential name.
    pub name: String,
    /// Where the value comes from on the host.
    pub source: SecretSource,
    /// Runtime-tmpfs filename, `"cred-<name>"`.
    pub dest: String,
}

/// The host-side origin of a secret. Externally tagged to match the rendered
/// JSON exactly: `{ "fromEnv": "VAR" }` / `{ "fromFile": "/path" }`.
/// `katsuctl` never reads the *value* here — the emitted script re-reads the env
/// var or copies the file at runtime (references-never-values).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SecretSource {
    /// Read from this host environment variable at script runtime.
    FromEnv(String),
    /// Copy from this host file path at script runtime.
    FromFile(String),
}

/// Graphics capability for the instance. Absent or `enable=false` ⇒ no GPU
/// device and no compositor — byte-for-byte today's no-graphics behavior.
/// Mirrors the camelCase / `deny_unknown_fields` conventions of
/// [`SecretSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphicsSpec {
    /// Opt-in switch; `false` (the default) is exactly today's behavior.
    pub enable: bool,
    /// GPU role-preference ladder, ordered `[integrated, discrete, software]`;
    /// `[]` when disabled.
    #[serde(default)]
    pub gpu: Vec<GpuRole>,
    /// Virtual output geometry; `None` ⇒ default `1920x1080@60`.
    #[serde(default)]
    pub output: Option<Output>,
}

/// A GPU role rung in the selection ladder (`integrated`/`discrete`/`software`,
/// in that preference order). `software` is also a security rung — llvmpipe,
/// no GPU device, no host attack surface.
///
/// `Serialize` so the resolved rung can be persisted in `instance.json` (the
/// `graphics` field) and surfaced by `sandbox:status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuRole {
    Integrated,
    Discrete,
    Software,
}

impl GpuRole {
    /// The lowercase rung name (`integrated`/`discrete`/`software`) — the same
    /// token the config uses, surfaced in the preflight row
    /// (`will render on <role>: <node>`).
    pub fn as_str(self) -> &'static str {
        match self {
            GpuRole::Integrated => "integrated",
            GpuRole::Discrete => "discrete",
            GpuRole::Software => "software",
        }
    }
}

/// Headless-compositor virtual output geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Output {
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}

/// State/runtime roots with their `$XDG_*`/`$HOME` templates expanded
/// ([`resolve_roots`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoots {
    pub state_glob: PathBuf,
    pub runtime_glob: PathBuf,
}

/// Read, parse, and version-check the Nix-rendered spec at `path`.
///
/// Fails loud on a `specVersion` mismatch with a "rebuild your devshell" hint
/// — sandboxes are ephemeral, so there is no migration path.
pub fn load_spec(path: &Path) -> Result<Spec> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading sandbox spec at {}", path.display()))?;
    from_json_bytes(&bytes).with_context(|| format!("loading sandbox spec at {}", path.display()))
}

/// Parse + version-check spec JSON. Split out from [`load_spec`] so the schema
/// and skew checks are unit-testable without touching the filesystem.
fn from_json_bytes(bytes: &[u8]) -> Result<Spec> {
    let spec: Spec = serde_json::from_slice(bytes).context("parsing sandbox spec JSON")?;
    if spec.spec_version != SUPPORTED_SPEC_VERSION {
        bail!(
            "spec version {}, this katsuctl supports {} — rebuild your devshell \
             (the Nix-rendered sandbox spec is stale; exit and re-enter `nix develop`)",
            spec.spec_version,
            SUPPORTED_SPEC_VERSION,
        );
    }
    Ok(spec)
}

/// Expand the `$XDG_*`/`$HOME` templates in `roots` against the real process
/// environment. Mirrors the shell's
/// `${XDG_STATE_HOME:-$HOME/.local/state}` / `${XDG_RUNTIME_DIR:-/tmp}` fallbacks.
pub fn resolve_roots(roots: &Roots) -> Result<ResolvedRoots> {
    resolve_roots_with(roots, |k| std::env::var(k).ok())
}

/// [`resolve_roots`] over an injected environment lookup, so expansion is a pure
/// function testable against a fake env (tier 1).
fn resolve_roots_with(
    roots: &Roots,
    get: impl Fn(&str) -> Option<String>,
) -> Result<ResolvedRoots> {
    Ok(ResolvedRoots {
        state_glob: PathBuf::from(expand_template(path_str(&roots.state_glob)?, &get)?),
        runtime_glob: PathBuf::from(expand_template(path_str(&roots.runtime_glob)?, &get)?),
    })
}

/// Borrow a `PathBuf` as `&str`, failing loud on non-UTF-8 (the templates are
/// always UTF-8 store/path strings rendered by Nix).
fn path_str(p: &Path) -> Result<&str> {
    p.to_str()
        .with_context(|| format!("root template {} is not valid UTF-8", p.display()))
}

/// Expand `$XDG_STATE_HOME`, `$XDG_RUNTIME_DIR`, and `$HOME` in `template` with
/// the same shell-style `:-` fallbacks the runner uses (unset *or* empty falls
/// back). Longer `$XDG_*` tokens are substituted before `$HOME`, and their
/// fallbacks are fully resolved first, so no `$HOME` token can survive in a
/// substituted value.
fn expand_template(template: &str, get: &impl Fn(&str) -> Option<String>) -> Result<String> {
    let nonempty = |k: &str| get(k).filter(|v| !v.is_empty());
    let home = || {
        nonempty("HOME").context(
            "$HOME is not set; cannot expand the sandbox root templates \
             (needed for the $XDG_STATE_HOME fallback)",
        )
    };

    let state_home = match nonempty("XDG_STATE_HOME") {
        Some(v) => v,
        None => format!("{}/.local/state", home()?),
    };
    let runtime_dir = nonempty("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".to_string());

    let mut out = template
        .replace("$XDG_STATE_HOME", &state_home)
        .replace("$XDG_RUNTIME_DIR", &runtime_dir);
    if out.contains("$HOME") {
        out = out.replace("$HOME", &home()?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// The example, with the `…` store-hash ellipses filled in.
    const EXAMPLE_SPEC_JSON: &str = r#"{
      "specVersion": 4,
      "projectId": "cdata/katsuobushi",
      "agentUser": "agent",
      "importHostStoreDb": true,
      "roots": { "stateGlob": "$XDG_STATE_HOME/katsuobushi/cdata/katsuobushi",
                 "runtimeGlob": "$XDG_RUNTIME_DIR/katsuobushi/cdata/katsuobushi" },
      "tools": { "git": "/nix/store/h1-git/bin/git",
                 "ssh": "/nix/store/h2-openssh/bin/ssh",
                 "sshKeygen": "/nix/store/h2-openssh/bin/ssh-keygen",
                 "tmux": "/nix/store/h3-tmux/bin/tmux",
                 "rsync": "/nix/store/h4-rsync/bin/rsync",
                 "sqlite3": "/nix/store/h5-sqlite/bin/sqlite3",
                 "bash": "/nix/store/h6-bash/bin/bash",
                 "katsuctl": "/nix/store/h8-katsuctl/bin/katsuctl" },
      "runner": "/nix/store/h7-microvm-run/bin/microvm-run",
      "diskImages": ["rw-store.img", "nix-db.img", "scratch.img"],
      "context": [],
      "secrets": [ { "name": "CLAUDE_CODE_OAUTH_TOKEN",
                     "source": { "fromEnv": "HARNESS_OAUTH_TOKEN" },
                     "dest": "cred-CLAUDE_CODE_OAUTH_TOKEN" } ],
      "vsockPort": 1024,
      "hostCid": 2,
      "heartbeatSecs": 10,
      "heartbeatMiss": 3,
      "progressStallSecs": 300,
      "deliveryDeadlineSecs": 20,
      "deliveryRetries": 3,
      "readyGateSecs": 60,
      "stopGraceMs": 1500
    }"#;

    #[test]
    fn it_parses_the_design_example_spec() {
        let spec = from_json_bytes(EXAMPLE_SPEC_JSON.as_bytes()).expect("example should parse");

        assert_eq!(spec.spec_version, 4);
        assert_eq!(spec.project_id, "cdata/katsuobushi");
        assert_eq!(spec.agent_user, "agent");
        assert!(spec.import_host_store_db);
        assert_eq!(
            spec.roots.state_glob,
            PathBuf::from("$XDG_STATE_HOME/katsuobushi/cdata/katsuobushi")
        );
        assert_eq!(spec.tools.git, PathBuf::from("/nix/store/h1-git/bin/git"));
        assert_eq!(
            spec.tools.katsuctl,
            PathBuf::from("/nix/store/h8-katsuctl/bin/katsuctl")
        );
        assert_eq!(
            spec.tools.sqlite3,
            Some(PathBuf::from("/nix/store/h5-sqlite/bin/sqlite3"))
        );
        assert_eq!(
            spec.runner,
            PathBuf::from("/nix/store/h7-microvm-run/bin/microvm-run")
        );
        assert_eq!(
            spec.disk_images,
            vec!["rw-store.img", "nix-db.img", "scratch.img"]
        );
        assert!(spec.context.is_empty());
        assert_eq!(spec.vsock_port, 1024);
        assert_eq!(spec.host_cid, 2);

        // Liveness tunables parse with the defaults (inert knob plumbing).
        assert_eq!(spec.heartbeat_secs, 10);
        assert_eq!(spec.heartbeat_miss, 3);
        assert_eq!(spec.progress_stall_secs, 300);
        assert_eq!(spec.delivery_deadline_secs, 20);
        assert_eq!(spec.delivery_retries, 3);
        assert_eq!(spec.ready_gate_secs, 60);
        assert_eq!(spec.stop_grace_ms, 1500);

        assert_eq!(spec.secrets.len(), 1);
        let secret = &spec.secrets[0];
        assert_eq!(secret.name, "CLAUDE_CODE_OAUTH_TOKEN");
        assert_eq!(secret.dest, "cred-CLAUDE_CODE_OAUTH_TOKEN");
        assert_eq!(
            secret.source,
            SecretSource::FromEnv("HARNESS_OAUTH_TOKEN".to_string())
        );

        // No `graphics` field in this spec ⇒ the no-graphics default, exactly
        // today's behavior.
        assert_eq!(spec.graphics, GraphicsSpec::default());
        assert!(!spec.graphics.enable);
        assert!(spec.graphics.gpu.is_empty());
        assert_eq!(spec.graphics.output, None);
    }

    #[test]
    fn it_parses_the_from_file_secret_source() {
        let json = r#"{ "fromFile": "/run/secrets/token" }"#;
        let source: SecretSource = serde_json::from_str(json).expect("fromFile should parse");
        assert_eq!(
            source,
            SecretSource::FromFile("/run/secrets/token".to_string())
        );
    }

    #[test]
    fn it_omits_sqlite3_when_absent() {
        // sqlite3 is gated on importHostStoreDb, so the field may be missing.
        let json = EXAMPLE_SPEC_JSON
            .replace(r#""sqlite3": "/nix/store/h5-sqlite/bin/sqlite3","#, "")
            .replace(
                r#""importHostStoreDb": true"#,
                r#""importHostStoreDb": false"#,
            );
        let spec = from_json_bytes(json.as_bytes()).expect("missing sqlite3 should be fine");
        assert_eq!(spec.tools.sqlite3, None);
        assert!(!spec.import_host_store_db);
    }

    #[test]
    fn it_rejects_a_bad_spec_version() {
        let json = EXAMPLE_SPEC_JSON.replace(r#""specVersion": 4"#, r#""specVersion": 999"#);
        let err = from_json_bytes(json.as_bytes()).expect_err("version skew must fail loud");
        let msg = format!("{err:#}");
        assert!(msg.contains("999"), "should name the bad version: {msg}");
        assert!(
            msg.contains("rebuild your devshell"),
            "should hint the fix: {msg}"
        );
    }

    #[test]
    fn it_rejects_a_now_stale_v2_spec() {
        // The version check is the skew gate: a spec from any older Nix render
        // (e.g. pre-graphics v2, or pre-`tools.katsuctl` v3) must fail loud
        // rather than parse against a newer shape.
        let json = EXAMPLE_SPEC_JSON.replace(r#""specVersion": 4"#, r#""specVersion": 2"#);
        let err = from_json_bytes(json.as_bytes()).expect_err("v2 must now be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains('2'), "should name the stale version: {msg}");
        assert!(
            msg.contains("rebuild your devshell"),
            "should hint the fix: {msg}"
        );
    }

    #[test]
    fn it_rejects_an_unknown_field() {
        let json = EXAMPLE_SPEC_JSON.replace(
            r#""specVersion": 4,"#,
            r#""specVersion": 4, "surpriseField": "boom","#,
        );
        let err = from_json_bytes(json.as_bytes()).expect_err("deny_unknown_fields must fire");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("surpriseField"),
            "should name the field: {msg}"
        );
    }

    /// Splice the graphics-on block onto the example spec.
    fn with_graphics(block: &str) -> String {
        EXAMPLE_SPEC_JSON.replace(
            r#""stopGraceMs": 1500"#,
            &format!(r#""stopGraceMs": 1500, "graphics": {block}"#),
        )
    }

    #[test]
    fn it_parses_the_graphics_on_example() {
        // The exact graphics-on shape (version 3).
        let json = with_graphics(
            r#"{ "enable": true,
                 "gpu": ["integrated", "discrete", "software"],
                 "output": { "width": 1920, "height": 1080, "refresh": 60 } }"#,
        );
        let spec = from_json_bytes(json.as_bytes()).expect("graphics-on example should parse");
        assert!(spec.graphics.enable);
        assert_eq!(
            spec.graphics.gpu,
            vec![GpuRole::Integrated, GpuRole::Discrete, GpuRole::Software]
        );
        assert_eq!(
            spec.graphics.output,
            Some(Output {
                width: 1920,
                height: 1080,
                refresh: 60
            })
        );
    }

    #[test]
    fn it_rejects_an_unknown_field_inside_graphics() {
        let json = with_graphics(r#"{ "enable": true, "surpriseKnob": "boom" }"#);
        let err =
            from_json_bytes(json.as_bytes()).expect_err("graphics deny_unknown_fields must fire");
        let msg = format!("{err:#}");
        assert!(msg.contains("surpriseKnob"), "should name the field: {msg}");
    }

    #[test]
    fn it_loads_a_spec_from_a_file() {
        // Exercise the filesystem read path of load_spec end to end.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("katsuctl-spec-test-{}.json", std::process::id()));
        std::fs::write(&path, EXAMPLE_SPEC_JSON).expect("write temp spec");
        let spec = load_spec(&path).expect("load_spec should succeed");
        assert_eq!(spec.project_id, "cdata/katsuobushi");
        let _ = std::fs::remove_file(&path);
    }

    fn roots() -> Roots {
        Roots {
            state_glob: PathBuf::from("$XDG_STATE_HOME/katsuobushi/cdata/katsuobushi"),
            runtime_glob: PathBuf::from("$XDG_RUNTIME_DIR/katsuobushi/cdata/katsuobushi"),
        }
    }

    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn it_expands_roots_from_explicit_xdg_vars() {
        let env = fake_env(&[
            ("XDG_STATE_HOME", "/home/me/.state"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("HOME", "/home/me"),
        ]);
        let resolved = resolve_roots_with(&roots(), env).expect("should expand");
        assert_eq!(
            resolved.state_glob,
            PathBuf::from("/home/me/.state/katsuobushi/cdata/katsuobushi")
        );
        assert_eq!(
            resolved.runtime_glob,
            PathBuf::from("/run/user/1000/katsuobushi/cdata/katsuobushi")
        );
    }

    #[test]
    fn it_falls_back_to_home_and_tmp_when_xdg_unset() {
        // Neither XDG var set -> ${XDG_STATE_HOME:-$HOME/.local/state}, ${...:-/tmp}.
        let env = fake_env(&[("HOME", "/home/me")]);
        let resolved = resolve_roots_with(&roots(), env).expect("should expand via fallbacks");
        assert_eq!(
            resolved.state_glob,
            PathBuf::from("/home/me/.local/state/katsuobushi/cdata/katsuobushi")
        );
        assert_eq!(
            resolved.runtime_glob,
            PathBuf::from("/tmp/katsuobushi/cdata/katsuobushi")
        );
    }

    #[test]
    fn it_treats_empty_xdg_vars_as_unset() {
        // Shell `:-` falls back on empty, not just unset.
        let env = fake_env(&[
            ("XDG_STATE_HOME", ""),
            ("XDG_RUNTIME_DIR", ""),
            ("HOME", "/home/me"),
        ]);
        let resolved = resolve_roots_with(&roots(), env).expect("empty -> fallback");
        assert_eq!(
            resolved.state_glob,
            PathBuf::from("/home/me/.local/state/katsuobushi/cdata/katsuobushi")
        );
        assert_eq!(
            resolved.runtime_glob,
            PathBuf::from("/tmp/katsuobushi/cdata/katsuobushi")
        );
    }

    #[test]
    fn it_expands_a_literal_home_token() {
        let env = fake_env(&[("HOME", "/home/me"), ("XDG_RUNTIME_DIR", "/run/user/1000")]);
        let literal = Roots {
            state_glob: PathBuf::from("$HOME/.local/state/katsuobushi"),
            runtime_glob: PathBuf::from("$XDG_RUNTIME_DIR/katsuobushi"),
        };
        let resolved = resolve_roots_with(&literal, env).expect("should expand $HOME");
        assert_eq!(
            resolved.state_glob,
            PathBuf::from("/home/me/.local/state/katsuobushi")
        );
    }

    #[test]
    fn it_fails_loud_when_home_needed_but_unset() {
        // XDG_STATE_HOME unset and HOME unset -> the fallback cannot resolve.
        let env = fake_env(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
        let err = resolve_roots_with(&roots(), env).expect_err("missing HOME must fail loud");
        assert!(format!("{err:#}").contains("$HOME is not set"));
    }
}
