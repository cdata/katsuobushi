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

use crate::sandbox::directive;
use crate::sandbox::emit::{self, Recipe};
use crate::sandbox::gfx::{self, Resolution};
use crate::sandbox::host::{pick_cid, pick_port, run_ok, Host, HostImpl, OsRng, Rng};
use crate::sandbox::instance::{self, Instance, Mode, SUPPORTED_INSTANCE_VERSION};
use crate::sandbox::spec::{load_spec, resolve_roots, ResolvedRoots, SecretSource, Spec};
use crate::Global;

/// Wall-clock bound on the pre-boot host Nix DB snapshot.
///
/// The snapshot is best-effort by design — a guest with no snapshot boots with a
/// system-only store DB — so the only thing that must never happen is the step
/// running unbounded. On a quiescent DB `VACUUM INTO` finishes in well under a
/// second for a ~72 MB database; two minutes leaves room for a very large host
/// store on slow storage while still failing fast enough that an operator does
/// not conclude the launch has hung and kill it (which is what the unbounded
/// `.backup` used to provoke).
const NIX_DB_SNAPSHOT_TIMEOUT_SECS: u32 = 120;

/// How the instance branch is seeded.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seed {
    /// Resume a named instance from its existing branch commit — **no** push is
    /// emitted (the accumulated work is continued as-is).
    Resume(String),
    /// Seed a fresh branch from this commit — the recipe pushes it.
    Fresh(String),
}

impl Seed {
    /// The seed commit SHA, regardless of variant. Persisted into `instance.json`
    /// so `sandbox fetch` can tell whether the branch advanced past it.
    fn commit(&self) -> &str {
        match self {
            Seed::Resume(c) | Seed::Fresh(c) => c,
        }
    }
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
    /// Pass `--until-report` to the `prompt` tail-call so the first turn stays
    /// armed across an unreported turn-end. Only meaningful with `prompt`.
    until_report: bool,
    /// The resolved GPU ladder verdict when graphics is enabled, else `None`.
    /// Decided here (the one graphics probe) so both the recipe and the persisted
    /// `instance.json` read the *same* resolution; never holds `Unavailable` (that
    /// fails the launch loud in [`decide`] before a `Plan` is built).
    gpu: Option<Resolution>,
    /// The commit to push as `refs/heads/base` in the mirror on a resume, or
    /// `None` when the tip has not moved (base already records this SHA) or this
    /// is a fresh launch. The guest rebases its branch onto this ref so it works
    /// against the current project state after waking from a pause.
    base_commit: Option<String>,
}

impl Plan {
    /// The GPU rung to record in `instance.json` (and surface in `sandbox status`):
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
///
/// `board_exclude` is the project-relative path to the board directory (e.g.
/// `project/kanban`).  When `Some`, a stash-based seed has any board changes
/// filtered out before the seed is pushed to the mirror — the board is
/// orchestrator-only and must never travel in the guest branch.  `None` for a
/// direct `sandbox start` that is not a dispatch.
pub fn run(
    config: &Path,
    agent: bool,
    name: Option<String>,
    prompt: Option<String>,
    until_report: bool,
    global: Global,
    board_exclude: Option<&Path>,
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
        until_report,
        &clock,
        pid,
        board_exclude,
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
        seed: Some(plan.seed.commit().to_string()),
    };
    instance::write(&roots.state_glob, &meta).context("writing instance.json")?;

    // Persist the directive next to it. Until now the composed text lived only
    // in this process's argv and the transient recipe, so killing the launch
    // (or losing its terminal) left a healthy, idle VM, a card marked
    // in-progress, and the directive nowhere on disk — recoverable only by
    // hand-recomposing it from the card. Writing it here makes
    // `sandbox prompt --redeliver` a mechanical recovery.
    //
    // No new secret exposure: a directive is a card body plus the project's
    // instructions prelude, both already plaintext in the board. Secrets ride
    // fw_cfg and never appear here.
    if let Some(text) = plan.prompt.as_deref() {
        directive::write(&roots.state_glob, &plan.name, text)
            .context("persisting the launch directive")?;
    }

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
    until_report: bool,
    clock: &str,
    pid: u32,
    board_exclude: Option<&Path>,
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
        board_exclude,
    )?;

    // On a named resume, refresh the mirror's base ref when the project tip has
    // advanced so the guest can rebase its branch onto the current state.
    let base_commit = resolve_base_refresh(host, &spec.tools.git, &project, &sync_git, &seed)?;

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
        until_report,
        gpu,
        base_commit,
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
/// When `board_exclude` is `Some` and the stash snapshot contains changes to
/// that path, those changes are filtered out before the seed is returned (see
/// [`strip_board_from_stash`]).  The board is orchestrator-only; board state in
/// the seed travels through the guest branch and would merge back silently at
/// landing time.
///
/// All git calls go through the seam so the branch is decided without touching a
/// real repo.
#[allow(clippy::too_many_arguments)]
fn resolve_seed(
    host: &impl Host,
    git: &Path,
    project: &Path,
    sync_git: &Path,
    branch: &str,
    named: bool,
    mirror_exists: bool,
    board_exclude: Option<&Path>,
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
    } else if let Some(exclude) = board_exclude {
        strip_board_from_stash(host, git, project, branch, &snap, exclude)?
    } else {
        snap
    };
    if commit.is_empty() {
        bail!("could not resolve a seed commit (neither `stash create` nor HEAD produced one)");
    }
    Ok(Seed::Fresh(commit))
}

/// When a dispatch-time stash snapshot (`stash_sha`) contains changes to
/// `board_exclude`, build and return a new commit whose tree has those paths
/// restored to their `HEAD` state.  Returns `stash_sha` unchanged when no
/// board changes are present in the stash.
///
/// The board is orchestrator-only (`project/kanban/` by convention).  Any
/// board state that travels in a stash seed ends up in the guest branch tree
/// and merges back silently when the orchestrator runs `git merge --squash` at
/// landing time, silently corrupting the board lane.  Filtering here is the
/// earliest place to sever that path.
///
/// Implementation uses a temporary git index at
/// `<project>/.git/katsuctl-seed-<branch>.idx` (via `GIT_INDEX_FILE`) so the
/// host's real index is never disturbed.  The file is not cleaned up — it is
/// overwritten on the next dispatch of the same branch and is harmless once the
/// instance is pruned.
fn strip_board_from_stash(
    host: &impl Host,
    git: &Path,
    project: &Path,
    branch: &str,
    stash_sha: &str,
    board_exclude: &Path,
) -> Result<String> {
    // Normalise to a project-relative path for the git commands that run under
    // `-C <project>`.  An absolute board_exclude (e.g. when the user passes
    // `--board-dir /abs/path`) is stripped of the project prefix; a relative
    // one (e.g. the default `project/kanban`) is used as-is.
    let board_rel: &Path = if board_exclude.is_absolute() {
        board_exclude.strip_prefix(project).unwrap_or(board_exclude)
    } else {
        board_exclude
    };

    // Quick check: does the stash actually differ from HEAD in the board dir?
    // If not, skip the filtering work entirely.
    let mut diff_check = Command::new(git);
    diff_check
        .arg("-C")
        .arg(project)
        .arg("diff")
        .arg("--name-only")
        .arg("HEAD")
        .arg(stash_sha)
        .arg("--")
        .arg(board_rel);
    let board_changed = host.run(&diff_check).ok().is_some_and(|o| {
        o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty()
    });

    if !board_changed {
        return Ok(stash_sha.to_string());
    }

    eprintln!(
        "sandbox: dispatch seed has {board_rel:?} changes — stripping board from seed commit"
    );

    // Use a branch-unique temp-index path so concurrent dispatches of
    // different cards do not race on the same file.
    let safe = branch.trim_start_matches("refs/heads/").replace('/', "-");
    let tmp_idx = project
        .join(".git")
        .join(format!("katsuctl-seed-{safe}.idx"));

    // Step 1: load the stash's working-tree state into the temp index.
    let mut read_stash = Command::new(git);
    read_stash
        .arg("-C")
        .arg(project)
        .arg("read-tree")
        .arg(stash_sha)
        .env("GIT_INDEX_FILE", &tmp_idx);
    run_ok(host, &read_stash, "loading stash tree into temp index")?;

    // Step 2: remove all board entries from the temp index.
    let mut rm_board = Command::new(git);
    rm_board
        .arg("-C")
        .arg(project)
        .arg("rm")
        .arg("--cached")
        .arg("-r")
        .arg("--quiet")
        .arg("--ignore-unmatch")
        .arg("--")
        .arg(board_rel)
        .env("GIT_INDEX_FILE", &tmp_idx);
    host.run(&rm_board).ok(); // best effort: --ignore-unmatch handles a missing prefix

    // Step 3: restore the board tree from HEAD so it is not absent from the seed.
    let head_board_ref = format!("HEAD:{}", board_rel.display());
    let mut verify_head = Command::new(git);
    verify_head
        .arg("-C")
        .arg(project)
        .arg("rev-parse")
        .arg("--verify")
        .arg(&head_board_ref);
    if host
        .run(&verify_head)
        .ok()
        .is_some_and(|o| o.status.success())
    {
        let prefix = format!("{}/", board_rel.display());
        let mut restore_head = Command::new(git);
        restore_head
            .arg("-C")
            .arg(project)
            .arg("read-tree")
            .arg(format!("--prefix={prefix}"))
            .arg(&head_board_ref)
            .env("GIT_INDEX_FILE", &tmp_idx);
        host.run(&restore_head).ok(); // best effort
    }

    // Step 4: write the filtered tree and create the seed commit.
    let mut write_tree = Command::new(git);
    write_tree
        .arg("-C")
        .arg(project)
        .arg("write-tree")
        .env("GIT_INDEX_FILE", &tmp_idx);
    let tree_out = run_ok(host, &write_tree, "writing filtered dispatch seed tree")?;
    let tree_sha = String::from_utf8_lossy(&tree_out.stdout).trim().to_string();
    if tree_sha.is_empty() {
        bail!("git write-tree returned no SHA for the filtered dispatch seed");
    }

    let mut commit_tree = Command::new(git);
    commit_tree
        .arg("-C")
        .arg(project)
        .arg("commit-tree")
        .arg(&tree_sha)
        .arg("-p")
        .arg("HEAD")
        .arg("-m")
        .arg("dispatch seed");
    let commit_out = run_ok(host, &commit_tree, "creating filtered dispatch seed commit")?;
    let new_commit = String::from_utf8_lossy(&commit_out.stdout)
        .trim()
        .to_string();
    if new_commit.is_empty() {
        bail!("git commit-tree returned no SHA for the filtered dispatch seed");
    }

    Ok(new_commit)
}

/// Resolve the base commit to push into the mirror on a resume.
///
/// Returns `Some(sha)` when the project `HEAD` differs from the current
/// `refs/heads/base` in the mirror (or when no base ref exists yet). Returns
/// `None` when the tip has not moved — base already records this exact SHA —
/// so a resume where nothing landed is a genuine no-op for the guest.
///
/// Always returns `None` for a fresh ([`Seed::Fresh`]) instance.
///
/// ## Distinguished from card c1a6e1
///
/// Card c1a6e1 documents the orphaning fault: the orchestrator rewrote commits
/// already in the mirror's `sandbox/<instance>` branch, causing the guest's
/// branch to be orphaned. This function is categorically different: it never
/// touches `sandbox/<instance>`. It only adds or advances `refs/heads/base`, a
/// separate ref the guest uses as a rebase target. The orchestrator never
/// rewrites any commit that is already in the mirror.
fn resolve_base_refresh(
    host: &impl Host,
    git: &Path,
    project: &Path,
    sync_git: &Path,
    seed: &Seed,
) -> Result<Option<String>> {
    // Only named resumes need a base refresh.
    if !matches!(seed, Seed::Resume(_)) {
        return Ok(None);
    }

    // Read the current project HEAD — this becomes the new base.
    let mut head_cmd = Command::new(git);
    head_cmd.arg("-C").arg(project).arg("rev-parse").arg("HEAD");
    let out = host
        .run(&head_cmd)
        .context("running `git rev-parse HEAD` for base refresh")?;
    if !out.status.success() {
        bail!("`git rev-parse HEAD` failed while resolving base for resume");
    }
    let current_head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if current_head.is_empty() {
        bail!("could not resolve HEAD for base refresh");
    }

    // Check the mirror's existing base ref. An absent or unreadable ref is
    // treated as "not present" — the first resume always pushes.
    let mut base_cmd = Command::new(git);
    base_cmd
        .arg("-C")
        .arg(sync_git)
        .arg("rev-parse")
        .arg("--verify")
        .arg("refs/heads/base");
    let existing_base = match host.run(&base_cmd) {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    };

    if !existing_base.is_empty() && existing_base == current_head {
        return Ok(None); // tip unchanged — nothing to push
    }

    Ok(Some(current_head))
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
    let phase_file = state_root.join("phase");
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

    // ---- provisioning visibility (log + phase marker) ----
    //
    // Everything from here until the runner starts is pre-boot: there is no QMP
    // socket yet, so `sandbox status` cannot see the instance, and `console.log`
    // does not exist yet because only the runner writes it. That window used to
    // be structurally silent — an operator watching an 11-minute provisioning
    // stall had `/proc` spelunking as their only diagnostic, and killed a live
    // step because of it. Two cheap signals close it:
    //
    //   * `provision.log` — a tee of this script's own output, so every step's
    //     begin/end marker is on disk the moment it happens.
    //   * `phase` — the current step, one line, which `sandbox status` renders
    //     as `provisioning (<step>)` instead of the untruthful `stopped`.
    //
    // stdout/stderr are *saved first* and restored before the handoff below:
    // interactive mode hands the terminal to ssh+tmux, which needs a real TTY
    // and would break behind a pipe.
    r.blank()
        .comment("Log provisioning from here (the runner's own console.log only starts at boot).");
    // Both streams are logged, but they stay *separate*: stdout tees back to
    // the saved stdout and stderr to the saved stderr. Collapsing them with
    // `2>&1` would have sent the recipe's own failures (a missing secret exits
    // via stderr) to the caller's stdout, quietly breaking the clean stream
    // split `emit` documents.
    r.line("exec 3>&1 4>&2".to_string());
    r.line(format!(
        "exec > >(tee -a {} >&3) 2> >(tee -a {} >&4)",
        qp(&state_root.join("provision.log")),
        qp(&state_root.join("provision.log"))
    ));
    r.line(format!(
        "phase() {{ printf '%s\\n' \"$1\" > {}; _t0=$SECONDS; printf '::: %s\\n' \"$1\"; }}",
        qp(&phase_file)
    ));
    r.line("phase_done() { printf '::: done (%ss)\\n' \"$((SECONDS-_t0))\"; }".to_string());
    // Clear the marker if provisioning *aborts* — a missing secret exits 1, and
    // `set -e` or Ctrl-C can end the script anywhere in here. Without this the
    // marker outlives the dead launch and `sandbox status` reports
    // `provisioning` forever, which is strictly worse than the `stopped` it used
    // to report and would strand an orchestrator that (per MIGRATING) now treats
    // `provisioning` as a live launch worth waiting for.
    //
    // Safe on every success path too: agent mode `exec`s away (traps do not run
    // on exec) and has already removed the marker, and interactive mode installs
    // its own EXIT trap later, which supersedes this one and also removes it.
    r.line(format!("trap 'rm -f {}' EXIT INT TERM", qp(&phase_file)));

    // ---- bare mirror (idempotent) + branch seed + push-perm chmod ----
    r.blank().comment(
        "Per-instance bare git mirror + seeded branch (the guest clones it and pushes back).",
    );
    r.line("phase 'staging project mirror'".to_string());
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
    // On a resume where the project tip has advanced, push the current HEAD as
    // refs/heads/base in the mirror so the guest can rebase its branch onto it.
    // Skipped when the tip has not moved (base_commit is None) — the mirror's
    // existing base is already current, so the guest's rebase is a no-op too.
    if let Some(sha) = &plan.base_commit {
        r.line(format!(
            "{git} -C {} push --quiet {} \"{sha}:refs/heads/base\" --force",
            qp(&plan.project),
            qp(&sync_git)
        ));
    }
    // Re-open the whole mirror to "other" writes so the guest can push (the
    // mapped-xattr saga) — run every launch, idempotent.
    r.line(format!("chmod -R a+rwX {}", qp(&sync_git)));
    r.line("phase_done".to_string());

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
            .comment("Snapshot the host Nix DB so the guest reuses host-built paths (non-fatal).")
            .comment("VACUUM INTO, not .backup: SQLite's backup API restarts from page zero")
            .comment("whenever another connection writes the source, and the nix-daemon writes")
            .comment("db.sqlite on every derivation registration — so a snapshot overlapping any")
            .comment("host `nix build` (a concurrent launch is one) restarts indefinitely.")
            .comment("VACUUM INTO holds a read transaction instead and completes. `timeout` is")
            .comment("belt-and-braces: on expiry the guest boots unseeded, the already-designed")
            .comment("fallback, rather than wedging provisioning.");
        r.line("phase 'snapshotting host nix DB'".to_string());
        r.line(r#"hostdb="${NIX_STATE_DIR:-/nix/var/nix}/db/db.sqlite""#.to_string());
        // VACUUM INTO refuses to write an existing file, so clear any leftover
        // from a previous launch that timed out or was killed.
        r.line(format!("rm -f {}", qp(&tmp)));
        // The SQL is single-quoted for the *shell* (the path must not be
        // shell-interpreted); the inner quotes are SQLite's own string literal.
        // `mv` only on success, so a partial snapshot never lands as the real
        // `nix-db.sqlite` the guest's seeding service conditions on.
        // `[ -r "$hostdb" ]` first: sqlite3 *creates* a missing database on open,
        // so without the guard an absent host DB would snapshot successfully as
        // an empty one and ship that to the guest as if it were real. The
        // `… && … || { … }` shape (rather than `if`) keeps the flat-recipe
        // invariant — see the module docs on `emit`.
        r.line(format!(
            "[ -r \"$hostdb\" ] && timeout {NIX_DB_SNAPSHOT_TIMEOUT_SECS} {sqlite} \"$hostdb\" {} 2>/dev/null && mv -f {} {} || {{ rm -f {}; echo {}; }}",
            sq(&format!("VACUUM INTO '{}'", tmp.display())),
            qp(&tmp),
            qp(&dest),
            qp(&tmp),
            sq(&format!(
                "WARNING: host Nix DB snapshot failed or exceeded {NIX_DB_SNAPSHOT_TIMEOUT_SECS}s — the guest will boot with a system-only store DB (host-built paths won't be valid in the VM)."
            )),
        ));
        r.line("phase_done".to_string());
    }

    // ---- context staging, only when declared ----
    if !spec.context.is_empty() {
        let ctx_root = state_root.join("context");
        r.blank().comment(
            "Stage declared untracked context (rsync --safe-links drops escaping symlinks).",
        );
        r.line("phase 'staging workspace context'".to_string());
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
        r.line("phase_done".to_string());
    }

    // ---- secrets as REFERENCES, never values ----
    if !spec.secrets.is_empty() {
        r.blank()
            .comment("Declared secrets, staged as references — the value is re-read at runtime, never baked in.");
        r.line("phase 'staging secrets'".to_string());
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
        r.line("phase_done".to_string());
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

    // ---- hand the terminal back, and mark the last pre-boot phase ----
    //
    // The runner takes over from here: it writes `console.log` itself, and in
    // interactive mode the script goes on to hand a real TTY to ssh+tmux — which
    // it cannot do while stdout is the `tee` pipe. Restoring the saved fds also
    // closes the pipe's last writer, so `tee` flushes `provision.log` here.
    // The marker is cleared as soon as the runner is *launched*, not when QMP
    // starts answering, so the brief window between those two still reads
    // `stopped` — by then `console.log` exists and is the better diagnostic.
    r.blank()
        .comment("Provisioning done: restore the real stdout/stderr before the runner takes over.");
    r.line("phase 'booting VM'".to_string());
    r.line("exec 1>&3 2>&4 3>&- 4>&-".to_string());

    // ---- mode-specific tail ----
    match plan.mode {
        Mode::Agent => agent_tail(
            &mut r,
            &runner,
            &console_log,
            &phase_file,
            &directive::path(&roots.state_glob, name),
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
            &phase_file,
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
#[allow(clippy::too_many_arguments)]
fn agent_tail(
    r: &mut Recipe,
    runner: &str,
    console_log: &Path,
    phase_file: &Path,
    directive_file: &Path,
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
    // The runner owns the diagnostics from here (it writes console.log), so the
    // pre-boot phase marker has done its job — drop it, or a dead instance would
    // read as "provisioning" forever.
    r.line(format!("rm -f {}", qp(phase_file)));
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
            // `--until-report` (when set) rides through to the tail-called
            // `prompt` so a dispatched turn keeps the same armed-until-report
            // semantics as a direct `sandbox prompt --until-report`.
            let until = if plan.until_report {
                " --until-report"
            } else {
                ""
            };
            r.line(format!(
                "exec {} sandbox --config {} prompt \"{}\"{until} {}",
                spec.tools.katsuctl.display(),
                qp(config),
                plan.name,
                sq(text)
            ));
        }
        None => {
            r.line(format!(
                "echo \"sandbox: prompt it with: sandbox prompt {} \\\"<text>\\\"\"",
                plan.name
            ));
            // A promptless launch of a *named* instance may still be resuming
            // one that was originally dispatched with a directive — whether that
            // file is there is a genuine runtime fact, so the guard stays in the
            // script (same shape as the context-staging guards above).
            r.line(format!(
                "[ -f {} ] && echo \"sandbox: or resend its original directive: sandbox prompt {} --redeliver\" || true",
                qp(directive_file),
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
    phase_file: &Path,
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
    r.line(format!("  rm -f {}", qp(phase_file)));
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
    r.line(format!("rm -f {}", qp(phase_file)));
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
            until_report: false,
            gpu: None,
            base_commit: None,
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
            false,
            "20260627-120000",
            7,
            None,
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
            false,
            "20260627-120000",
            7,
            None,
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
            false,
            "20260627-120000",
            7,
            None,
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
            false,
            "20260627-120000",
            7,
            None,
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
            None,
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
            None,
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
            None,
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
            None,
        )
        .expect("seed");
        assert_eq!(seed, Seed::Fresh("snap".into()));
    }

    // ---- base refresh on resume ----

    #[test]
    fn it_pushes_base_when_tip_has_moved_on_resume() {
        // Resume with a new HEAD: the base ref in the mirror shows the old SHA;
        // resolve_base_refresh must return the new HEAD.
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out("newsha999\n"))) // rev-parse HEAD (project)
            .push_run(Ok(ok_out("oldsha111\n"))); // rev-parse --verify refs/heads/base (mirror)
        let result = resolve_base_refresh(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            Path::new("/state/sync.git"),
            &Seed::Resume("existingbranch789".into()),
        )
        .expect("resolve should succeed");
        assert_eq!(result, Some("newsha999".to_string()));
    }

    #[test]
    fn it_skips_base_when_tip_unchanged_on_resume() {
        // Same HEAD as the mirror's current base — no push needed.
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out("samesha\n"))) // rev-parse HEAD (project)
            .push_run(Ok(ok_out("samesha\n"))); // rev-parse --verify refs/heads/base (mirror)
        let result = resolve_base_refresh(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            Path::new("/state/sync.git"),
            &Seed::Resume("existingbranch789".into()),
        )
        .expect("resolve should succeed");
        assert_eq!(result, None, "unchanged tip must return None");
    }

    #[test]
    fn it_pushes_base_on_first_resume_when_no_base_ref_exists() {
        // Mirror has no refs/heads/base yet (first resume ever). The verify
        // returns a non-zero exit — treat as absent and push the current HEAD.
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out("firsttip\n"))) // rev-parse HEAD
            .push_run(Ok(Output {
                // rev-parse --verify refs/heads/base: ref absent
                status: ExitStatus::from_raw(128),
                stdout: Vec::new(),
                stderr: b"fatal: Needed a single revision".to_vec(),
            }));
        let result = resolve_base_refresh(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            Path::new("/state/sync.git"),
            &Seed::Resume("existingbranch789".into()),
        )
        .expect("resolve should succeed");
        assert_eq!(
            result,
            Some("firsttip".to_string()),
            "first resume must push base even when no base ref exists yet"
        );
    }

    #[test]
    fn it_returns_no_base_commit_for_a_fresh_seed() {
        // Fresh instances do not need a base push — the seed IS the base.
        let host = FakeHost::new();
        let result = resolve_base_refresh(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            Path::new("/state/sync.git"),
            &Seed::Fresh("freshsha".into()),
        )
        .expect("resolve should succeed");
        assert_eq!(result, None);
        assert!(
            host.calls().is_empty(),
            "fresh instance must not touch the host seam"
        );
    }

    #[test]
    fn it_emits_base_push_in_recipe_when_base_commit_is_set() {
        let spec = spec_with(vec![], vec![], false);
        let mut p = plan("myfeature-0badf00d", true, Mode::Interactive);
        p.clone_mirror = false;
        p.seed = Seed::Resume("existingbranch789".into());
        p.base_commit = Some("newbasesha".to_string());
        let recipe = render(&spec, &p);
        assert!(
            recipe.contains("newbasesha:refs/heads/base"),
            "recipe must push the base commit: {recipe}"
        );
    }

    #[test]
    fn it_omits_base_push_when_base_commit_is_none() {
        let spec = spec_with(vec![], vec![], false);
        let mut p = plan("myfeature-0badf00d", true, Mode::Interactive);
        p.clone_mirror = false;
        p.seed = Seed::Resume("existingbranch789".into());
        // base_commit is None by default — unchanged tip
        let recipe = render(&spec, &p);
        assert!(
            !recipe.contains("refs/heads/base"),
            "recipe must not push base when tip is unchanged: {recipe}"
        );
    }

    #[test]
    fn snapshot_named_resume_with_base_refresh() {
        let spec = spec_with(vec![env_secret()], vec![], false);
        let mut p = plan("myfeature-0badf00d", true, Mode::Agent);
        p.clone_mirror = false;
        p.seed = Seed::Resume("existingbranch789".into());
        p.base_commit = Some("deadbeefcafe1234deadbeefcafe1234deadbeef".to_string());
        p.prompt = Some("continue the work".into());
        insta::assert_snapshot!(render(&spec, &p));
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
    fn it_threads_until_report_into_the_prompt_tail_call() {
        // `--until-report` rides through to the tail-called `prompt` so a
        // dispatched turn keeps the armed-until-report semantics. Absent by
        // default (so the snapshots above stay byte-stable).
        let spec = spec_with(vec![env_secret()], vec![], false);
        let mut p = plan("card-abc123", false, Mode::Agent);
        p.prompt = Some("do the work".into());

        assert!(
            !render(&spec, &p).contains("--until-report"),
            "the flag must be absent unless set"
        );

        p.until_report = true;
        let recipe = render(&spec, &p);
        assert!(
            recipe.contains("prompt \"card-abc123\" --until-report "),
            "the tail-called prompt must carry --until-report: {recipe}"
        );
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
    fn nix_db_snapshot_is_bounded_and_uses_vacuum_into() {
        // Pinned explicitly, not just by the golden snapshot, because every
        // clause here is load-bearing and a re-blessed snapshot would hide a
        // regression. `.backup` restarts from page zero under a concurrent
        // writer, and the nix-daemon writes db.sqlite throughout any host
        // `nix build` — so the old command spun indefinitely (11m32s / 9.1 TB
        // read observed) whenever a launch overlapped one.
        let spec = spec_with(vec![file_secret()], vec![], true);
        let recipe = render(&spec, &plan("20260627-120000-4242", false, Mode::Agent));

        assert!(
            recipe.contains("VACUUM INTO"),
            "snapshot must use VACUUM INTO: {recipe}"
        );
        // Only the *commands* matter — the comment above the step names
        // `.backup` deliberately, to say why it is not used.
        assert!(
            !recipe
                .lines()
                .any(|l| !l.trim_start().starts_with('#') && l.contains(".backup")),
            "the restart-prone .backup must be gone from the commands: {recipe}"
        );
        assert!(
            recipe.contains(&format!("timeout {NIX_DB_SNAPSHOT_TIMEOUT_SECS} ")),
            "the snapshot must be time-bounded: {recipe}"
        );
        // VACUUM INTO refuses to overwrite, so a stale tmp from a killed launch
        // would otherwise wedge every subsequent launch.
        assert!(
            recipe.contains(".nix-db.sqlite.tmp'\n[ -r \"$hostdb\" ]"),
            "the tmp must be cleared immediately before the snapshot: {recipe}"
        );
        // sqlite3 creates a missing database on open, so without this guard an
        // absent host DB snapshots "successfully" as an empty one.
        assert!(
            recipe.contains("[ -r \"$hostdb\" ] &&"),
            "a missing host DB must skip the snapshot, not fabricate one: {recipe}"
        );
        // Non-fatal, no partial publish, and visibly warned.
        assert!(
            recipe.contains("|| { rm -f ") && recipe.contains("echo 'WARNING: host Nix DB"),
            "failure must clean up and warn: {recipe}"
        );
        for line in recipe.lines() {
            if !line.trim_start().starts_with('#') && line.contains("VACUUM INTO") {
                // One self-contained line whose failure branch swallows the
                // error, so `set -e` can never abort the launch here — and no
                // `if`, per the flat-recipe invariant.
                assert!(
                    !line.starts_with("if ") && line.ends_with("; }"),
                    "the step must stay flat, self-contained and non-fatal: {line}"
                );
            }
        }
    }

    #[test]
    fn an_aborted_launch_clears_its_phase_marker() {
        // A launch can abort anywhere in provisioning — a missing secret exits
        // 1, and `set -e`/Ctrl-C can end it mid-step. The marker must not
        // outlive the dead launch, or `sandbox status` reports `provisioning`
        // forever for an instance with no VM (worse than the `stopped` it
        // reported before the marker existed).
        for mode in [Mode::Agent, Mode::Interactive] {
            let spec = spec_with(vec![file_secret()], vec![], true);
            let recipe = render(&spec, &plan("20260627-120000-4242", false, mode));
            let trap = recipe
                .lines()
                .find(|l| l.starts_with("trap 'rm -f ") && l.contains("/phase'"))
                .unwrap_or_else(|| panic!("no phase-clearing trap for {mode:?}: {recipe}"));
            assert!(
                trap.contains("EXIT") && trap.contains("INT") && trap.contains("TERM"),
                "{mode:?}: the trap must cover abort and interrupt: {trap}"
            );

            // It has to be armed before anything that can fail — in particular
            // before the first `phase` call, or the window it guards is open.
            let trap_at = recipe.lines().position(|l| l == trap).unwrap();
            let first_phase = recipe
                .lines()
                .position(|l| l.starts_with("phase '"))
                .expect("a provisioning phase");
            assert!(
                trap_at < first_phase,
                "{mode:?}: the trap must be armed before the first phase"
            );
        }
    }

    #[test]
    fn every_provisioning_phase_is_closed_except_the_terminal_one() {
        // `provision.log` is only useful if each step's begin marker has a
        // matching end: an unclosed phase reads as "still running that step"
        // forever in the log. `booting VM` is deliberately the exception — the
        // runner takes over there and nothing returns to close it.
        for (label, spec, plan) in [
            (
                "full",
                spec_with(vec![file_secret()], vec!["dist/build.tar".into()], true),
                plan("20260627-120000-4242", false, Mode::Agent),
            ),
            (
                "no secrets, no context, no nix-db",
                spec_with(vec![], vec![], false),
                plan("20260627-120000-4242", false, Mode::Interactive),
            ),
        ] {
            let recipe = render(&spec, &plan);
            let opened: Vec<&str> = recipe
                .lines()
                .filter(|l| l.starts_with("phase '"))
                .collect();
            let closed = recipe.lines().filter(|l| *l == "phase_done").count();
            assert_eq!(
                opened.len(),
                closed + 1,
                "{label}: every phase but the last must close: {opened:?} / {closed} closes"
            );
            assert_eq!(
                opened.last().copied(),
                Some("phase 'booting VM'"),
                "{label}: the unclosed phase must be the terminal one"
            );
        }
    }

    #[test]
    fn no_nix_db_snapshot_step_without_import_host_store_db() {
        let spec = spec_with(vec![file_secret()], vec![], false);
        let recipe = render(&spec, &plan("20260627-120000-4242", false, Mode::Agent));
        assert!(!recipe.contains("VACUUM INTO"), "{recipe}");
        assert!(!recipe.contains("hostdb="), "{recipe}");
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
            false,
            "20260627-120000",
            7,
            None,
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

    // ---- board stripping from stash seed ----

    #[test]
    fn it_returns_stash_sha_unchanged_when_no_board_changes() {
        // diff returns empty output: stash and HEAD agree on the board dir.
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out(""))); // diff --name-only: no board changes
        let result = strip_board_from_stash(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            "refs/heads/sandbox/card-aabbcc-deadbeef",
            "stashsha111",
            Path::new("project/kanban"),
        )
        .unwrap();
        assert_eq!(
            result, "stashsha111",
            "stash sha returned unchanged when no board changes"
        );
    }

    #[test]
    fn it_builds_a_filtered_commit_when_board_changes_are_present() {
        // diff returns a path: board changed in stash; filtering must produce a new SHA.
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out("project/kanban/BOARD.md\n"))) // diff: board changed
            .push_run(Ok(ok_out(""))) // read-tree stash -> ok
            .push_run(Ok(ok_out(""))) // rm --cached board
            .push_run(Ok(ok_out("headtresha\n"))) // rev-parse --verify HEAD:project/kanban -> exists
            .push_run(Ok(ok_out(""))) // read-tree --prefix restore from HEAD
            .push_run(Ok(ok_out("filteredtreesha\n"))) // write-tree
            .push_run(Ok(ok_out("filteredcommitsha\n"))); // commit-tree
        let result = strip_board_from_stash(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            "refs/heads/sandbox/card-aabbcc-deadbeef",
            "stashsha111",
            Path::new("project/kanban"),
        )
        .unwrap();
        assert_eq!(
            result, "filteredcommitsha",
            "filtered commit sha returned when board changes stripped"
        );
    }

    #[test]
    fn it_still_builds_a_filtered_commit_when_head_board_is_absent() {
        // diff: board changed; HEAD:project/kanban does not exist (unusual, but
        // must not panic — we just skip the restore step).
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out("project/kanban/BOARD.md\n"))) // diff: board changed
            .push_run(Ok(ok_out(""))) // read-tree stash
            .push_run(Ok(ok_out(""))) // rm --cached
            .push_run(Ok(Output {
                // rev-parse --verify: HEAD:project/kanban absent
                status: ExitStatus::from_raw(128),
                stdout: Vec::new(),
                stderr: b"fatal: not a tree object".to_vec(),
            }))
            .push_run(Ok(ok_out("treesha-no-kanban\n"))) // write-tree
            .push_run(Ok(ok_out("commitsha-no-kanban\n"))); // commit-tree
        let result = strip_board_from_stash(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            "refs/heads/sandbox/card-aabbcc-deadbeef",
            "stashsha111",
            Path::new("project/kanban"),
        )
        .unwrap();
        assert_eq!(result, "commitsha-no-kanban");
    }

    #[test]
    fn it_fails_when_write_tree_returns_no_sha() {
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out("project/kanban/BOARD.md\n")))
            .push_run(Ok(ok_out(""))) // read-tree
            .push_run(Ok(ok_out(""))) // rm --cached
            .push_run(Ok(ok_out("headtreesha\n"))) // verify HEAD:kanban
            .push_run(Ok(ok_out(""))) // restore from HEAD
            .push_run(Ok(ok_out(""))); // write-tree returns nothing (failure)
        let err = strip_board_from_stash(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            "refs/heads/sandbox/card-aabbcc-deadbeef",
            "stashsha111",
            Path::new("project/kanban"),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("write-tree returned no SHA"),
            "should report write-tree failure: {err:#}"
        );
    }

    #[test]
    fn resolve_seed_with_board_exclude_calls_strip_when_stash_has_board_changes() {
        // resolve_seed: stash non-empty, board_exclude set, diff says board changed
        // → strip_board_from_stash is invoked and its filtered SHA is returned.
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out("stashsha\n"))) // stash create
            .push_run(Ok(ok_out("project/kanban/BOARD.md\n"))) // diff: board changed
            .push_run(Ok(ok_out(""))) // read-tree stash
            .push_run(Ok(ok_out(""))) // rm --cached
            .push_run(Ok(ok_out("headkanbansha\n"))) // rev-parse --verify HEAD:project/kanban
            .push_run(Ok(ok_out(""))) // read-tree --prefix restore
            .push_run(Ok(ok_out("filteredtreesha\n"))) // write-tree
            .push_run(Ok(ok_out("filteredcommit\n"))); // commit-tree
        let seed = resolve_seed(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            Path::new("/state/sync.git"),
            "refs/heads/sandbox/card-aabbcc-deadbeef",
            false,
            false,
            Some(Path::new("project/kanban")),
        )
        .expect("seed");
        assert_eq!(
            seed,
            Seed::Fresh("filteredcommit".into()),
            "filtered commit used as seed when board changes detected"
        );
    }

    #[test]
    fn resolve_seed_without_board_exclude_passes_stash_through_unchanged() {
        // board_exclude = None → no diff check, stash SHA used as-is.
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out("stashsha\n"))); // stash create
        let seed = resolve_seed(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            Path::new("/state/sync.git"),
            "refs/heads/sandbox/card-aabbcc-deadbeef",
            false,
            false,
            None,
        )
        .expect("seed");
        assert_eq!(
            seed,
            Seed::Fresh("stashsha".into()),
            "stash SHA used unchanged when board_exclude is None"
        );
        // Exactly one run call (the stash create); no diff was attempted.
        let runs = host
            .calls()
            .iter()
            .filter(|c| matches!(c, Call::Run(_)))
            .count();
        assert_eq!(
            runs,
            1,
            "only stash create must run; no diff check: {:?}",
            host.calls()
        );
    }

    #[test]
    fn it_strips_board_using_absolute_board_path() {
        // When board_exclude is absolute, it is normalised to a project-relative
        // path for the git commands.  diff should be called with `project/kanban`
        // (the relative part), not the full absolute path.
        let mut host = FakeHost::new();
        host.push_run(Ok(ok_out(""))); // diff: no board changes
        let result = strip_board_from_stash(
            &host,
            Path::new("/git"),
            Path::new("/proj"),
            "refs/heads/sandbox/x",
            "stashsha",
            Path::new("/proj/project/kanban"), // absolute board path
        )
        .unwrap();
        // No board changes → stash returned unchanged.
        assert_eq!(result, "stashsha");
        // The diff call must use the relative path `project/kanban`, not the
        // absolute one — check by inspecting the last Run arg.
        let diff_args = host.calls().into_iter().find_map(|c| match c {
            Call::Run(v) if v.iter().any(|a| a == "diff") => Some(v),
            _ => None,
        });
        let args = diff_args.expect("diff must have been called");
        assert!(
            args.last().map(|a| a.as_str()) == Some("project/kanban"),
            "diff must use the project-relative path: {args:?}"
        );
    }
}
