//! `katsuctl sandbox start` — boot a new instance (the big one). Replaces the
//! shell `sandboxRunner`.
//!
//! The split is the whole point: every **probe-dependent
//! decision** is made *directly* in Rust through the [`Host`] seam — so it is
//! `FakeHost`-testable without booting a VM — and only the *results* are baked
//! into a flat, undecorated shell recipe the devshell wrapper `exec`s. The
//! decisions are:
//!
//! - the instance **name** (ephemeral `<timestamp>-<pid>` vs named
//!   `<friendly>-<8hex>`, with verbatim resume of an already-suffixed name;
//!   [`decide_name`]);
//! - the **ssh port** ([`pick_port`]) and, in agent mode, the **vsock
//!   CID** ([`pick_cid`] over the sibling instances' recorded CIDs);
//! - the **seed commit**: a resumed named branch as-is, else `git stash create`
//!   falling back to `HEAD` ([`resolve_seed`]);
//! - whether the bare **mirror** must be cloned (it is idempotent).
//!
//! The emitted recipe (see [`build_recipe`]) then contains only literals and
//! unconditional commands — its branching was all resolved here. Secrets are
//! emitted as **references, never values**: the script re-reads the
//! env var / copies the file at runtime, so no plaintext ever transits
//! `katsuctl` stdout or a golden snapshot.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::sandbox::emit::{self, Recipe};
use crate::sandbox::gfx::{self, Resolution};
use crate::sandbox::host::{pick_cid, pick_port, Host, HostImpl, OsRng, Rng};
use crate::sandbox::instance::{self, Instance, Mode, SUPPORTED_INSTANCE_VERSION};
use crate::sandbox::spec::{load_spec, resolve_roots, ResolvedRoots, SecretSource, Spec};
use crate::Global;

/// How the instance branch is seeded.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seed {
    /// Resume a named instance from its existing branch commit — **no** push is
    /// emitted (the accumulated work is continued as-is).
    Resume(String),
    /// Seed a fresh branch from this commit — the recipe pushes it.
    Fresh(String),
}

/// Every decision `katsuctl` makes before emitting — the act-directly results the
/// flat recipe is built from. Returned by [`decide`] so the seam
/// tests can assert each decision without a real boot.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    /// Full suffixed instance name.
    name: String,
    /// Persistent (`--name`d) instance — replaces the `.named` marker.
    named: bool,
    /// Interactive attach vs detached agent.
    mode: Mode,
    /// The probed free loopback ssh port.
    ssh_port: u16,
    /// Agent-mode vsock CID; `None` for interactive.
    vsock_cid: Option<u32>,
    /// The host project root (`git rev-parse --show-toplevel`).
    project: PathBuf,
    /// Whether the bare mirror is missing and must be cloned.
    clone_mirror: bool,
    /// How the branch is seeded.
    seed: Seed,
    /// The agent-mode initial prompt to tail-call `prompt` with.
    prompt: Option<String>,
    /// The resolved GPU ladder verdict when graphics is enabled, else `None`.
    /// Decided here (the one graphics probe) so both the recipe and the persisted
    /// `instance.json` read the *same* resolution; never holds `Unavailable` (that
    /// fails the launch loud in [`decide`] before a `Plan` is built).
    gpu: Option<Resolution>,
}

impl Plan {
    /// The GPU rung to record in `instance.json` (and surface in `sandbox:status`):
    /// the role a hardware rung satisfied, `software` for the llvmpipe rung, or
    /// `None` when graphics is disabled.
    fn gpu_rung(&self) -> Option<crate::sandbox::spec::GpuRole> {
        match &self.gpu {
            Some(Resolution::Gpu { role, .. }) => Some(*role),
            Some(Resolution::Software) => Some(crate::sandbox::spec::GpuRole::Software),
            Some(Resolution::Unavailable) | None => None,
        }
    }
}

/// Production entry point: load the spec, stand up the real host
/// seam, make every probe-dependent decision in Rust, persist `instance.json`,
/// then emit the flat recipe (printing only its path for the wrapper to `exec`).
pub fn run(
    config: &Path,
    agent: bool,
    name: Option<String>,
    prompt: Option<String>,
    global: Global,
) -> Result<()> {
    let spec = load_spec(config)?;
    let host = HostImpl::new().context("initializing the host IO seam")?;
    let roots = resolve_roots(&spec.roots)?;

    // Serialize the allocation window across concurrent `start`s: the CID/port
    // picks read sibling `instance.json` files, and this launch's own record is
    // not written until after `decide` returns — unserialized, two parallel
    // launches (the swarm workflow) could claim the same CID or port. The
    // advisory flock covers probe→persist and releases when this process exits.
    let _alloc_lock = lock_allocation(&roots.state_glob)?;

    let mut rng = OsRng::new();
    let clock = now_timestamp()?;
    let pid = std::process::id();
    let plan = decide(
        &host,
        &mut rng,
        &roots,
        &spec,
        agent,
        name.as_deref(),
        prompt.as_deref(),
        &clock,
        pid,
    )?;

    // `--json` *describes* the resolved identity rather than emitting a script:
    // the bare form prints a path to `exec`, `--json` says what will
    // happen. A power-user/structured caller, so no side effects either.
    if global.json {
        println!("{}", identity_json(&plan));
        return Ok(());
    }

    // Persist the consolidated scalar metadata the later commands and the guest
    // read before booting.
    let meta = Instance {
        instance_version: SUPPORTED_INSTANCE_VERSION,
        name: plan.name.clone(),
        mode: plan.mode,
        named: plan.named,
        ssh_port: plan.ssh_port,
        vsock_cid: plan.vsock_cid,
        graphics: plan.gpu_rung(),
    };
    instance::write(&roots.state_glob, &meta).context("writing instance.json")?;

    let script_dir = emit::script_runtime_dir();
    emit::emit(&host, &script_dir, &mut rng, || {
        build_recipe(&spec, config, &roots, &plan)
    })?;
    Ok(())
}

/// The testable planning core (tier 3): make every probe-dependent
/// decision through the seam and return them as a [`Plan`]. No filesystem writes
/// happen here — `instance.json` and the emitted script are side effects [`run`]
/// performs afterward — so a [`FakeHost`](crate::sandbox::host::FakeHost) drives
/// the whole thing.
#[allow(clippy::too_many_arguments)]
fn decide(
    host: &impl Host,
    rng: &mut impl Rng,
    roots: &ResolvedRoots,
    spec: &Spec,
    agent: bool,
    name: Option<&str>,
    prompt: Option<&str>,
    clock: &str,
    pid: u32,
) -> Result<Plan> {
    // `--prompt` implies agent mode, exactly as the shell runner did.
    let mode = if agent || prompt.is_some() {
        Mode::Agent
    } else {
        Mode::Interactive
    };

    // Validate + generate the name *before* any IO so a hostile `--name` bails
    // here, before instance.json is written or a recipe is emitted.
    let (full_name, named) = decide_name(name, clock, pid, rng)?;

    // Sibling claims (recorded CIDs *and* ports), gathered before any
    // allocation: a sibling's ssh port is not bound until its qemu boots, so
    // the bind probe alone cannot see a just-planned launch's claim.
    let claims = gather_sibling_claims(host, &roots.state_glob, &full_name);

    // Probe a free loopback port, also skipping sibling-recorded ports.
    let ssh_port = pick_port(
        |p| !claims.used_ports.contains(&p) && host.port_is_free(p),
        rng,
    )?;

    // Agent mode allocates a vsock CID not claimed by a sibling; a resumed named
    // instance keeps its already-recorded CID.
    let vsock_cid = match mode {
        Mode::Interactive => None,
        Mode::Agent => Some(match claims.own_cid {
            Some(cid) => cid,
            None => pick_cid(&claims.used_cids, rng)?,
        }),
    };

    let project = resolve_project(host, &spec.tools.git)?;
    let state_root = roots.state_glob.join(&full_name);
    let sync_git = state_root.join("sync.git");
    let branch = format!("refs/heads/sandbox/{full_name}");
    // The mirror is reused if it already exists; its absence is what drives the
    // emitted (idempotent) clone and the resume-vs-seed decision.
    let mirror_exists = host.exists(&sync_git);
    let seed = resolve_seed(
        host,
        &spec.tools.git,
        &project,
        &sync_git,
        &branch,
        named,
        mirror_exists,
    )?;

    // The one graphics probe: walk the GPU role ladder against the host now, so
    // the recipe and the persisted instance.json share a single resolution. An
    // exhausted ladder with no `software` tail fails the launch loud here rather
    // than booting GPU-less and slow.
    let gpu = if spec.graphics.enable {
        match gfx::resolve_gpu(&spec.graphics.gpu, host) {
            Resolution::Unavailable => {
                bail!("graphics: no usable GPU and no `software` fallback in `gpu`")
            }
            resolved => Some(resolved),
        }
    } else {
        None
    };

    Ok(Plan {
        name: full_name,
        named,
        mode,
        ssh_port,
        vsock_cid,
        project,
        clone_mirror: !mirror_exists,
        seed,
        prompt: prompt.map(str::to_string),
        gpu,
    })
}

/// Generate the instance name:
///
/// - **no `--name`** → ephemeral `<timestamp>-<pid>`; a timestamp + pid is unique
///   enough on its own;
/// - **`--name <friendly>`** → mint a *fresh* instance by appending 8 hex of
///   entropy, so a friendly name never silently resumes an older same-named
///   branch;
/// - **`--name <…-8hex>`** → an already-suffixed full name (copied back from a
///   prior launch) is taken **verbatim**, which is how you deliberately resume one
///   specific instance.
///
/// Returns the full name and whether it is named (persistent). Pure given the
/// injected clock/pid/RNG, so it is an ordinary unit test.
///
/// **Security:** the name is interpolated as *literal* script text throughout the
/// emitted recipe (mkdir paths, the `refs/heads/sandbox/<name>` branch, echoes,
/// and the `prompt` tail-call) — unlike the old shell runner, which kept it in an
/// inert `$instance` variable. `--name` is unvalidated operator input, so it is
/// validated to a shell-safe charset here: the raw input is rejected up front (so
/// the friendly part can't smuggle `"`/`$`/`` ` ``/`\`), and the final name is
/// re-checked as defense in depth. A rejection bails before any IO in [`decide`].
fn decide_name(
    name: Option<&str>,
    clock: &str,
    pid: u32,
    rng: &mut impl Rng,
) -> Result<(String, bool)> {
    let (full, named) = match name {
        None => (format!("{clock}-{pid}"), false),
        Some(friendly) => {
            // Reject metacharacters in the raw `--name` up front, so neither the
            // friendly prefix nor a verbatim-resume name can carry shell syntax.
            validate_instance_name(friendly)?;
            if has_hex8_suffix(friendly) {
                (friendly.to_string(), true)
            } else {
                let suffix = format!("{:08x}", rng.next_u32());
                (format!("{friendly}-{suffix}"), true)
            }
        }
    };
    // Defense in depth: the final name (incl. the ephemeral `<ts>-<pid>`) is baked
    // as literal script text, so assert it is shell-safe before it goes anywhere.
    validate_instance_name(&full)?;
    Ok((full, named))
}

/// Assert `name` is a non-empty string of `[A-Za-z0-9._-]` only — the charset that
/// is safe to interpolate unescaped into the emitted recipe (no shell
/// metacharacters, no whitespace, no path traversal via anything but the literal
/// chars). Anything else is rejected with a clear, actionable error.
fn validate_instance_name(name: &str) -> Result<()> {
    let safe = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
    if name.is_empty() || !name.bytes().all(safe) {
        bail!(
            "invalid instance name {name:?}: instance names may contain only \
             letters, digits, '.', '_' and '-' (got a disallowed character)"
        );
    }
    Ok(())
}

/// Whether `name` already carries our `-<8 lowercase hex>` suffix — the same
/// `-[0-9a-f]{8}$` test the shell uses.
fn has_hex8_suffix(name: &str) -> bool {
    let bytes = name.as_bytes();
    let n = bytes.len();
    // `-` + exactly 8 hex digits at the very end.
    n >= 9
        && bytes[n - 9] == b'-'
        && bytes[n - 8..]
            .iter()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Collect the vsock CIDs and ssh ports already claimed by *sibling* instances,
/// plus this instance's own recorded CID when it is being resumed. Each
/// sibling's claims are read from its `instance.json` through the seam, so the
/// whole sweep is `FakeHost`-testable. A missing/unreadable/parse-failing
/// sibling is simply skipped (best-effort, as the shell's `cat … 2>/dev/null`
/// was).
fn gather_sibling_claims(host: &impl Host, state_glob: &Path, current: &str) -> SiblingClaims {
    let mut claims = SiblingClaims::default();
    let names = match host.list_dir(state_glob) {
        Ok(names) => names,
        Err(_) => return claims,
    };
    for name in names {
        if name.starts_with('.') {
            continue;
        }
        let path = state_glob.join(&name).join("instance.json");
        let Ok(bytes) = host.read(&path) else {
            continue;
        };
        let Some((cid, port)) = parse_claims(&bytes) else {
            continue;
        };
        if name == current {
            claims.own_cid = cid;
        } else {
            if let Some(cid) = cid {
                claims.used_cids.insert(cid);
            }
            if let Some(port) = port {
                claims.used_ports.insert(port);
            }
        }
    }
    claims
}

/// The resources sibling instances have recorded (and the current instance's
/// own prior CID, for a verbatim resume). Gathered once per `decide` and
/// consulted by both the port and CID picks.
#[derive(Debug, Default)]
struct SiblingClaims {
    used_cids: HashSet<u32>,
    used_ports: HashSet<u16>,
    own_cid: Option<u32>,
}

/// Extract just the `vsockCid` + `sshPort` from an `instance.json` blob,
/// tolerating any other fields (this is a claims census, not a full load, so it
/// must not fail on a schema-newer sibling).
fn parse_claims(bytes: &[u8]) -> Option<(Option<u32>, Option<u16>)> {
    #[derive(serde::Deserialize)]
    struct ClaimProbe {
        #[serde(rename = "vsockCid")]
        vsock_cid: Option<u32>,
        #[serde(rename = "sshPort")]
        ssh_port: Option<u16>,
    }
    serde_json::from_slice::<ClaimProbe>(bytes)
        .ok()
        .map(|c| (c.vsock_cid, c.ssh_port))
}

/// Resolve the host project root via `git rev-parse --show-toplevel` (run through
/// the seam). Baked into the recipe as the clone/seed source.
fn resolve_project(host: &impl Host, git: &Path) -> Result<PathBuf> {
    let mut cmd = Command::new(git);
    cmd.arg("rev-parse").arg("--show-toplevel");
    let out = host
        .run(&cmd)
        .context("running `git rev-parse --show-toplevel`")?;
    if !out.status.success() {
        bail!("`git rev-parse --show-toplevel` failed — are you inside the project repo?");
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        bail!("`git rev-parse --show-toplevel` returned no path");
    }
    Ok(PathBuf::from(path))
}

/// Resolve the seed commit:
///
/// - a **named** instance whose mirror already carries the branch is resumed from
///   that exact commit ([`Seed::Resume`]) — no re-seed, so the agent's
///   accumulated work continues;
/// - otherwise the branch is seeded from a snapshot of the host working tree
///   (`git stash create`, capturing tracked + staged changes), falling back to
///   `HEAD` when the tree is clean and `stash create` prints nothing
///   ([`Seed::Fresh`]).
///
/// All git calls go through the seam so the branch is decided without touching a
/// real repo.
fn resolve_seed(
    host: &impl Host,
    git: &Path,
    project: &Path,
    sync_git: &Path,
    branch: &str,
    named: bool,
    mirror_exists: bool,
) -> Result<Seed> {
    if named && mirror_exists {
        let mut verify = Command::new(git);
        verify
            .arg("-C")
            .arg(sync_git)
            .arg("rev-parse")
            .arg("--verify")
            .arg(branch);
        if let Ok(out) = host.run(&verify) {
            if out.status.success() {
                let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !commit.is_empty() {
                    return Ok(Seed::Resume(commit));
                }
            }
        }
    }

    // Fresh seed: a working-tree snapshot, else HEAD.
    let mut stash = Command::new(git);
    stash.arg("-C").arg(project).arg("stash").arg("create");
    let snap = match host.run(&stash) {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    };
    let commit = if snap.is_empty() {
        let mut head = Command::new(git);
        head.arg("-C").arg(project).arg("rev-parse").arg("HEAD");
        let out = host.run(&head).context("running `git rev-parse HEAD`")?;
        if !out.status.success() {
            bail!("`git rev-parse HEAD` failed — the project repo has no commits?");
        }
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        snap
    };
    if commit.is_empty() {
        bail!("could not resolve a seed commit (neither `stash create` nor HEAD produced one)");
    }
    Ok(Seed::Fresh(commit))
}

/// The ephemeral-name timestamp (`YYYYMMDD-HHMMSS`, UTC), formatted in Rust:
/// the recipe contract runs every world-touching tool by its pinned store
/// path, and shelling out to a bare-PATH `date` was the lone exception (and an
/// avoidable subprocess). UTC where the shell used local time — the stamp only
/// needs rough sortability; uniqueness comes from the appended pid. Kept out
/// of [`decide`] so the core stays pure on an injected clock string.
fn now_timestamp() -> Result<String> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    let (y, mo, d, h, mi, s) = crate::sandbox::liveness::unix_to_civil(secs);
    Ok(format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}"))
}

/// Take the project-wide allocation lock: an exclusive advisory `flock` on a
/// dotfile under the state root (dot-prefixed, so the sibling sweep skips it).
/// Blocks until any concurrent `start` finishes its probe→persist window; the
/// lock releases when the returned handle drops (or the process exits, however
/// it exits). Direct `std::fs` rather than the [`Host`] seam: this is [`run`]'s
/// world-touching layer, and [`decide`] stays pure.
fn lock_allocation(state_glob: &Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(state_glob)
        .with_context(|| format!("creating the state root {}", state_glob.display()))?;
    let path = state_glob.join(".start.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening the allocation lock {}", path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    Ok(file)
}

/// The `<base>/katsuobushi` directory whose mode is clamped to 700 so no *other*
/// host user can descend to the world-writable bare mirror inside (the 9p
/// push-permission saga). `state_glob` ends
/// with `project_id`, so stripping its components yields the clamp target.
fn katsuobushi_base(state_glob: &Path, project_id: &str) -> PathBuf {
    let mut base = state_glob.to_path_buf();
    for _ in 0..Path::new(project_id).components().count() {
        base.pop();
    }
    base
}

/// The resolved-identity JSON `start --json` prints: name/mode/port/
/// cid — *not* the script path.
fn identity_json(plan: &Plan) -> String {
    serde_json::json!({
        "name": plan.name,
        "mode": plan.mode.as_str(),
        "named": plan.named,
        "sshPort": plan.ssh_port,
        "vsockCid": plan.vsock_cid,
    })
    .to_string()
}

// ---- recipe construction -------------------------------------

/// Single-quote a path for the emitted shell. Double quotes would leave `$`,
/// backticks, and `\` shell-active — and these paths are host-derived (the git
/// toplevel, XDG-expanded roots, context entries), not validated like instance
/// names — so they must never be shell-interpreted. Single quotes cover spaces
/// too; escaping delegates to [`sq`].
fn qp(p: &Path) -> String {
    sq(&p.display().to_string())
}

/// Single-quote arbitrary text for the emitted shell (the `--prompt` payload is
/// attacker-shaped: it may carry quotes, `$`, spaces). `'\''` is the standard
/// close-escape-reopen idiom.
fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build the flat setup + boot recipe. Pure over the [`Plan`] and spec, so the
/// golden snapshots render it directly — every branch was already decided in
/// [`decide`] (including the GPU resolution carried in [`Plan::gpu`]); what is
/// emitted is unconditional, apart from the genuinely-runtime secret-presence
/// and file-existence guards.
fn build_recipe(spec: &Spec, config: &Path, roots: &ResolvedRoots, plan: &Plan) -> Result<Recipe> {
    let name = &plan.name;
    let state_root = roots.state_glob.join(name);
    let runtime_root = roots.runtime_glob.join(name);
    let sync_git = state_root.join("sync.git");
    let state_base = katsuobushi_base(&roots.state_glob, &spec.project_id);
    let console_log = state_root.join("console.log");
    let branch = format!("refs/heads/sandbox/{name}");

    let git = spec.tools.git.display().to_string();
    let ssh = spec.tools.ssh.display().to_string();
    let ssh_keygen = spec.tools.ssh_keygen.display().to_string();
    let rsync = spec.tools.rsync.display().to_string();
    let runner = spec.runner.display().to_string();

    let mut r = Recipe::new();
    r.comment(format!(
        "katsuctl sandbox start: set up and boot {} instance '{name}'",
        plan.mode.as_str()
    ));

    // ---- dirs + the parent clamp ----
    r.line(format!(
        "mkdir -p {} {}",
        qp(&state_root),
        qp(&runtime_root)
    ));
    r.line(format!("chmod 700 {}", qp(&runtime_root)));
    r.line(format!("chmod 700 {}", qp(&state_base)));
    // Open the per-instance share root itself (non-recursive, so the large
    // image files keep their perms) so the agent-run guest controller can
    // create entries here — notably turn-state.json. The 9p share is
    // mapped-xattr (files the guest creates are recorded
    // agent-owned), but a host-created root-owned dir is otherwise unwritable by
    // the agent; the parent state_base is clamped 700, so this only widens
    // within the per-instance dir. Mirrors the sync.git push-perm chmod below.
    r.line(format!("chmod a+rwX {}", qp(&state_root)));

    // ---- bare mirror (idempotent) + branch seed + push-perm chmod ----
    r.blank().comment(
        "Per-instance bare git mirror + seeded branch (the guest clones it and pushes back).",
    );
    if plan.clone_mirror {
        r.line(format!(
            "{git} clone --bare {} {} >/dev/null 2>&1",
            qp(&plan.project),
            qp(&sync_git)
        ));
    }
    match &plan.seed {
        Seed::Fresh(commit) => {
            r.line(format!(
                "{git} -C {} push --quiet {} \"{commit}:{branch}\" --force",
                qp(&plan.project),
                qp(&sync_git)
            ));
        }
        Seed::Resume(commit) => {
            r.comment(format!(
                "resuming named instance from its existing branch ({commit})"
            ));
        }
    }
    // Re-open the whole mirror to "other" writes so the guest can push (the
    // mapped-xattr saga) — run every launch, idempotent.
    r.line(format!("chmod -R a+rwX {}", qp(&sync_git)));

    // ---- importHostStoreDb snapshot, only when enabled ----
    if spec.import_host_store_db {
        let tmp = state_root.join(".nix-db.sqlite.tmp");
        let dest = state_root.join("nix-db.sqlite");
        let sqlite = spec
            .tools
            .sqlite3
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "sqlite3".to_string());
        r.blank()
            .comment("Snapshot the host Nix DB so the guest reuses host-built paths (non-fatal).");
        r.line(r#"hostdb="${NIX_STATE_DIR:-/nix/var/nix}/db/db.sqlite""#.to_string());
        // The dot-command is single-quoted for the *shell* (the path must not be
        // shell-interpreted); the inner quotes are sqlite's own dot-arg quoting.
        r.line(format!(
            "{sqlite} \"$hostdb\" {} 2>/dev/null && mv -f {} {} || true",
            sq(&format!(".backup '{}'", tmp.display())),
            qp(&tmp),
            qp(&dest)
        ));
    }

    // ---- context staging, only when declared ----
    if !spec.context.is_empty() {
        let ctx_root = state_root.join("context");
        r.blank().comment(
            "Stage declared untracked context (rsync --safe-links drops escaping symlinks).",
        );
        r.line(format!("rm -rf {}", qp(&ctx_root)));
        r.line(format!("mkdir -p {}", qp(&ctx_root)));
        for p in &spec.context {
            let src = plan.project.join(p);
            let dst = ctx_root.join(p);
            let dst_parent = dst
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| ctx_root.clone());
            // Whether the context path exists is genuinely a runtime fact, so this
            // existence guard stays in the script (the shell did the same).
            r.line(format!(
                "[ -e {} ] && {{ mkdir -p {}; {rsync} -a --safe-links {} {}/; }} || true",
                qp(&src),
                qp(&dst_parent),
                qp(&src),
                qp(&dst_parent)
            ));
        }
    }

    // ---- secrets as REFERENCES, never values ----
    if !spec.secrets.is_empty() {
        r.blank()
            .comment("Declared secrets, staged as references — the value is re-read at runtime, never baked in.");
        for s in &spec.secrets {
            let cred = runtime_root.join(&s.dest);
            match &s.source {
                SecretSource::FromEnv(var) => {
                    // The env var is read at script-exec time (the wrapper's
                    // process tree already exports it); its *value* is never seen
                    // by katsuctl, so it cannot land in a snapshot.
                    r.line(format!("if [ -z \"${{{var}:-}}\" ]; then"));
                    r.line(format!(
                        "  echo \"sandbox: required secret {} is not set on the host (expected in \\${var}).\" >&2",
                        s.name
                    ));
                    r.line("  exit 1".to_string());
                    r.line("fi".to_string());
                    // Recreate under a subshell umask so the file is *born* 0600 —
                    // a plain `>` then `chmod` would leave a window where the
                    // plaintext token is world-readable under the default umask
                    // (the fromFile branch gets the same guarantee from
                    // `install -m 0600`). The `rm -f` matters: `>` alone would
                    // keep a pre-existing file's looser mode.
                    r.line(format!(
                        "rm -f {} && (umask 077; printf '%s' \"${{{var}}}\" > {})",
                        qp(&cred),
                        qp(&cred)
                    ));
                    r.line(format!("export KATSU_CRED_{}={}", s.name, qp(&cred)));
                }
                SecretSource::FromFile(path) => {
                    let src = Path::new(path);
                    r.line(format!("if [ ! -r {} ]; then", qp(src)));
                    r.line(format!(
                        "  echo \"sandbox: required secret {} not readable at {}.\" >&2",
                        s.name, path
                    ));
                    r.line("  exit 1".to_string());
                    r.line("fi".to_string());
                    r.line(format!("install -m 0600 {} {}", qp(src), qp(&cred)));
                    r.line(format!("export KATSU_CRED_{}={}", s.name, qp(&cred)));
                }
            }
        }
    }

    // ---- ephemeral ssh keypair + authorized_keys ----
    let id_key = runtime_root.join("id");
    let id_pub = runtime_root.join("id.pub");
    let authorized_keys = state_root.join("authorized_keys");
    r.blank()
        .comment("Ephemeral ssh keypair (private key stays in the runtime tmpfs; pubkey travels in the share).");
    r.line(format!(
        "[ -f {} ] || {ssh_keygen} -t ed25519 -N \"\" -f {} -q",
        qp(&id_key),
        qp(&id_key)
    ));
    r.line(format!("cp {} {}", qp(&id_pub), qp(&authorized_keys)));

    // ---- launch environment for the microvm runner (extraArgsScript reads these) ----
    r.blank()
        .comment("Per-instance launch environment for the microvm runner.");
    r.line(format!("export KATSU_STATE_DIR={}", qp(&state_root)));
    r.line(format!("export KATSU_SSH_PORT={}", plan.ssh_port));
    if let Some(cid) = plan.vsock_cid {
        r.line(format!("export KATSU_VSOCK_CID={cid}"));
    }

    // ---- graphics: announce, and (hardware rung only) stage the KATSU_GFX_* env ----
    // The resolution was decided once in `decide` (and recorded in instance.json).
    // Whenever graphics is on — either rung — announce it; a graphics-off instance
    // carries `None` and emits nothing here, byte-for-byte today's no-graphics
    // recipe. (`Unavailable` never reaches here — `decide` fails the launch.)
    if plan.gpu.is_some() {
        r.line("echo \"sandbox: graphics enabled\" >&2");
    }
    match &plan.gpu {
        // A usable hardware rung: the host-facing boundary warning + the node and
        // venus flag for extraArgsScript.
        Some(Resolution::Gpu { node, .. }) => {
            // Boundary warning — emitted ONLY on a hardware rung, because
            // virglrenderer parses the guest's GPU command stream inside the host
            // QEMU process exactly when one resolves, which widens the host-facing
            // attack surface. The software rung (below) keeps the full original
            // isolation (in-guest llvmpipe, no GPU device, no virglrenderer host
            // attack surface), so the warning would be factually wrong there.
            r.line(
                "echo \"sandbox: WARNING! Hardware graphics capability widens the host-facing \
                attack surface, increasing the risk of guest escape.\" >&2",
            );
            r.line(format!("export KATSU_GFX_RENDERNODE={}", qp(node)));
            r.line("export KATSU_GFX_VENUS=1".to_string());
        }
        // The software rung is in-guest llvmpipe — graphics is on (announced
        // above), but no host render node, no GPU device, and no virglrenderer in
        // the loop, so no GPU env and no boundary warning (the host attack surface
        // is unchanged from graphics-off). Graphics-off (`None`) emits nothing.
        Some(Resolution::Software) | Some(Resolution::Unavailable) | None => {}
    }

    // ---- disk-image symlinks: back each volume from the persistent state dir ----
    r.blank()
        .comment("Back each guest disk image from the persistent state dir via a runtime symlink.");
    for img in &spec.disk_images {
        let target = state_root.join(img);
        let link = runtime_root.join(img);
        r.line(format!("ln -sfn {} {}", qp(&target), qp(&link)));
    }
    r.line(format!("cd {}", qp(&runtime_root)));

    // ---- mode-specific tail ----
    match plan.mode {
        Mode::Agent => agent_tail(
            &mut r,
            &runner,
            &console_log,
            &runtime_root,
            config,
            plan,
            spec,
        ),
        Mode::Interactive => interactive_tail(
            &mut r,
            &ssh,
            &runner,
            &console_log,
            &state_root,
            &runtime_root,
            &id_key,
            plan,
            &spec.agent_user,
        ),
    }

    Ok(r)
}

/// The agent tail: `setsid` a
/// lingering, detached VM, then — with `--prompt` — **tail-call** the `prompt`
/// subcommand so `start` reuses the one streaming/readiness implementation
/// rather than duplicating vsock logic; without a prompt, exit 0 and
/// let the wrapper return.
fn agent_tail(
    r: &mut Recipe,
    runner: &str,
    console_log: &Path,
    runtime_root: &Path,
    config: &Path,
    plan: &Plan,
    spec: &Spec,
) {
    let cid = plan.vsock_cid.expect("agent mode always allocates a CID");
    r.blank()
        .comment("Agent mode: detach a lingering VM (setsid) that outlives this script.");
    r.line(format!(
        "setsid {runner} > {} 2>&1 < /dev/null &",
        qp(console_log)
    ));
    r.line("vm=$!".to_string());
    r.line("disown \"$vm\" 2>/dev/null || true".to_string());
    r.line(format!(
        "echo \"sandbox: agent instance '{}' running (cid {cid}).\"",
        plan.name
    ));
    match &plan.prompt {
        Some(text) => {
            // The VM was just launched detached above; wait for qemu to bind its
            // QMP socket so the `prompt` tail-call's liveness check sees the
            // instance as RUNNING (not paused — which would trigger a spurious
            // resume). qemu's `server,nowait` monitor socket appears within a
            // second or two; prompt then does its own channel readiness-wait.
            let qmp_sock = runtime_root.join("katsuobushi.sock");
            r.comment("Wait for the VM's QMP monitor socket before delivering the first turn.");
            r.line(format!(
                "for _ in $(seq 1 120); do [ -S {} ] && break; sleep 0.5; done",
                qp(&qmp_sock)
            ));
            r.comment("Deliver the first turn by tail-calling the prompt subcommand (it bakes in the channel readiness wait).");
            // Absolute path from the spec (not a bare `katsuctl`): this line runs
            // in a child shell that need not have the controller on its PATH. A
            // store path has no shell-special characters, so it is emitted
            // unquoted — keeping the bare-name test fixture's snapshot stable.
            r.line(format!(
                "exec {} sandbox --config {} prompt \"{}\" {}",
                spec.tools.katsuctl.display(),
                qp(config),
                plan.name,
                sq(text)
            ));
        }
        None => {
            r.line(format!(
                "echo \"sandbox: prompt it with: sandbox:prompt {} \\\"<text>\\\"\"",
                plan.name
            ));
            r.line("exit 0".to_string());
        }
    }
}

/// The interactive tail: a cleanup trap that tears the VM down on any exit (and prunes the
/// state dir for an ephemeral instance), then wait-for-sshd, then a foreground
/// `ssh`. The `ssh` is **not** `exec`ed — control must return to the shell so the
/// EXIT trap fires and cleanup runs (faithful to the prior art, which lets the
/// runner script fall off its end into the trap).
#[allow(clippy::too_many_arguments)]
fn interactive_tail(
    r: &mut Recipe,
    ssh: &str,
    runner: &str,
    console_log: &Path,
    state_root: &Path,
    runtime_root: &Path,
    id_key: &Path,
    plan: &Plan,
    agent_user: &str,
) {
    r.blank()
        .comment("Tear the VM down on any exit; an ephemeral instance also prunes its state dir.");
    r.line("cleanup() {".to_string());
    r.line("  trap - EXIT".to_string());
    r.line("  trap \"\" INT TERM HUP".to_string());
    r.line("  if [ -n \"${vm:-}\" ] && kill -0 \"$vm\" 2>/dev/null; then".to_string());
    r.line("    kill \"$vm\" 2>/dev/null || true".to_string());
    r.line(
        "    for _ in 1 2 3 4 5; do kill -0 \"$vm\" 2>/dev/null || break; sleep 1; done"
            .to_string(),
    );
    r.line("    kill -9 \"$vm\" 2>/dev/null || true".to_string());
    r.line("    wait \"$vm\" 2>/dev/null || true".to_string());
    r.line("  fi".to_string());
    r.line(format!("  rm -rf {}", qp(runtime_root)));
    r.line(format!("  [ -d {} ] || return 0", qp(state_root)));
    if plan.named {
        // Named instances are persistent (restart with the full suffixed name).
        r.line(format!(
            "  echo \"sandbox: kept named instance '{}' at {}\"",
            plan.name,
            state_root.display()
        ));
    } else {
        r.line(format!("  rm -rf {}", qp(state_root)));
    }
    r.line("}".to_string());
    r.line("trap cleanup EXIT".to_string());
    r.line("trap 'exit 143' TERM".to_string());
    r.line("trap 'exit 130' INT".to_string());
    r.line("trap 'exit 129' HUP".to_string());

    r.blank().line(format!(
        "echo \"sandbox: launching interactive instance '{}' (logs: {})\"",
        plan.name,
        console_log.display()
    ));
    r.line(format!("{runner} > {} 2>&1 &", qp(console_log)));
    r.line("vm=$!".to_string());
    r.line(format!(
        "echo \"sandbox: connecting to '{}' on 127.0.0.1:{}\"",
        plan.name, plan.ssh_port
    ));
    // Wait for sshd to accept on the forwarded port.
    r.line(format!(
        "for _ in $(seq 1 120); do (exec 3<>\"/dev/tcp/127.0.0.1/{}\") 2>/dev/null && break; sleep 1; done",
        plan.ssh_port
    ));
    r.line(format!(
        "{ssh} -i {} -p {} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR {agent_user}@127.0.0.1 || true",
        qp(id_key),
        plan.ssh_port
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host::{Call, FakeHost};
    use crate::sandbox::spec::{Roots, SecretSpec, Tools};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    /// A scripted [`Rng`] yielding a fixed sequence, repeating the last value —
    /// the shape every sibling test module uses.
    struct FakeRng {
        values: Vec<u32>,
        next: usize,
    }

    impl FakeRng {
        fn new(values: &[u32]) -> Self {
            Self {
                values: values.to_vec(),
                next: 0,
            }
        }
    }

    impl Rng for FakeRng {
        fn next_u32(&mut self) -> u32 {
            let value = self.values[self.next.min(self.values.len() - 1)];
            self.next += 1;
            value
        }
    }

    /// An `Output` that exited 0 carrying `stdout`.
    fn ok_out(stdout: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    /// Token-free resolved roots so the recipe paths are deterministic literals.
    fn roots() -> ResolvedRoots {
        ResolvedRoots {
            state_glob: PathBuf::from("/state/katsuobushi/cdata/katsuobushi"),
            runtime_glob: PathBuf::from("/run/katsuobushi/cdata/katsuobushi"),
        }
    }

    /// A base spec; callers tweak secrets/context/import-db/disk-images per test.
    fn spec_with(
        secrets: Vec<SecretSpec>,
        context: Vec<String>,
        import_host_store_db: bool,
    ) -> Spec {
        Spec {
            spec_version: 2,
            project_id: "cdata/katsuobushi".into(),
            agent_user: "agent".into(),
            import_host_store_db,
            roots: Roots {
                state_glob: PathBuf::from("$XDG_STATE_HOME/katsuobushi/cdata/katsuobushi"),
                runtime_glob: PathBuf::from("$XDG_RUNTIME_DIR/katsuobushi/cdata/katsuobushi"),
            },
            tools: Tools {
                git: PathBuf::from("/nix/store/git/bin/git"),
                ssh: PathBuf::from("/nix/store/openssh/bin/ssh"),
                ssh_keygen: PathBuf::from("/nix/store/openssh/bin/ssh-keygen"),
                tmux: PathBuf::from("/nix/store/tmux/bin/tmux"),
                rsync: PathBuf::from("/nix/store/rsync/bin/rsync"),
                sqlite3: if import_host_store_db {
                    Some(PathBuf::from("/nix/store/sqlite/bin/sqlite3"))
                } else {
                    None
                },
                bash: PathBuf::from("/nix/store/bash/bin/bash"),
                // Bare name (not a store path) so the agent-tail snapshot stays
                // byte-stable: the emitted recipe renders `exec katsuctl … prompt`.
                katsuctl: PathBuf::from("katsuctl"),
            },
            runner: PathBuf::from("/nix/store/microvm/bin/microvm-run"),
            disk_images: vec![
                "rw-store.img".into(),
                "nix-db.img".into(),
                "scratch.img".into(),
            ],
            context,
            secrets,
            vsock_port: 1024,
            host_cid: 2,
            heartbeat_secs: 10,
            heartbeat_miss: 3,
            progress_stall_secs: 300,
            delivery_deadline_secs: 20,
            delivery_retries: 3,
            ready_gate_secs: 60,
            stop_grace_ms: 1500,
            graphics: crate::sandbox::spec::GraphicsSpec::default(),
        }
    }

    fn env_secret() -> SecretSpec {
        SecretSpec {
            name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
            source: SecretSource::FromEnv("HARNESS_OAUTH_TOKEN".into()),
            dest: "cred-CLAUDE_CODE_OAUTH_TOKEN".into(),
        }
    }

    fn file_secret() -> SecretSpec {
        SecretSpec {
            name: "EXTRA_TOKEN".into(),
            source: SecretSource::FromFile("/run/host-secrets/extra".into()),
            dest: "cred-EXTRA_TOKEN".into(),
        }
    }

    /// A canned plan for the snapshots; callers override fields.
    fn plan(name: &str, named: bool, mode: Mode) -> Plan {
        Plan {
            name: name.to_string(),
            named,
            mode,
            ssh_port: 22042,
            vsock_cid: matches!(mode, Mode::Agent).then_some(4242),
            project: PathBuf::from("/home/user/project"),
            clone_mirror: true,
            seed: Seed::Fresh("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0".into()),
            prompt: None,
            gpu: None,
        }
    }

    const CONFIG: &str = "/nix/store/katsuctl-sandbox-spec.json";

    fn render(spec: &Spec, plan: &Plan) -> String {
        build_recipe(spec, Path::new(CONFIG), &roots(), plan)
            .expect("recipe should build")
            .render()
    }

    /// A spec with graphics opted in over the given GPU role ladder.
    fn spec_with_graphics(gpu: Vec<crate::sandbox::spec::GpuRole>) -> Spec {
        let mut spec = spec_with(vec![], vec![], false);
        spec.graphics = crate::sandbox::spec::GraphicsSpec {
            enable: true,
            gpu,
            output: None,
        };
        spec
    }

    // ---- naming (pure unit tests, tier 1) ----

    #[test]
    fn it_mints_an_ephemeral_name_from_clock_and_pid() {
        let mut rng = FakeRng::new(&[0xdead_beef]);
        let (name, named) = decide_name(None, "20260627-120000", 4242, &mut rng).unwrap();
        assert_eq!(name, "20260627-120000-4242");
        assert!(!named, "an unnamed instance is ephemeral");
    }

    #[test]
    fn it_appends_an_8hex_suffix_to_a_friendly_name() {
        let mut rng = FakeRng::new(&[0x0badf00d]);
        let (name, named) = decide_name(Some("myfeature"), "20260627-120000", 1, &mut rng).unwrap();
        assert_eq!(name, "myfeature-0badf00d");
        assert!(named, "a --name instance is persistent");
    }

    #[test]
    fn it_resumes_an_already_suffixed_name_verbatim() {
        // A full name copied back from a prior launch is taken as-is (resume).
        let mut rng = FakeRng::new(&[0x1111_2222]);
        let (name, named) =
            decide_name(Some("myfeature-0badf00d"), "20260627-120000", 1, &mut rng).unwrap();
        assert_eq!(name, "myfeature-0badf00d", "no re-suffixing on resume");
        assert!(named);
    }

    #[test]
    fn it_rejects_a_name_with_a_shell_metacharacter() {
        // `--name` is operator input baked as literal script text, so any shell
        // metacharacter must be rejected before a recipe could be built — the old
        // command-injection surface (`--name 'x";id;"'`).
        for hostile in [
            "a\";id",
            "a$b",
            "a b",
            "a`id`",
            "a\\b",
            "x\";id;\"",
            "",
            "a/b",
        ] {
            let mut rng = FakeRng::new(&[0xdead_beef]);
            let err = decide_name(Some(hostile), "20260627-120000", 1, &mut rng)
                .expect_err("a metacharacter name must be rejected");
            assert!(
                format!("{err:#}").contains("invalid instance name"),
                "rejected {hostile:?}: {err:#}"
            );
        }
    }

    #[test]
    fn it_accepts_normal_and_resume_names() {
        // The safe charset still admits ordinary friendly names and a verbatim
        // hex8-suffixed resume name (dots/underscores/dashes allowed).
        let mut rng = FakeRng::new(&[0x0badf00d]);
        assert!(decide_name(Some("my.feature_v2"), "20260627-120000", 1, &mut rng).is_ok());
        let mut rng = FakeRng::new(&[0x1111_2222]);
        let (name, _) =
            decide_name(Some("my-feature-0badf00d"), "20260627-120000", 1, &mut rng).unwrap();
        assert_eq!(name, "my-feature-0badf00d");
    }

    #[test]
    fn it_bails_before_any_io_on_a_hostile_name() {
        // A hostile `--name` must short-circuit in `decide` *before* any host
        // interaction, so no recipe and no instance.json can be produced.
        let spec = spec_with(vec![], vec![], false);
        let host = FakeHost::new();
        let mut rng = FakeRng::new(&[1]);
        let err = decide(
            &host,
            &mut rng,
            &roots(),
            &spec,
            true,
            Some("evil\";id"),
            None,
            "20260627-120000",
            7,
        )
        .expect_err("a hostile name must abort planning");
        assert!(
            format!("{err:#}").contains("invalid instance name"),
            "{err:#}"
        );
        assert!(
            host.calls().is_empty(),
            "nothing must touch the world before the name is validated: {:?}",
            host.calls()
        );
    }

    #[test]
    fn it_only_treats_a_lowercase_8hex_tail_as_a_suffix() {
        assert!(has_hex8_suffix("x-0badf00d"));
        assert!(
            !has_hex8_suffix("x-0BADF00D"),
            "uppercase is not our suffix"
        );
        assert!(!has_hex8_suffix("x-0badf0d"), "7 hex is not a suffix");
        assert!(!has_hex8_suffix("x-0badf00dd"), "9 hex is not a suffix");
        assert!(!has_hex8_suffix("0badf00d"), "needs the leading dash");
        assert!(!has_hex8_suffix("x-deadbefg"), "g is not hex");
    }

    // ---- seam: port allocation (tier 3) ----

    #[test]
    fn it_bakes_the_probed_free_port_into_the_plan() {
        let spec = spec_with(vec![], vec![], false);
        let mut host = FakeHost::new();
        // rng 42 -> port 20042 (free); project + a fresh HEAD seed.
        host.with_free_port(20_042)
            .push_run(Ok(ok_out("/home/user/project\n"))) // rev-parse --show-toplevel
            .push_run(Ok(ok_out(""))) // stash create -> clean tree
            .push_run(Ok(ok_out("cafebabe\n"))); // rev-parse HEAD
        let mut rng = FakeRng::new(&[42]);

        let plan = decide(
            &host,
            &mut rng,
            &roots(),
            &spec,
            false,
            None,
            None,
            "20260627-120000",
            7,
        )
        .expect("planning should succeed");

        assert_eq!(plan.ssh_port, 20_042);
        assert!(host.calls().contains(&Call::PortIsFree(20_042)));
    }

    // ---- seam: CID allocation skips siblings' CIDs (tier 3) ----

    #[test]
    fn it_allocates_a_cid_skipping_used_sibling_cids() {
        let spec = spec_with(vec![], vec![], false);
        let mut host = FakeHost::new();
        // One sibling already on CID 13; the agent must not collide with it.
        host.with_free_port(20_001)
            .push_list_dir(Ok(vec!["sibling-aaaaaaaa".into()]))
            .push_read(Ok(br#"{"vsockCid": 13}"#.to_vec()))
            .push_run(Ok(ok_out("/home/user/project\n")))
            .push_run(Ok(ok_out("")))
            .push_run(Ok(ok_out("cafebabe\n")));
        // port rng 1 -> 20001; cid rng 10 -> 13 (collides, retry); 20 -> 23.
        let mut rng = FakeRng::new(&[1, 10, 20]);

        let plan = decide(
            &host,
            &mut rng,
            &roots(),
            &spec,
            true,
            None,
            None,
            "20260627-120000",
            7,
        )
        .expect("agent planning should succeed");

        assert_eq!(plan.mode, Mode::Agent);
        assert_eq!(plan.vsock_cid, Some(23), "skipped the sibling's CID 13");
    }

    #[test]
    fn it_reuses_a_resumed_instances_recorded_cid() {
        // A verbatim-resumed named agent keeps the CID recorded in its own
        // instance.json rather than re-allocating.
        let host_state = PathBuf::from("/state/katsuobushi/cdata/katsuobushi");
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(vec!["myfeature-0badf00d".into()]))
            .push_read(Ok(br#"{"vsockCid": 777}"#.to_vec()));
        let claims = gather_sibling_claims(&host, &host_state, "myfeature-0badf00d");
        assert!(
            claims.used_cids.is_empty(),
            "the current instance is not a sibling"
        );
        assert_eq!(claims.own_cid, Some(777));
    }

    #[test]
    fn it_skips_a_port_recorded_by_a_sibling_instance() {
        // A sibling's recorded sshPort is not bound until its qemu boots, so
        // the bind probe alone cannot see it — the recorded claim must be
        // enough to force a re-draw.
        let spec = spec_with(vec![], vec![], false);
        let mut host = FakeHost::new();
        host.with_free_port(20_005) // free per the bind probe, but claimed
            .with_free_port(20_010)
            .push_list_dir(Ok(vec!["sibling-aaaaaaaa".into()]))
            .push_read(Ok(br#"{"vsockCid": 13, "sshPort": 20005}"#.to_vec()))
            .push_run(Ok(ok_out("/home/user/project\n")))
            .push_run(Ok(ok_out("")))
            .push_run(Ok(ok_out("cafebabe\n")));
        // port rng 5 -> 20005 (claimed by the sibling; re-draw), 10 -> 20010.
        let mut rng = FakeRng::new(&[5, 10]);

        let plan = decide(
            &host,
            &mut rng,
            &roots(),
            &spec,
            false,
            None,
            None,
            "20260627-120000",
            7,
        )
        .expect("planning should succeed");

        assert_eq!(plan.ssh_port, 20_010, "skipped the sibling's claimed port");
    }

    // ---- seam: seed resolution (tier 3) ----

    #[test]
    fn it_seeds_fresh_from_stash_create_when_dirty() {
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out("stashcommit123\n"))); // stash create produced one
        let seed = resolve_seed(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            Path::new("/state/sync.git"),
            "refs/heads/sandbox/x",
            false,
            false,
        )
        .expect("seed");
        assert_eq!(seed, Seed::Fresh("stashcommit123".into()));
    }

    #[test]
    fn it_falls_back_to_head_when_stash_create_is_empty() {
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out(""))) // clean tree -> stash create prints nothing
            .push_run(Ok(ok_out("headcommit456\n")));
        let seed = resolve_seed(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            Path::new("/state/sync.git"),
            "refs/heads/sandbox/x",
            false,
            false,
        )
        .expect("seed");
        assert_eq!(seed, Seed::Fresh("headcommit456".into()));
    }

    #[test]
    fn it_resumes_a_named_branch_that_already_exists() {
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out("existingbranch789\n"))); // rev-parse --verify
        let seed = resolve_seed(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            Path::new("/state/sync.git"),
            "refs/heads/sandbox/myfeature-0badf00d",
            true, // named
            true, // mirror exists
        )
        .expect("seed");
        assert_eq!(seed, Seed::Resume("existingbranch789".into()));
    }

    #[test]
    fn it_seeds_fresh_when_named_but_branch_is_missing() {
        // Mirror exists but has no such branch -> the verify yields nothing, so we
        // fall through to a fresh seed (and the recipe will push it).
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out(""))) // rev-parse --verify: branch absent
            .push_run(Ok(ok_out("snap\n"))); // stash create
        let seed = resolve_seed(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            Path::new("/state/sync.git"),
            "refs/heads/sandbox/myfeature-0badf00d",
            true,
            true,
        )
        .expect("seed");
        assert_eq!(seed, Seed::Fresh("snap".into()));
    }

    // ---- secrets stay references, never values ----

    #[test]
    fn it_never_bakes_a_plaintext_secret_value() {
        // Even with the env value present in this process, katsuctl never reads it,
        // so it cannot reach the recipe — only the env-var NAME is referenced.
        const SENTINEL: &str = "SUPER-SECRET-OAUTH-VALUE-9f8e7d6c";
        std::env::set_var("HARNESS_OAUTH_TOKEN", SENTINEL);

        let spec = spec_with(vec![env_secret(), file_secret()], vec![], false);
        let text = render(&spec, &plan("20260627-120000-4242", false, Mode::Agent));

        std::env::remove_var("HARNESS_OAUTH_TOKEN");

        assert!(
            !text.contains(SENTINEL),
            "the plaintext secret value must never appear in the recipe:\n{text}"
        );
        // The reference (env-var name) and the file source path may appear.
        assert!(
            text.contains("HARNESS_OAUTH_TOKEN"),
            "the env-var name is the reference"
        );
        assert!(
            text.contains("/run/host-secrets/extra"),
            "the file source path is the reference"
        );
        assert!(!text.contains('\u{1b}'), "emitted scripts carry zero ANSI");
    }

    // ---- golden snapshots across the matrix (tier 2) ----

    #[test]
    fn snapshot_ephemeral_interactive() {
        let spec = spec_with(vec![], vec![], false);
        insta::assert_snapshot!(render(
            &spec,
            &plan("20260627-120000-4242", false, Mode::Interactive)
        ));
    }

    #[test]
    fn snapshot_named_interactive() {
        let spec = spec_with(vec![], vec![], false);
        let mut p = plan("myfeature-0badf00d", true, Mode::Interactive);
        p.clone_mirror = false;
        p.seed = Seed::Resume("existingbranch789".into());
        insta::assert_snapshot!(render(&spec, &p));
    }

    #[test]
    fn snapshot_ephemeral_agent_no_prompt() {
        let spec = spec_with(vec![env_secret()], vec![], false);
        insta::assert_snapshot!(render(
            &spec,
            &plan("20260627-120000-4242", false, Mode::Agent)
        ));
    }

    #[test]
    fn snapshot_ephemeral_agent_with_prompt() {
        let spec = spec_with(vec![env_secret()], vec![], false);
        let mut p = plan("20260627-120000-4242", false, Mode::Agent);
        p.prompt = Some("fix the bug in foo's \"bar\" path".into());
        insta::assert_snapshot!(render(&spec, &p));
    }

    #[test]
    fn snapshot_named_agent_with_prompt() {
        let spec = spec_with(vec![env_secret()], vec![], false);
        let mut p = plan("myfeature-0badf00d", true, Mode::Agent);
        p.clone_mirror = false;
        p.seed = Seed::Resume("existingbranch789".into());
        p.prompt = Some("continue the work".into());
        insta::assert_snapshot!(render(&spec, &p));
    }

    #[test]
    fn snapshot_agent_with_import_host_store_db_and_context() {
        // Covers ±importHostStoreDb and context staging + a fromFile secret.
        let spec = spec_with(
            vec![file_secret()],
            vec!["dist/build.tar".into(), "data/seed.json".into()],
            true,
        );
        insta::assert_snapshot!(render(
            &spec,
            &plan("20260627-120000-4242", false, Mode::Agent)
        ));
    }

    // ---- graphics: GPU resolution + the launch-time boundary notice ----

    #[test]
    fn snapshot_agent_graphics_gpu_rung() {
        // A resolved hardware rung: the recipe exports KATSU_GFX_RENDERNODE +
        // KATSU_GFX_VENUS and prints the boundary notice.
        let spec = spec_with_graphics(vec![
            crate::sandbox::spec::GpuRole::Integrated,
            crate::sandbox::spec::GpuRole::Discrete,
            crate::sandbox::spec::GpuRole::Software,
        ]);
        let mut p = plan("20260627-120000-4242", false, Mode::Agent);
        p.gpu = Some(Resolution::Gpu {
            node: PathBuf::from("/dev/dri/renderD128"),
            role: crate::sandbox::spec::GpuRole::Integrated,
            venus: true,
        });
        insta::assert_snapshot!(render(&spec, &p));
    }

    #[test]
    fn snapshot_agent_graphics_software_fallback() {
        // The ladder resolved to its `software` tail: in-guest llvmpipe. Graphics
        // is still announced, but no GPU env is staged and no boundary warning
        // fires (the host attack surface is unchanged from graphics-off).
        let spec = spec_with_graphics(vec![
            crate::sandbox::spec::GpuRole::Integrated,
            crate::sandbox::spec::GpuRole::Discrete,
            crate::sandbox::spec::GpuRole::Software,
        ]);
        let mut p = plan("20260627-120000-4242", false, Mode::Agent);
        p.gpu = Some(Resolution::Software);
        let recipe = render(&spec, &p);
        // Graphics is announced on the software rung too…
        assert!(
            recipe.contains("echo \"sandbox: graphics enabled\""),
            "software rung still announces graphics: {recipe}"
        );
        // …but it carries no boundary warning and stages no GPU env.
        assert!(
            !recipe.contains("WARNING"),
            "software rung has no boundary warning: {recipe}"
        );
        assert!(
            !recipe.contains("KATSU_GFX_RENDERNODE"),
            "software rung stages no GPU env: {recipe}"
        );
        insta::assert_snapshot!(recipe);
    }

    #[test]
    fn it_errors_when_graphics_has_no_gpu_and_no_software_tail() {
        // A GPU-less host with a `software`-less ladder must abort the launch in
        // `decide` (fail loud, never silently boot slow) — before any recipe or
        // instance.json is produced.
        let spec = spec_with_graphics(vec![
            crate::sandbox::spec::GpuRole::Integrated,
            crate::sandbox::spec::GpuRole::Discrete,
        ]);
        let mut host = FakeHost::new();
        // Get planning through port + git (project + fresh-HEAD seed); inject no
        // render nodes, so the integrated/discrete ladder resolves to Unavailable.
        host.with_free_port(20_042)
            .push_run(Ok(ok_out("/home/user/project\n")))
            .push_run(Ok(ok_out("")))
            .push_run(Ok(ok_out("cafebabe\n")));
        let mut rng = FakeRng::new(&[42]);
        let err = decide(
            &host,
            &mut rng,
            &roots(),
            &spec,
            false,
            None,
            None,
            "20260627-120000",
            7,
        )
        .expect_err("no usable GPU and no software tail must fail planning");
        assert!(
            format!("{err:#}").contains("no usable GPU and no `software` fallback"),
            "{err:#}"
        );
    }

    // ---- end-to-end: the emitted recipe is exec-able under bash ----

    #[test]
    fn it_emits_a_syntactically_valid_script() {
        // `bash -n` parses (does not run) — guards the heredoc-free recipe shape
        // across every tail without booting anything.
        let spec = spec_with(vec![env_secret()], vec!["ctx/file".into()], true);
        for (name, named, mode, prompt) in [
            ("e-int", false, Mode::Interactive, None),
            ("e-agt", false, Mode::Agent, None),
            ("e-agt-p", false, Mode::Agent, Some("hi")),
            ("named-0badf00d", true, Mode::Interactive, None),
        ] {
            let mut p = plan(name, named, mode);
            p.prompt = prompt.map(str::to_string);
            let text = render(&spec, &p);
            let dir = std::env::temp_dir().join(format!(
                "katsuctl-start-it-{}-{}",
                std::process::id(),
                name
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("start.sh");
            std::fs::write(&path, &text).unwrap();
            let status = Command::new("bash")
                .arg("-n")
                .arg(&path)
                .status()
                .expect("bash -n");
            assert!(status.success(), "recipe must parse under bash:\n{text}");
            let _ = std::fs::remove_dir_all(&dir);
        }

        // And the graphics-on tail (the boundary notice + the KATSU_GFX_* exports)
        // parses too — the em-dash in the notice lives inside a quoted echo.
        let gfx_spec = spec_with_graphics(vec![
            crate::sandbox::spec::GpuRole::Integrated,
            crate::sandbox::spec::GpuRole::Software,
        ]);
        let mut gfx_plan = plan("e-gfx", false, Mode::Agent);
        gfx_plan.gpu = Some(Resolution::Gpu {
            node: PathBuf::from("/dev/dri/renderD128"),
            role: crate::sandbox::spec::GpuRole::Integrated,
            venus: true,
        });
        let text = render(&gfx_spec, &gfx_plan);
        let dir =
            std::env::temp_dir().join(format!("katsuctl-start-it-{}-gfx", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("start.sh");
        std::fs::write(&path, &text).unwrap();
        let status = Command::new("bash")
            .arg("-n")
            .arg(&path)
            .status()
            .expect("bash -n");
        assert!(
            status.success(),
            "graphics recipe must parse under bash:\n{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
