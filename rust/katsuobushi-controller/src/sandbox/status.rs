//! `katsuctl sandbox status` — list all instances or detail one.
//!
//! Replaces the shell `sandbox:status` command:
//! the byte-sorted instance listing (`_list_instances`), the live
//! `column -t` table, the per-instance detail view, and the
//! secret + `/dev/vhost-vsock` preflight (`statusSecretChecks` + the vhost row,
//! ). A bare `status` (list mode) doubles as the launch
//! prerequisite gate: it exits **nonzero** iff any declared secret is missing or
//! `vhost-vsock` is absent.
//!
//! The pure pieces — the preflight decision over a declared `secrets` set, the
//! table formatting, and the index↔name ordering — are factored out of the
//! world-touching derivation so they are unit-testable without a live VM: the
//! preflight takes injected env/file/vhost lookups, and the renderers take
//! already-derived [`InstanceView`]s (tier 1). Liveness, the
//! `instance.json` read, and the branch probe go through the host seam / the
//! `instance` model exactly as `stop`/`fetch` do.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::sandbox::host::{Host, HostImpl};
use crate::sandbox::instance::{self, Mode};
use crate::sandbox::liveness::{self, Liveness, TurnState};
use crate::sandbox::output::{render_table, CellStyle, Renderer, TableCell};
use crate::sandbox::resolve::{list_instances, resolve_instance};
use crate::sandbox::spec::{
    load_spec, resolve_roots, ResolvedRoots, SecretSource, SecretSpec, Spec,
};
use crate::Global;

/// Whether an instance's VM is up — derived live from QMP, never stored.
/// Serializes lowercase for the `--json` `state` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Running,
    Stopped,
}

/// One instance's derived summary — the unit of both the list array and the
/// detail object in `--json` (: name, state, mode, named, port, cid,
/// branch-present). `mode`/`port`/`cid` are `Option` because an instance whose
/// `instance.json` is missing or unreadable still lists (degraded), and only
/// agent instances carry a CID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstanceView {
    name: String,
    state: State,
    mode: Option<Mode>,
    named: bool,
    port: Option<u16>,
    cid: Option<u32>,
    branch_present: bool,
    /// The full per-instance liveness line — turn/agent state
    /// from `turn-state.json` plus transport freshness from `liveness.json`,
    /// corroborated against QMP. `None` when no `turn-state.json` is present
    /// (degrades to today's connection-derived behavior). Surfaced in the detail
    /// view and `--json`.
    #[serde(skip_serializing_if = "Option::is_none")]
    liveness: Option<String>,
    /// The compact list-column form (`in-flight 9m`); the detail/`--json` line
    /// carries the full phrase. Not serialized (the full `liveness` field is the
    /// machine-facing one).
    #[serde(skip)]
    liveness_brief: Option<String>,
}

impl InstanceView {
    /// Whether the VM is currently up.
    fn running(&self) -> bool {
        self.state == State::Running
    }

    /// The `MODE` column / detail value: the rendered mode, or `-` when unknown.
    fn mode_label(&self) -> &'static str {
        match self.mode {
            Some(Mode::Agent) => "agent",
            Some(Mode::Interactive) => "interactive",
            None => "-",
        }
    }

    /// The `PERSIST` column / detail value.
    fn persist_label(&self) -> &'static str {
        if self.named {
            "named"
        } else {
            "ephemeral"
        }
    }
}

// ---- liveness surfacing (the / out-of-band turn + transport line) ----

/// The compact age token (`9m`) for `then` measured from `now`, both already
/// through the host clock seam. `None` when either is absent or unparseable
/// (advisory: the phase still renders, just without an age).
fn age_token(now: Option<i64>, then: Option<&str>) -> Option<String> {
    let now = now?;
    let then = liveness::parse_rfc3339(then?)?;
    Some(liveness::humanize_ago(now, then))
}

/// The age token suffixed with `" ago"` (`9m ago`) for the full liveness line.
fn age_ago(now: Option<i64>, then: Option<&str>) -> Option<String> {
    age_token(now, then).map(|t| format!("{t} ago"))
}

/// Build the full per-instance liveness line.
///
/// Reads **`turn-state.json` first** (the guest-authoritative turn/agent state,
/// needs no connection); a missing record returns `None` so `status` degrades to
/// today's behavior. The transport tail is corroborated against QMP: an
/// active stream is only believed while the VM is up, so a stale `streamActive`
/// can never mask a dead server — `· no active stream` (VM up, merely idle / a
/// hung agent) is distinguished from `· vm stopped` (server gone).
///
/// Examples:
/// - `turn 3 ended-unreported 14m ago · no active stream` (unattended verdict)
/// - `turn 3 in-flight · last activity 9m ago · heartbeat 4s ago` (attached)
fn render_liveness(
    ts: Option<&TurnState>,
    lv: Option<&Liveness>,
    now: Option<i64>,
    running: bool,
) -> Option<String> {
    let ts = ts?;
    let mut parts: Vec<String> = Vec::new();

    // Head: `turn N <phase>` (the turn id is absent while idle), with an ended
    // phase carrying its `endedAt` age inline — this is the surfacing.
    let phase = ts.phase.label();
    let mut head = match ts.turn_id {
        Some(id) => format!("turn {id} {phase}"),
        None => phase.to_string(),
    };
    if ts.phase.is_ended() {
        if let Some(age) = age_ago(now, ts.ended_at.as_deref()) {
            head = format!("{head} {age}");
        }
    }
    parts.push(head);

    // A still-in-flight turn shows its last-activity age; a stale value here is
    // how the never-`Stop` hung-mid-tool case becomes visible.
    if ts.phase.is_in_flight() {
        if let Some(age) = age_ago(now, Some(ts.last_activity_at.as_str())) {
            parts.push(format!("last activity {age}"));
        }
    }

    // Transport tail, corroborated against QMP: only trust an active stream
    // while the VM is up.
    let stream_active = running && lv.is_some_and(|l| l.stream_active);
    if stream_active {
        match age_ago(now, lv.and_then(|l| l.last_heartbeat_at.as_deref())) {
            Some(age) => parts.push(format!("heartbeat {age}")),
            None => parts.push("stream active".to_string()),
        }
    } else if running {
        parts.push("no active stream".to_string());
    } else {
        // Neither file is fresh and the VM is down: the server is gone, not idle.
        parts.push("vm stopped".to_string());
    }

    Some(parts.join(" · "))
}

/// The compact list-column form: `<phase> <age>` (age of the relevant edge), or
/// just `<phase>` when no age is available. `None` (rendered `-`) when there is
/// no `turn-state.json`.
fn render_liveness_brief(ts: Option<&TurnState>, now: Option<i64>) -> Option<String> {
    let ts = ts?;
    let edge = if ts.phase.is_ended() {
        ts.ended_at.as_deref()
    } else if ts.phase.is_in_flight() {
        Some(ts.last_activity_at.as_str())
    } else {
        None
    };
    Some(match age_token(now, edge) {
        Some(age) => format!("{} {age}", ts.phase.label()),
        None => ts.phase.label().to_string(),
    })
}

// ---- preflight (the prerequisite gate, pure over injected lookups) ----

/// One row of the environment checklist: a label, whether it passed, and the
/// human detail (the actionable hint when it failed).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckRow {
    label: String,
    ok: bool,
    detail: String,
}

/// The result of the secret + `vhost-vsock` preflight. `problems() == 0` is the
/// gate: a bare `status` exits nonzero iff there is at least one problem.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Preflight {
    rows: Vec<CheckRow>,
}

impl Preflight {
    /// How many checks failed.
    fn problems(&self) -> usize {
        self.rows.iter().filter(|r| !r.ok).count()
    }

    /// Whether every check passed (the launch prerequisite is satisfied).
    fn ok(&self) -> bool {
        self.problems() == 0
    }
}

/// Re-check each declared secret at its *host* source and `/dev/vhost-vsock`,
/// pure over injected lookups so the gate decision is unit-testable without a
/// real env or filesystem (mirrors `statusSecretChecks` + the vhost row,
/// ).
///
/// A `FromEnv` secret passes iff its host env var is set and non-empty; a
/// `FromFile` secret passes iff its file is readable. The `CLAUDE_CODE_OAUTH_TOKEN`
/// hint matches the shell's special-cased `claude setup-token` guidance.
fn preflight(
    secrets: &[SecretSpec],
    get_env: impl Fn(&str) -> Option<String>,
    file_readable: impl Fn(&Path) -> bool,
    vhost_vsock_present: bool,
) -> Preflight {
    let mut rows = Vec::with_capacity(secrets.len() + 1);
    for secret in secrets {
        let (ok, detail) = match &secret.source {
            SecretSource::FromEnv(var) => {
                if get_env(var).is_some_and(|v| !v.is_empty()) {
                    (true, format!("ok (host env {var} is set)"))
                } else {
                    let hint = if secret.name == "CLAUDE_CODE_OAUTH_TOKEN" {
                        format!(" (run 'claude setup-token' and export its output as {var})")
                    } else {
                        String::new()
                    };
                    (false, format!("MISSING - export {var} on the host{hint}"))
                }
            }
            SecretSource::FromFile(path) => {
                if file_readable(Path::new(path)) {
                    (true, format!("ok (host file {path})"))
                } else {
                    (false, format!("MISSING - host file {path} not readable"))
                }
            }
        };
        rows.push(CheckRow {
            label: secret.name.clone(),
            ok,
            detail,
        });
    }

    let (ok, detail) = if vhost_vsock_present {
        (true, "ok".to_string())
    } else {
        (
            false,
            "MISSING - agent mode needs it (sudo modprobe vhost_vsock)".to_string(),
        )
    };
    rows.push(CheckRow {
        label: "vhost-vsock".to_string(),
        ok,
        detail,
    });

    Preflight { rows }
}

/// Render the preflight as a clean checklist. The glyph is plain
/// Unicode (✓/⚠) and survives color gating; only its coloring is gated, so with
/// color off this is `✓ label  detail` / `⚠ label  detail`. Labels are padded to
/// the widest so the detail column aligns, mirroring the shell's `%-Ns`.
fn render_preflight(pf: &Preflight, r: &Renderer) -> String {
    let width = pf.rows.iter().map(|row| row.label.len()).max().unwrap_or(0);
    let mut out = String::from("environment:");
    for row in &pf.rows {
        let glyph = if row.ok {
            r.green("✓")
        } else {
            r.yellow("⚠")
        };
        out.push_str(&format!(
            "\n  {glyph} {label:width$}  {detail}",
            label = row.label,
            detail = row.detail,
        ));
    }
    out
}

// ---- the list table (pure over already-derived views) ----

/// Render the instance table — the `comfy-table` replacement for `column -t`.
/// Rows are numbered 1..n in the
/// order given, which is the byte-sorted [`list_instances`] order so the `#`
/// column matches the index every other command accepts. State is color-coded
/// (running=green, stopped=dim) via styled cells the borderless table colors
/// itself, so widths measure the printable text and the columns stay aligned.
fn render_list(views: &[InstanceView], r: &Renderer) -> String {
    // Just orientation columns: the ssh port and vsock CID are internal plumbing,
    // not things you type (you drive an instance by name or `#`), so they stay in
    // the detail view (with the actual ssh/prompt commands) and in `--json` for
    // machine consumers — out of the scannable human list.
    // LIVENESS is the compact out-of-band turn/agent state — the column
    // that makes a swarm's "stopped without reporting" / hung-mid-tool instances
    // scannable without attaching; `-` when no `turn-state.json` is present.
    let headers = ["#", "INSTANCE", "STATE", "MODE", "PERSIST", "LIVENESS"];
    let rows: Vec<Vec<TableCell>> = views
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let state = if v.running() {
                TableCell::styled("running", CellStyle::Green)
            } else {
                TableCell::styled("stopped", CellStyle::Dim)
            };
            vec![
                TableCell::plain((i + 1).to_string()),
                TableCell::plain(v.name.clone()),
                state,
                TableCell::styled(v.mode_label(), CellStyle::Dim),
                TableCell::styled(v.persist_label(), CellStyle::Dim),
                TableCell::styled(v.liveness_brief.as_deref().unwrap_or("-"), CellStyle::Dim),
            ]
        })
        .collect();
    render_table(&headers, &rows, r.color())
}

// ---- the detail view (pure over the derived view + computed strings) ----

/// Render the single-instance detail block. The `ssh`/`attach` lines appear only when
/// running with a known port, `branch` only when the sandbox ref exists, and
/// `agent` only for an instance carrying a CID.
fn render_detail(v: &InstanceView, ssh: Option<&str>, console_log: &str, r: &Renderer) -> String {
    let state = if v.running() {
        r.green("running")
    } else {
        r.dim("stopped")
    };
    let mut lines = vec![
        format!("instance:   {}", v.name),
        format!("state:      {state}"),
        format!(
            "persistent: {}",
            if v.named {
                "named (persistent)"
            } else {
                "ephemeral"
            }
        ),
        format!("mode:       {}", v.mode_label()),
    ];
    // The out-of-band liveness line: turn/agent state + transport
    // freshness, present only once the guest has written a `turn-state.json`.
    if let Some(liveness) = &v.liveness {
        lines.push(format!("liveness:   {liveness}"));
    }
    if let Some(ssh) = ssh {
        lines.push(format!("ssh:        {ssh}"));
        lines.push(format!(
            "attach:     sandbox:attach {} (ssh in + attach the agent's tmux session)",
            v.name
        ));
    }
    if v.branch_present {
        lines.push(format!(
            "branch:     sandbox/{0} (fetch: sandbox:fetch {0})",
            v.name
        ));
    }
    if let Some(cid) = v.cid {
        lines.push(format!(
            "agent:      cid {cid} (prompt: sandbox:prompt {0} \"...\")",
            v.name
        ));
    }
    lines.push(format!("console:    {console_log}"));
    lines.join("\n")
}

// ---- the world-touching derivation (host seam + instance.json model) ----

/// Derive one instance's summary: liveness from QMP (through the seam), the
/// scalar metadata from `instance.json` (degrading to unknowns if it is missing
/// or version-skewed), and branch presence from a pinned `git rev-parse`.
fn summarize(
    host: &impl Host,
    spec: &Spec,
    roots: &ResolvedRoots,
    name: &str,
    now: Option<i64>,
) -> InstanceView {
    let sock = roots.runtime_glob.join(name).join("katsuobushi.sock");
    let running = host.qmp_alive(&sock);
    let state = if running {
        State::Running
    } else {
        State::Stopped
    };

    // A missing/unreadable instance.json still lists (degraded): liveness and
    // branch presence are derived independently of it.
    let meta = instance::read(&roots.state_glob, name).ok();
    let (mode, named, port, cid) = match &meta {
        Some(i) => (Some(i.mode), i.named, Some(i.ssh_port), i.vsock_cid),
        None => (None, false, None, None),
    };

    // Out-of-band liveness: read the guest-authored `turn-state.json`
    // first (no connection needed), then the host-written `liveness.json` for
    // transport freshness. Both are advisory — a missing/old file just degrades.
    let inst_dir = roots.state_glob.join(name);
    let turn_state = TurnState::read(host, &inst_dir.join("turn-state.json"));
    let live = Liveness::read(host, &inst_dir.join("liveness.json"));
    let liveness = render_liveness(turn_state.as_ref(), live.as_ref(), now, running);
    let liveness_brief = render_liveness_brief(turn_state.as_ref(), now);

    InstanceView {
        name: name.to_string(),
        state,
        mode,
        named,
        port,
        cid,
        branch_present: branch_present(host, spec, roots, name),
        liveness,
        liveness_brief,
    }
}

/// Whether `refs/heads/sandbox/<name>` exists in the instance's bare mirror —
/// the Rust form of a `git -C $d/sync.git rev-parse --verify` probe.
/// A missing mirror (or any git error) is simply
/// "no branch", never a hard failure.
fn branch_present(host: &impl Host, spec: &Spec, roots: &ResolvedRoots, name: &str) -> bool {
    let sync_git = roots.state_glob.join(name).join("sync.git");
    let mut cmd = Command::new(&spec.tools.git);
    cmd.arg("-C")
        .arg(&sync_git)
        .arg("rev-parse")
        .arg("--verify")
        .arg("--quiet")
        .arg(format!("refs/heads/sandbox/{name}"));
    host.run(&cmd).is_ok_and(|o| o.status.success())
}

/// Production entry point: load the spec, stand up the real host seam, then list
/// every instance (with the preflight gate) or detail the one named.
pub fn run(config: &Path, instance: Option<String>, global: Global) -> Result<()> {
    let spec = load_spec(config)?;
    let roots = resolve_roots(&spec.roots)?;
    let host = HostImpl::new().context("initializing the host IO seam")?;
    let renderer = Renderer::resolve(global);

    match instance {
        None => run_list(&host, &spec, &roots, &renderer),
        Some(inst) => run_detail(&host, &spec, &roots, &renderer, &inst),
    }
}

/// List mode: derive every instance, run the preflight gate, render, and exit
/// nonzero iff the preflight found a problem (the launch prerequisite gate,
/// ).
fn run_list(
    host: &impl Host,
    spec: &Spec,
    roots: &ResolvedRoots,
    renderer: &Renderer,
) -> Result<()> {
    // One clock read for the whole fleet, so every instance's liveness ages are
    // taken against the same `now` (and we shell out to `date` once, not per row).
    let now = liveness::now_unix(host);
    let names = list_instances(&roots.state_glob, host)?;
    let views: Vec<InstanceView> = names
        .iter()
        .map(|name| summarize(host, spec, roots, name, now))
        .collect();

    let pf = preflight(
        &spec.secrets,
        |k| std::env::var(k).ok(),
        |p| host.exists(p),
        host.exists(Path::new("/dev/vhost-vsock")),
    );

    if renderer.json() {
        // Structured output is just the instance array; the gate still governs
        // the exit status (a parser checks the array, the caller checks `$?`).
        println!("{}", serde_json::to_string(&views)?);
    } else {
        // The env block is shown only when there is a problem to act on (it
        // doubles as the prerequisite report), mirroring the shell's suppression
        // of the all-"ok" case.
        if !pf.ok() {
            println!("{}", render_preflight(&pf, renderer));
            eprintln!(
                "  ({} problem(s) above - resolve before launching)",
                pf.problems()
            );
            println!();
        }
        if views.is_empty() {
            println!("No active sandboxes");
        } else {
            println!("{}", render_list(&views, renderer));
        }
    }

    // Nonzero exit iff the preflight found a problem, so a bare `status` is a
    // usable launch prerequisite gate by its exit status alone.
    if !pf.ok() {
        std::process::exit(1);
    }
    Ok(())
}

/// Detail mode: resolve the selector, derive the instance, and print its fields.
fn run_detail(
    host: &impl Host,
    spec: &Spec,
    roots: &ResolvedRoots,
    renderer: &Renderer,
    instance: &str,
) -> Result<()> {
    let inst = resolve_instance(&roots.state_glob, host, instance)?;
    let now = liveness::now_unix(host);
    let view = summarize(host, spec, roots, &inst, now);

    // The ssh line only makes sense for a running instance with a known port.
    let ssh = if view.running() {
        view.port.map(|port| {
            format!(
                "ssh -i {}/id -p {port} -o StrictHostKeyChecking=no \
                 -o UserKnownHostsFile=/dev/null {}@127.0.0.1",
                roots.runtime_glob.join(&inst).display(),
                spec.agent_user,
            )
        })
    } else {
        None
    };
    let console_log = roots
        .state_glob
        .join(&inst)
        .join("console.log")
        .display()
        .to_string();

    renderer.emit(&view, |r| {
        render_detail(&view, ssh.as_deref(), &console_log, r)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host::FakeHost;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// True iff the string carries an ANSI escape (SGR) sequence — mirrors the
    /// `output` module's gating check so "plain when color off" is enforced.
    fn has_ansi(s: &str) -> bool {
        s.contains('\u{1b}')
    }

    /// A `FromEnv` secret declaration.
    fn env_secret(name: &str, var: &str) -> SecretSpec {
        SecretSpec {
            name: name.to_string(),
            source: SecretSource::FromEnv(var.to_string()),
            dest: format!("cred-{name}"),
        }
    }

    /// A fake env lookup over the given pairs.
    fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn view(name: &str, state: State, mode: Option<Mode>, named: bool) -> InstanceView {
        InstanceView {
            name: name.to_string(),
            state,
            mode,
            named,
            port: None,
            cid: None,
            branch_present: false,
            liveness: None,
            liveness_brief: None,
        }
    }

    // ---- table formatting (pure) ----

    #[test]
    fn it_renders_the_list_table_numbered_in_order_without_ansi() {
        let r = Renderer::new(false, false);
        let views = vec![
            InstanceView {
                port: Some(2222),
                cid: Some(4242),
                ..view("inst-a", State::Running, Some(Mode::Agent), true)
            },
            view("inst-b", State::Stopped, Some(Mode::Interactive), false),
        ];
        let table = render_list(&views, &r);

        assert!(!has_ansi(&table), "no ANSI when color off: {table:?}");
        // Orientation columns are present.
        for col in ["#", "INSTANCE", "STATE", "MODE", "PERSIST"] {
            assert!(table.contains(col), "header {col} present: {table}");
        }
        // The ssh port / vsock CID are plumbing, not list columns — they live in
        // the detail view and `--json` only.
        assert!(
            !table.contains("SSH") && !table.contains("CID"),
            "no SSH/CID columns in the list: {table}"
        );
        // Rows are numbered 1..n in the given order, carrying each name + fields.
        let rows: Vec<&str> = table
            .lines()
            .filter(|l| l.contains("inst-a") || l.contains("inst-b"))
            .collect();
        assert!(rows[0].starts_with("1 ") || rows[0].trim_start().starts_with('1'));
        assert!(rows[0].contains("inst-a") && rows[0].contains("running"));
        assert!(rows[0].contains("agent") && rows[0].contains("named"));
        // The ssh port / CID are no longer in the list row.
        assert!(!rows[0].contains("2222") && !rows[0].contains("4242"));
        assert!(rows[1].contains("inst-b") && rows[1].contains("stopped"));
        assert!(rows[1].contains("interactive") && rows[1].contains("ephemeral"));
    }

    #[test]
    fn it_colors_state_cells_when_color_is_on() {
        let r = Renderer::new(false, true);
        let table = render_list(&[view("inst-a", State::Running, None, false)], &r);
        assert!(has_ansi(&table), "running state is colored: {table:?}");
    }

    // ---- index↔name ordering parity with the shell `_list_instances` ----

    #[test]
    fn it_numbers_the_table_in_the_same_byte_order_resolve_indexes() {
        // The `#` column must denote the same instance the index resolves to.
        // Drive both the table enumeration and `resolve_instance` through the
        // seam over the SAME unsorted listing, then assert each row's number maps
        // back to the same name via `resolve_instance` (one shared, byte-sorted
        // `list_instances`).
        let state = PathBuf::from("/state/cdata/katsuobushi");
        let unsorted = ["inst-c", "inst-a", "inst-b"];

        let mut host = FakeHost::new();
        // One scripted listing for our own enumeration, then one per resolve call.
        host.push_list_dir(Ok(unsorted.iter().map(|s| s.to_string()).collect()))
            .push_list_dir(Ok(unsorted.iter().map(|s| s.to_string()).collect()))
            .push_list_dir(Ok(unsorted.iter().map(|s| s.to_string()).collect()))
            .push_list_dir(Ok(unsorted.iter().map(|s| s.to_string()).collect()));

        let names = list_instances(&state, &host).expect("listing");
        // Byte-sorted, the same order the table numbers against.
        assert_eq!(names, vec!["inst-a", "inst-b", "inst-c"]);

        let r = Renderer::new(false, false);
        let views: Vec<InstanceView> = names
            .iter()
            .map(|n| view(n, State::Stopped, None, false))
            .collect();
        let table = render_list(&views, &r);

        // Row k (1-based) names the same instance `resolve_instance("k")` returns.
        for (i, name) in names.iter().enumerate() {
            let idx = (i + 1).to_string();
            let resolved = resolve_instance(&state, &host, &idx).expect("index resolves in range");
            assert_eq!(&resolved, name, "row {idx} parity");
            // And that name appears on a table row.
            assert!(table.contains(name.as_str()), "row for {name}: {table}");
        }
    }

    // ---- the preflight gate (the secret-missing nonzero gate) ----

    #[test]
    fn it_gates_nonzero_when_a_secret_is_missing() {
        // The declared secret reads from HARNESS_OAUTH_TOKEN; with it absent the
        // preflight has a problem, so the bare `status` gate must be nonzero.
        let secrets = vec![env_secret("CLAUDE_CODE_OAUTH_TOKEN", "HARNESS_OAUTH_TOKEN")];
        let pf = preflight(
            &secrets,
            fake_env(&[]), // HARNESS_OAUTH_TOKEN unset
            |_| true,
            true, // vhost-vsock present
        );
        assert!(!pf.ok(), "a missing secret must fail the gate");
        assert_eq!(pf.problems(), 1);

        // The failing row names the host var + the setup-token hint.
        let row = pf
            .rows
            .iter()
            .find(|row| row.label == "CLAUDE_CODE_OAUTH_TOKEN")
            .expect("the secret row");
        assert!(!row.ok);
        assert!(row.detail.contains("HARNESS_OAUTH_TOKEN"), "{}", row.detail);
        assert!(row.detail.contains("claude setup-token"), "{}", row.detail);
    }

    #[test]
    fn it_treats_an_empty_secret_env_var_as_missing() {
        // Shell `-n "${VAR:-}"` rejects empty, not just unset.
        let secrets = vec![env_secret("TOK", "HOST_TOK")];
        let pf = preflight(&secrets, fake_env(&[("HOST_TOK", "")]), |_| true, true);
        assert!(!pf.ok(), "an empty env var counts as missing");
    }

    #[test]
    fn it_passes_the_gate_when_every_check_is_satisfied() {
        let secrets = vec![env_secret("TOK", "HOST_TOK")];
        let pf = preflight(
            &secrets,
            fake_env(&[("HOST_TOK", "s3cret")]),
            |_| true,
            true,
        );
        assert!(pf.ok(), "all present -> gate passes");
        assert_eq!(pf.problems(), 0);
    }

    #[test]
    fn it_gates_nonzero_when_vhost_vsock_is_absent() {
        // No secrets, but the vhost-vsock device is missing -> still a problem.
        let pf = preflight(&[], fake_env(&[]), |_| true, false);
        assert!(!pf.ok(), "missing vhost-vsock fails the gate");
        let row = pf.rows.last().expect("the vhost-vsock row");
        assert_eq!(row.label, "vhost-vsock");
        assert!(!row.ok);
        assert!(
            row.detail.contains("modprobe vhost_vsock"),
            "{}",
            row.detail
        );
    }

    #[test]
    fn it_checks_a_from_file_secret_at_its_path() {
        let secrets = vec![SecretSpec {
            name: "TOK".into(),
            source: SecretSource::FromFile("/run/secrets/tok".into()),
            dest: "cred-TOK".into(),
        }];
        // Readable -> ok.
        let ok = preflight(
            &secrets,
            fake_env(&[]),
            |p| p == Path::new("/run/secrets/tok"),
            true,
        );
        assert!(ok.ok(), "a readable file passes");
        // Unreadable -> problem naming the path.
        let bad = preflight(&secrets, fake_env(&[]), |_| false, true);
        assert!(!bad.ok());
        assert!(bad.rows[0].detail.contains("/run/secrets/tok"));
    }

    // ---- preflight rendering ----

    #[test]
    fn it_renders_the_preflight_checklist_without_ansi_when_color_off() {
        let r = Renderer::new(false, false);
        let secrets = vec![env_secret("TOK", "HOST_TOK")];
        let pf = preflight(&secrets, fake_env(&[]), |_| true, true);
        let text = render_preflight(&pf, &r);
        assert!(!has_ansi(&text), "plain when color off: {text:?}");
        assert!(text.starts_with("environment:"));
        // The ✓/⚠ glyphs survive gating (only their color is gated).
        assert!(text.contains('⚠'), "failing row glyph: {text}");
        assert!(text.contains('✓'), "passing (vhost) row glyph: {text}");
        assert!(text.contains("TOK"));
    }

    // ---- detail rendering ----

    #[test]
    fn it_renders_detail_lines_for_a_running_agent() {
        let r = Renderer::new(false, false);
        let v = InstanceView {
            port: Some(2222),
            cid: Some(4242),
            branch_present: true,
            ..view("inst-x", State::Running, Some(Mode::Agent), true)
        };
        let text = render_detail(
            &v,
            Some("ssh -i /run/inst-x/id ..."),
            "/state/inst-x/console.log",
            &r,
        );
        assert!(!has_ansi(&text));
        assert!(text.contains("instance:   inst-x"));
        assert!(text.contains("state:      running"));
        assert!(text.contains("persistent: named (persistent)"));
        assert!(text.contains("ssh:        ssh -i /run/inst-x/id"));
        assert!(text.contains("attach:     sandbox:attach inst-x"));
        assert!(text.contains("branch:     sandbox/inst-x (fetch: sandbox:fetch inst-x)"));
        assert!(text.contains("agent:      cid 4242 (prompt: sandbox:prompt inst-x"));
        assert!(text.contains("console:    /state/inst-x/console.log"));
    }

    #[test]
    fn it_omits_ssh_and_branch_lines_for_a_stopped_bare_instance() {
        let r = Renderer::new(false, false);
        let v = view("inst-y", State::Stopped, None, false);
        let text = render_detail(&v, None, "/state/inst-y/console.log", &r);
        assert!(text.contains("state:      stopped"));
        assert!(text.contains("persistent: ephemeral"));
        assert!(!text.contains("ssh:"), "no ssh line when stopped: {text}");
        assert!(
            !text.contains("attach:"),
            "no attach line when stopped: {text}"
        );
        assert!(
            !text.contains("branch:"),
            "no branch line when absent: {text}"
        );
        assert!(
            !text.contains("agent:"),
            "no agent line without a cid: {text}"
        );
        assert!(text.contains("console:    /state/inst-y/console.log"));
    }

    // ---- json shape ----

    #[test]
    fn it_serializes_the_instance_view_as_camelcase_json() {
        let v = InstanceView {
            port: Some(2222),
            cid: Some(4242),
            branch_present: true,
            ..view("inst-x", State::Running, Some(Mode::Agent), true)
        };
        let json = serde_json::to_string(&v).expect("serialize");
        assert_eq!(
            json,
            r#"{"name":"inst-x","state":"running","mode":"agent","named":true,"port":2222,"cid":4242,"branchPresent":true}"#
        );
    }

    #[test]
    fn it_serializes_missing_metadata_as_nulls() {
        let v = view("inst-z", State::Stopped, None, false);
        let json = serde_json::to_string(&v).expect("serialize");
        assert!(json.contains(r#""mode":null"#), "{json}");
        assert!(json.contains(r#""port":null"#), "{json}");
        assert!(json.contains(r#""cid":null"#), "{json}");
        assert!(json.contains(r#""state":"stopped""#), "{json}");
    }

    // ---- summarize through the seam ----

    #[test]
    fn it_summarizes_liveness_and_branch_through_the_seam() {
        use crate::sandbox::spec::{Roots, Tools};
        let spec = Spec {
            spec_version: 2,
            project_id: "cdata/katsuobushi".into(),
            agent_user: "agent".into(),
            import_host_store_db: false,
            roots: Roots {
                state_glob: PathBuf::from("/state"),
                runtime_glob: PathBuf::from("/run"),
            },
            tools: Tools {
                git: PathBuf::from("/nix/store/h1-git/bin/git"),
                ssh: PathBuf::from("/bin/ssh"),
                ssh_keygen: PathBuf::from("/bin/ssh-keygen"),
                tmux: PathBuf::from("/bin/tmux"),
                rsync: PathBuf::from("/bin/rsync"),
                sqlite3: None,
                bash: PathBuf::from("/bin/bash"),
            },
            runner: PathBuf::from("/bin/microvm-run"),
            disk_images: vec![],
            context: vec![],
            secrets: vec![],
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
        };
        let roots = ResolvedRoots {
            state_glob: PathBuf::from("/state"),
            runtime_glob: PathBuf::from("/run"),
        };

        // Mark the QMP socket alive; the default `run` queue returns success, so
        // the branch probe reports present.
        let mut host = FakeHost::new();
        host.with_alive_sock(
            PathBuf::from("/run")
                .join("inst-a")
                .join("katsuobushi.sock"),
        );

        let v = summarize(&host, &spec, &roots, "inst-a", None);
        assert_eq!(v.state, State::Running, "alive socket -> running");
        assert!(v.branch_present, "git rev-parse success -> branch present");
        // No instance.json on the fake fs -> degraded metadata.
        assert_eq!(v.mode, None);
        assert!(!v.named);

        // The branch probe goes through the seam as the pinned git rev-parse.
        use crate::sandbox::host::Call;
        let ran = host.calls().into_iter().any(|c| {
            matches!(c, Call::Run(args)
                if args.first().map(String::as_str) == Some("/nix/store/h1-git/bin/git")
                    && args.contains(&"rev-parse".to_string())
                    && args.contains(&"refs/heads/sandbox/inst-a".to_string()))
        });
        assert!(
            ran,
            "branch presence runs the pinned git rev-parse: {:?}",
            host.calls()
        );
    }

    #[test]
    fn it_reports_stopped_and_no_branch_when_the_seam_says_so() {
        use crate::sandbox::spec::{Roots, Tools};
        let spec = Spec {
            spec_version: 2,
            project_id: "p".into(),
            agent_user: "agent".into(),
            import_host_store_db: false,
            roots: Roots {
                state_glob: PathBuf::from("/state"),
                runtime_glob: PathBuf::from("/run"),
            },
            tools: Tools {
                git: PathBuf::from("/bin/git"),
                ssh: PathBuf::from("/bin/ssh"),
                ssh_keygen: PathBuf::from("/bin/ssh-keygen"),
                tmux: PathBuf::from("/bin/tmux"),
                rsync: PathBuf::from("/bin/rsync"),
                sqlite3: None,
                bash: PathBuf::from("/bin/bash"),
            },
            runner: PathBuf::from("/bin/microvm-run"),
            disk_images: vec![],
            context: vec![],
            secrets: vec![],
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
        };
        let roots = ResolvedRoots {
            state_glob: PathBuf::from("/state"),
            runtime_glob: PathBuf::from("/run"),
        };

        // Socket not alive; scripted git failure -> no branch.
        let mut host = FakeHost::new();
        host.push_run(Ok(output_failed()));
        let v = summarize(&host, &spec, &roots, "inst-dead", None);
        assert_eq!(v.state, State::Stopped);
        assert!(!v.branch_present);
    }

    /// A failed `git rev-parse` output (exit code 1).
    fn output_failed() -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    // ---- liveness surfacing (the // out-of-band line) ----

    use crate::sandbox::liveness::Phase;

    /// `2026-06-28T12:00:00Z` as Unix seconds — the fixed "now" the liveness
    /// tests measure ages against.
    fn now() -> Option<i64> {
        liveness::parse_rfc3339("2026-06-28T12:00:00Z")
    }

    /// A guest-shaped `TurnState` for the render tests.
    fn turn_state(
        phase: Phase,
        turn_id: Option<u64>,
        ended_at: Option<&str>,
        last_activity_at: &str,
    ) -> TurnState {
        TurnState {
            turn_state_version: 1,
            turn_id,
            phase,
            accepted_at: None,
            ended_at: ended_at.map(String::from),
            last_activity_at: last_activity_at.to_string(),
        }
    }

    #[test]
    fn it_surfaces_an_unattended_ended_unreported_turn() {
        // verdict, persisted: ended 14m ago with nobody driving.
        let state = turn_state(
            Phase::EndedUnreported,
            Some(3),
            Some("2026-06-28T11:46:00Z"),
            "2026-06-28T11:46:00Z",
        );
        let line = render_liveness(Some(&state), None, now(), true).expect("a line");
        assert_eq!(line, "turn 3 ended-unreported 14m ago · no active stream");
    }

    #[test]
    fn it_surfaces_an_attached_in_flight_turn_with_a_fresh_heartbeat() {
        let state = turn_state(Phase::InFlight, Some(3), None, "2026-06-28T11:51:00Z");
        let lv = Liveness {
            next_turn_id: 4,
            last_heartbeat_at: Some("2026-06-28T11:59:56Z".to_string()),
            stream_active: true,
            ..Liveness::default()
        };
        let line = render_liveness(Some(&state), Some(&lv), now(), true).expect("a line");
        assert_eq!(
            line,
            "turn 3 in-flight · last activity 9m ago · heartbeat 4s ago"
        );
    }

    #[test]
    fn it_renders_a_clean_ended_ok_turn() {
        let state = turn_state(
            Phase::EndedOk,
            Some(3),
            Some("2026-06-28T11:57:00Z"),
            "2026-06-28T11:57:00Z",
        );
        let line = render_liveness(Some(&state), None, now(), true).expect("a line");
        assert_eq!(line, "turn 3 ended-ok 3m ago · no active stream");
    }

    #[test]
    fn it_makes_a_hung_mid_tool_turn_visible_via_a_stale_last_activity() {
        // in-flight with no Stop and a long-stale lastActivityAt: the
        // age is what surfaces the hang, with no connection.
        let state = turn_state(Phase::InFlight, Some(3), None, "2026-06-28T11:46:00Z");
        let line = render_liveness(Some(&state), None, now(), true).expect("a line");
        assert_eq!(
            line,
            "turn 3 in-flight · last activity 14m ago · no active stream"
        );
    }

    #[test]
    fn it_distinguishes_a_dead_server_from_an_idle_one_via_qmp() {
        //: a stale in-flight file is ambiguous on its own. QMP corroborates —
        // VM up ⇒ "no active stream" (idle / hung agent, server alive); VM down ⇒
        // "vm stopped" (the server is gone, which is why the file went stale).
        let state = turn_state(Phase::InFlight, Some(3), None, "2026-06-28T11:46:00Z");

        let up = render_liveness(Some(&state), None, now(), true).expect("a line");
        assert!(up.ends_with("· no active stream"), "VM up ⇒ idle: {up}");

        let down = render_liveness(Some(&state), None, now(), false).expect("a line");
        assert!(down.ends_with("· vm stopped"), "VM down ⇒ dead: {down}");
    }

    #[test]
    fn it_never_believes_an_active_stream_when_the_vm_is_down() {
        // A stale `streamActive:true` liveness must not mask a dead VM: QMP
        // wins, so the tail is "vm stopped", not a phantom heartbeat.
        let state = turn_state(Phase::InFlight, Some(3), None, "2026-06-28T11:46:00Z");
        let lv = Liveness {
            last_heartbeat_at: Some("2026-06-28T11:46:00Z".to_string()),
            stream_active: true,
            ..Liveness::default()
        };
        let line = render_liveness(Some(&state), Some(&lv), now(), false).expect("a line");
        assert!(line.ends_with("· vm stopped"), "{line}");
    }

    #[test]
    fn it_degrades_to_no_line_when_there_is_no_turn_state() {
        // Missing turn-state.json ⇒ no liveness line, regardless of VM state
        // (advisory: degrade to today's behavior, never an error).
        assert!(render_liveness(None, None, now(), true).is_none());
        assert!(render_liveness(None, None, now(), false).is_none());
        assert!(render_liveness_brief(None, now()).is_none());
    }

    #[test]
    fn it_renders_phases_without_ages_when_the_clock_is_unavailable() {
        // No `now` (clock seam failed) ⇒ the phase still surfaces, just no ages.
        let state = turn_state(
            Phase::EndedUnreported,
            Some(3),
            Some("2026-06-28T11:46:00Z"),
            "2026-06-28T11:46:00Z",
        );
        let line = render_liveness(Some(&state), None, None, true).expect("a line");
        assert_eq!(line, "turn 3 ended-unreported · no active stream");
    }

    #[test]
    fn it_renders_the_compact_brief_for_the_list_column() {
        let in_flight = turn_state(Phase::InFlight, Some(3), None, "2026-06-28T11:51:00Z");
        assert_eq!(
            render_liveness_brief(Some(&in_flight), now()).unwrap(),
            "in-flight 9m"
        );
        let unreported = turn_state(
            Phase::EndedUnreported,
            Some(3),
            Some("2026-06-28T11:46:00Z"),
            "2026-06-28T11:46:00Z",
        );
        assert_eq!(
            render_liveness_brief(Some(&unreported), now()).unwrap(),
            "ended-unreported 14m"
        );
        // Idle has no age edge → just the phase token.
        let idle = turn_state(Phase::Idle, None, None, "");
        assert_eq!(render_liveness_brief(Some(&idle), now()).unwrap(), "idle");
    }

    #[test]
    fn it_renders_the_liveness_brief_as_a_list_column() {
        let r = Renderer::new(false, false);
        let mut v = view("inst-a", State::Running, Some(Mode::Agent), true);
        v.liveness_brief = Some("ended-unreported 14m".to_string());
        let table = render_list(&[v], &r);
        assert!(table.contains("LIVENESS"), "header present: {table}");
        assert!(
            table.contains("ended-unreported 14m"),
            "cell present: {table}"
        );
    }

    #[test]
    fn it_renders_the_liveness_line_in_the_detail_view() {
        let r = Renderer::new(false, false);
        let v = InstanceView {
            liveness: Some("turn 3 ended-unreported 14m ago · no active stream".to_string()),
            ..view("inst-x", State::Running, Some(Mode::Agent), true)
        };
        let text = render_detail(&v, None, "/state/inst-x/console.log", &r);
        assert!(
            text.contains("liveness:   turn 3 ended-unreported 14m ago · no active stream"),
            "{text}"
        );
    }

    // ---- summarize reads the durable records out-of-band, no vsock ----

    fn sample_spec() -> Spec {
        use crate::sandbox::spec::{Roots, Tools};
        Spec {
            spec_version: 2,
            project_id: "cdata/katsuobushi".into(),
            agent_user: "agent".into(),
            import_host_store_db: false,
            roots: Roots {
                state_glob: PathBuf::from("/state"),
                runtime_glob: PathBuf::from("/run"),
            },
            tools: Tools {
                git: PathBuf::from("/nix/store/h1-git/bin/git"),
                ssh: PathBuf::from("/bin/ssh"),
                ssh_keygen: PathBuf::from("/bin/ssh-keygen"),
                tmux: PathBuf::from("/bin/tmux"),
                rsync: PathBuf::from("/bin/rsync"),
                sqlite3: None,
                bash: PathBuf::from("/bin/bash"),
            },
            runner: PathBuf::from("/bin/microvm-run"),
            disk_images: vec![],
            context: vec![],
            secrets: vec![],
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

    fn sample_roots() -> ResolvedRoots {
        ResolvedRoots {
            state_glob: PathBuf::from("/state"),
            runtime_glob: PathBuf::from("/run"),
        }
    }

    #[test]
    fn it_summarizes_turn_state_out_of_band_without_a_vsock_connection() {
        use crate::sandbox::host::Call;
        // turn-state.json present (in-flight), liveness.json absent, instance.json
        // absent — `status` still surfaces the agent state with NO connection.
        let mut host = FakeHost::new();
        host.with_alive_sock(
            PathBuf::from("/run")
                .join("inst-a")
                .join("katsuobushi.sock"),
        )
        .push_read(Ok(
            br#"{"turnStateVersion":1,"turnId":3,"phase":"in-flight","acceptedAt":"2026-06-28T11:50:00Z","endedAt":null,"lastActivityAt":"2026-06-28T11:46:00Z"}"#
                .to_vec(),
        ));

        let v = summarize(&host, &sample_spec(), &sample_roots(), "inst-a", now());
        let line = v.liveness.expect("a liveness line from turn-state.json");
        assert_eq!(
            line,
            "turn 3 in-flight · last activity 14m ago · no active stream"
        );
        assert_eq!(v.liveness_brief.as_deref(), Some("in-flight 14m"));

        // The turn-state read goes through the seam at the per-instance path, and
        // no vsock connection is ever attempted to surface it.
        assert!(
            host.calls()
                .iter()
                .any(|c| matches!(c, Call::Read(p) if p.ends_with("turn-state.json"))),
            "turn-state.json read through the seam: {:?}",
            host.calls()
        );
        assert!(
            !host
                .calls()
                .iter()
                .any(|c| matches!(c, Call::VsockConnect(..))),
            "no vsock connection to surface turn state: {:?}",
            host.calls()
        );
    }

    #[test]
    fn it_summarizes_without_a_liveness_line_when_no_records_exist() {
        // No turn-state.json (and no liveness.json) ⇒ no line, no error: the view
        // degrades to exactly today's fields.
        let mut host = FakeHost::new();
        host.with_alive_sock(
            PathBuf::from("/run")
                .join("inst-a")
                .join("katsuobushi.sock"),
        );
        let v = summarize(&host, &sample_spec(), &sample_roots(), "inst-a", now());
        assert!(v.liveness.is_none(), "no turn-state ⇒ no liveness line");
        assert!(v.liveness_brief.is_none());
    }
}
