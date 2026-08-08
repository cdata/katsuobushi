---
name: sandbox
description:
  Run agent work inside an ephemeral, network-restricted Katsuobushi sandbox VM
  with a bounded blast radius, and orchestrate it from the host. Use this skill
  when the user wants to "use the sandbox to…", delegate a task to a sandbox or
  VM, run risky / long-running / parallel work in isolation, spin up an
  agent-mode sandbox, push prompts to a running sandbox instance, check on or
  fetch a sandbox's work, attach to a running sandbox's live session, screenshot
  a graphics-enabled sandbox, or stop one — i.e. anything involving the sandbox
  start / sandbox prompt / sandbox status / sandbox attach / sandbox fetch /
  sandbox screenshot / sandbox stop commands or `nix run .#sandbox`.
---

# Driving a Katsuobushi sandbox

A Katsuobushi sandbox is a hermetic `microvm.nix` guest (a real NixOS VM under
QEMU). An agent harness runs inside it with its blast radius bounded by the VM:
default-deny network behind an HTTPS allowlist, no host access, unprivileged.
Work returns as a pushed git branch.

You are the **host orchestrator**. You launch a sandbox, push it prompts over a
private host↔guest channel (the _sandbox controller_), read its status reports,
and collect its branch. The full human guide is at
<https://github.com/cdata/katsuobushi/blob/main/lib/sandbox/README.md>.

## When to use this

Delegate to a sandbox when work should be **isolated** or run **unattended / in
parallel**: risky refactors, running untrusted or experimental code, letting an
agent grind on a task with auto-approved tool use, or fanning out several tasks
at once. Each sandbox is an independent VM with its own branch.

Do **not** reach for it for quick edits in the current repo — it's for bounded,
delegated work.

## Prerequisites

**Run `sandbox status` to get your bearings** before you do anything else.

It may reveal the following problems:

- Missing command; if `sandbox status` is not available, then you probably need
  to drop into the Nix dev shell. `nix develop -c sandbox status` should work if
  you are in a folder with a viable dev shell.
- Missing environment variables; if any are detected as missing, share which
  ones with the user and ask them to export them by name (give them a helpful
  example).
- Missing `vhost-vsock`; if `qemu` is not compiled with this feature, or if the
  kernel module is not available you won't be able to communicate with the
  sandboxed agent; the user may need to load it with `sudo modprobe vhost_vsock`
  (although this probably isnt the "fix" on a NixOS system).

## Configuring a project's sandbox

If a project doesn't yet expose the `sandbox` command, offer to wire the library
into the local flake. The call lives in the per-system outputs (alongside
`apps.sandbox` / `checks.sandbox` and the dev-shell menu; see
`templates/sandbox/flake.nix` in the katsuobushi repo for the full flake). A
comprehensive call exercising every consumer-facing argument:

```nix
sandbox = katsuobushi.lib.sandbox {
  inherit pkgs;

  # Identity
  workspaceRoot = ./.;                 # project root; builds the per-instance mirror at launch
  projectId = "my-org/my-project";     # owner-qualified; names the in-guest path + host state dirs

  # Network egress (appended to the lean Anthropic+Nix baseline)
  #
  # Hostnames only, no implicit wildcards; HTTPS (443) assumed; else default-deny.
  allowedOrigins = [ "crates.io" "static.crates.io" "index.crates.io" ];
  # No per-entry removal — override the whole baseline to drop a host:
  #   baseAllowedOrigins = [ "api.anthropic.com" "platform.claude.com" ];

  # Guest PATH: the agent harness + tooling (the lib ships no harness)
  packages = [
    llm-agents.packages.${system}.claude-code   # or pkgs.claude-code (unfree)
    pkgs.cargo
    pkgs.ripgrep
  ];

  # Runtime secrets: read from the host at launch; never in the store
  #
  # The guest always sees CLAUDE_CODE_OAUTH_TOKEN; `fromEnv` picks which *host*
  # var supplies it. An agent harness scrubs CLAUDE_CODE_OAUTH_TOKEN from its
  # children, so when one launches the sandbox, source it from a differently-
  # named var (e.g. "HARNESS_OAUTH_TOKEN"). `sandbox status` reports which.
  secrets = {
    CLAUDE_CODE_OAUTH_TOKEN.fromEnv = "CLAUDE_CODE_OAUTH_TOKEN";
    # SOME_API_KEY.fromFile = "/run/secrets/some-api-key";
  };

  # Reference repos: build-time pinned, writable copies in the VM
  #
  # `source` = any store path (a `flake = false` input / fetcher); `dest`
  # mirrors ~/Git/<host>/<owner>/<repo>. One-way; host need NOT be allowlisted.
  extraRepos = [
    { source = rust-overlay-src; dest = "Git/github.com/oxalica/rust-overlay"; }
  ];

  # Untracked project context overlaid on the workspace (host -> guest)
  #
  # Project-relative paths; absolute/".." rejected; escaping symlinks dropped.
  workspaceContext = [ ".claude" "notes" ];

  # Files mapped into the agent's home
  #
  # dest -> { source; path?; mode }; mode: "immutable" | "seed" | "link"
  homeFiles = {
    ".claude/CLAUDE.md" = {
      source = nixos-config;           # a `flake = false` input
      path = "AGENTS.md";
      mode = "immutable";
    };
  };

  # Graphics (opt-in; default off) — see "Graphics (opt-in)" below
  #
  # Boots a headless sway compositor + a paravirtual GPU so a browser
  # (WebDriver/Playwright) or any Wayland app can actually render. The browser/app
  # itself is an ordinary package — it goes in `packages` above, not here.
  graphics = {
    enable = true;
    # Host GPU role ladder, resolved at launch; first present + openable wins.
    # "software" is the llvmpipe (CPU) tail — always available, and the safe
    # rung (no host GPU device handed to QEMU). Pin `[ "software" ]` to keep the
    # full original boundary at a perf cost.
    gpu = [ "integrated" "discrete" "software" ];
    output = { width = 1920; height = 1080; refresh = 60; };
  };

  # Resources
  vcpu = 4;
  mem = 8192;                          # MiB — avoid exactly 2048 (QEMU hangs)
                                       # graphics needs a floor: vcpu >= 4, mem >= 8192
  # Disk-backed writable scratch (sparse raw images, MiB). Named instances keep
  # these across stop/restart, so warm build caches survive a pause.
  storeVolumeSize = 16384;             # writable /nix/store overlay
  scratchVolumeSize = 32768;           # workspace clone + cargo/rustup/XDG caches
  dbVolumeSize = 4096;                 # guest Nix database

  # Escape hatch: extra NixOS modules merged into the guest
  guestModules = [ ./guest-extra.nix ];
};
```

`llm-agents` / `rust-overlay-src` / `nixos-config` are flake inputs the project
declares; `system` comes from `flake-utils`. The internal `microvm` / `rust` /
`controlSrc` arguments are supplied by Katsuobushi — consumers don't set them.
The fastest starting point is
`nix flake init -t github:cdata/katsuobushi#sandbox`.

## Graphics (opt-in)

By default the guest is headless with no GPU, so a browser or Wayland app has
nothing to render against. The `graphics` attrset (above) opts in: it boots a
headless sway compositor on a virtual output and gives the guest a paravirtual
GPU. Off by default; existing instances are unaffected. Material points for an
orchestrator:

- **Enable per project, not per launch.** It is a `lib.sandbox` config arg, so
  it applies to every instance of that project; offer to add the `graphics`
  block (and raise `vcpu`/`mem` to the floor) when the user wants browser/GUI
  work. The browser/app is an ordinary entry in `packages`.
- **`sandbox status` shows it.** When graphics is enabled, the instance list
  gains a **GRAPHICS** column — `integrated` / `discrete` / `software` (the rung
  the instance actually launched on), or `none` when graphics is off — and the
  preflight gains a `graphics` row that resolves the GPU against the host now.
  That row reports the **`render`-group prerequisite**: a host render node is
  portably `root:render 0660`, so the launching uid may need to be in the
  `render` group; the row reads `MISSING - … add yourself to the 'render' group`
  (non-zero exit) when no node is openable and the `gpu` list has no `software`
  tail. Surface that fix to the user as you would a missing secret.
- **`sandbox screenshot <name|#> [path]`** grabs a PNG of the headless-sway
  output over the existing ssh (`-` streams to stdout; default is a timestamped
  PNG in the cwd). Works in both modes; requires the opt-in. A purely offscreen
  workload that never puts a surface on the compositor screenshots **blank** —
  expected, not a bug.
- **Boundary delta.** A resolved GPU rung means the guest's GPU command stream
  is parsed by virglrenderer inside the host QEMU process — the one place
  graphics widens the host-facing attack surface. It does not exist with
  graphics off or on the `software` rung. A launch-time notice repeats this. The
  full treatment (and the `gpu = [ "software" ]` escape valve) is in the README.

## Launch a sandboxed agent

```sh
# Boot a lingering agent VM; returns immediately once it's up.
sandbox start --agent --name "<name>"

# …or boot AND send the first directive, streaming reports until done/blocked:
sandbox start --agent --name "<name>" --prompt "<directive>"

# Alternatively for debugging you can invoke the sandbox binary directly e.g.,
# nix run .#sandbox -- --agent --name <name>
```

Agent-mode VMs **linger** (they outlive the launch command). A dormant Claude
session runs inside the VM with the controller armed. After a no-`--prompt`
launch, the VM still needs ~30–60s to finish booting and arm the channel before
it will answer — if `sandbox prompt` can't connect, wait and retry.

Give a directive that says how to finish, e.g.: _"Do X. Commit and push on the
branch. Run `report done \"<summary>\"` when complete;
`report blocked \"<what you need>\"` if you get stuck."_

## Driving the agent

```sh
sandbox prompt <name> "<the next directive>"
```

Each prompt to a **running** instance is the next turn in the **same**
conversation — context is retained across pokes. Iterate: "do X" → done → "now
Y" → done → "finish up". The command streams the agent's status lines and
returns when the agent reports a terminal status:

- `working` — progress (optional, non-terminal).
- `done` — the turn is complete; the work product is the pushed branch.
- `blocked` — it needs something; address it and send the next prompt.
- `info` — anything else worth surfacing.

**When an agent ends its turn without a terminal report**, what happens depends
on which command is driving — and the defaults differ deliberately:

| Command                            | Default on an unreported yield                              |
| ---------------------------------- | ----------------------------------------------------------- |
| `sandbox dispatch`                 | **stays armed**, keeps waiting for `done`/`blocked`         |
| `sandbox start --agent --prompt …` | **stays armed** (a first-turn directive is dispatch-shaped) |
| `sandbox prompt` (interactive)     | returns with a "stopped without reporting" warning          |

The orchestration flows stay armed because for them "the command returned" is
read as "the work concluded"; returning early on a yield would say the work is
finished when it is not. Pass **`--no-until-report`** to opt out.
(`--until-report` is still accepted on those two and is now a no-op.)

Interactive `sandbox prompt` keeps returning, because you are watching and can
re-prompt — and its warning names both recoveries: re-run with `--until-report`
to wait, or look for a later `ended-ok` in the instance's `turn-state.json` /
`sandbox status`. That matters because the guest **auto-nudges** an unreported
idle agent a few times on its own, so a report may still land after the command
exits — most silent stops recover without you doing anything.

**Reports are journaled — a lost terminal doesn't lose the verdict.** Every
relayed report, plus the stopped/re-armed lifecycle notices, is appended to
`reports.ndjson` in the instance's state dir as it streams. So if the driving
process's output is gone (terminal closed, tmux pane died, scrollback lost) you
do **not** need to re-prompt the agent to restate itself:

```sh
sandbox status <name>            # `last report:` shows the latest conclusion
```

The detail view shows the first line; the full text (multi-line verdicts
included) is in `reports.ndjson`, one JSON object per line with `turnId`,
`status`, `text`, and a host timestamp. Journaling is best-effort and additive —
the `--json` stream is unchanged.

**Limit:** the journal is written by the _live_ drive, so it captures what that
drive saw. If you kill the driving process itself, anything the agent reports
afterwards — including a report the guest's auto-nudges land later — is not
journaled. Losing the **output** is fully covered; losing the **process** is
not.

**Recovering a launch that lost its directive.** A prompted launch
(`sandbox start --agent --prompt …`, and every `sandbox dispatch`) writes its
composed directive to `directive.md` in the instance's state dir, then boots the
VM **detached** before delivering it. So if the launching process dies in
between — killed, terminal closed, orchestrator gone — you are left with a
healthy, idle VM that never received its instructions. Don't recompose it by
hand:

```sh
sandbox prompt <name> --redeliver     # re-send the persisted directive as a fresh turn
```

It takes no text (passing both is an error) and delivers verbatim, with normal
turn-id and delivery-ack semantics. If there is no directive — the instance was
launched without a `--prompt` — the command says so and tells you to send the
text explicitly. The file's lifetime follows the state dir: an ephemeral
instance's goes when its state is reaped, a named instance's survives
stop/start.

**Prompting a paused instance auto-starts it.** `sandbox stop <name>` on a named
instance _pauses_ it: the VM powers off but its state dir (and branch) are kept.
If you `sandbox prompt` a paused instance, the command restarts it for you —
booting and arming the channel (~30–60s) before delivering the turn — rather
than hanging against the dead VM. The catch: a pause wipes the VM's RAM, so the
live conversation **does not** survive it; only the committed branch does. The
resumed agent is a fresh session reading its branch, _not_ a continuation of the
pre-pause context — so write the prompt to stand on its own (point at the branch
state, not "as we discussed"). Poking a still-running instance keeps the
same-conversation behavior above.

When the work is finished, tell the agent it's done — it powers the VM off
itself — or stop it from the host (below).

## Collecting & integrating work

Work returns as ordinary git: the agent commits on `sandbox/<name>` and pushes
to a per-instance mirror. The channel carries control/status only — the branch
is the artifact.

A guest never writes the host's board. `project/kanban/` (`BOARD.md` and the
card notes) is host-only state, and the host is its single writer. A dispatched
agent gets its card as prose, returns code as this branch, and returns findings
over the `report` channel — never by editing a card note. Never write a
directive that tells an agent to record a finding into the board; the host
writes it down.

```sh
sandbox fetch <name>            # git fetch <mirror> +sandbox/<name>:refs/remotes/sandbox-guest/<name>
```

The fetch force-updates the `sandbox-guest/<name>` remote bookmark to the
guest's tip. Writing to `refs/remotes/` rather than `refs/heads/` means jj
imports it as a _remote_ bookmark: a remote bookmark moving never rewrites local
history, so force-updating it cannot orphan host commits regardless of what the
host has built on top. The leading `+` (force) keeps refetching idempotent at
the ref level — a second fetch of the same instance updates the pointer rather
than failing non-fast-forward. For instances first fetched with this version,
that idempotency also holds at the repository level (no abandoned commits, no
rebase surprises on a review bounce). Instances migrated from an older fetch
scheme may have a stale `refs/heads/sandbox/<name>` ref left behind; the guard
below handles it correctly and it is safe to delete (see Bounce, below). A
colocated jj repo imports the ref automatically: jj reads `refs/remotes/*`
alongside `refs/heads/*` and `refs/tags/*`.

`sandbox fetch` also guards against the off-script case: if any local
`refs/heads/` branch descends from the current ref tip (evidence that the host
used an old rebase-based landing workflow), the fetch refuses rather than
silently orphaning host work.

`sandbox fetch` brings the branch into your repo but **never merges**.
Integration is yours to drive, and the goal is to land the work as automatically
as a built-in sub-agent would — pausing only on a genuine dead-end. A sandbox is
meant to be a _more secure_ substitute for sub-agent spawning, so the back-half
should feel just as hands-off.

### Change integration

When an agent reports `done`, integrate **without asking**. The sandbox already
bounded the _execution_; the safety net for the _diff_ is that everything you
land stays revertable — the fetched `sandbox/<name>` branch is preserved — not a
pre-merge prompt.

Speak the user's VCS tool: `.jj/` present → use `jj`; else `.git` → `git`; if
neither or it's ambiguous, ask. The sync layer is always git (the mirror +
`sandbox fetch`), but the host-side landing is done in their tool.

**Land a single branch:**

1. `sandbox fetch <name>`.
2. **Snapshot the host first.** If the working copy is dirty, capture it as a
   `wip: …` commit (jj: the working copy already _is_ a commit; git: commit the
   dirty tree) — never a stash. Concurrent host edits must survive the landing.
3. **Duplicate** the guest commits onto the current tip of your work. Do **not**
   rebase: the guest branch is a remote bookmark, and jj refuses `jj rebase` on
   an immutable remote bookmark by default. Instead:
   - jj: `jj duplicate <name>@sandbox-guest -d @` — copies the guest commits
     with new identities, leaving the remote bookmark untouched.
   - git: `git cherry-pick sandbox-guest/<name>` — copies the diff without
     moving any ref. (`sandbox-guest/<name>` is git's disambiguation form for
     `refs/remotes/sandbox-guest/<name>`; `<name>@sandbox-guest` is jj notation
     and is rejected by git.) Duplicating (never rebasing) guarantees the remote
     bookmark always points only at guest history, so every future force-update
     is safe.
4. **Clean → land it, then remove the sandbox.** In `jj`, advance the
   working-copy pointer `@` onto the duplicated commits and leave bookmark
   placement to the user — anchoring accepted work on `@` keeps it durable
   across the git imports the sandbox commands trigger. In `git`, fast-forward
   your branch onto the cherry-picked commits. Either way, confirm the files
   materialize in the working copy, then run `sandbox stop --remove <name>` —
   the instance's unit of work is accepted, so it's spent (a plain
   `sandbox stop` removes an ephemeral instance; `--remove` also tears down a
   named one). Keep the `sandbox-guest/<name>` remote bookmark as the revert
   artifact, and surface the agent's `done` summary plus a diffstat of what
   landed — that digest is the orchestrator's "return value".
5. **Doesn't land cleanly →** treat the reconciliation as ordinary delegated
   work, not a special case (below).

**Bounce (review turn 2+):** When you send the agent back for changes after
landing its first commit, the agent pushes its follow-up onto the _original_
guest commit — its mirror is frozen at launch and doesn't see the host's rebased
copy. On `sandbox fetch <name>` a second time the remote bookmark simply
advances to the new tip; nothing is orphaned because the host never built on
that bookmark. For instances migrated from an older fetch scheme, the first
post-upgrade fetch creates `refs/remotes/sandbox-guest/<name>` while leaving a
stale `refs/heads/sandbox/<name>` behind — that debris ref is filtered out by
the strict-descent guard and does not block re-fetching; delete it when
convenient: `git branch -D sandbox/<name>`. To land _only_ the new commits
(since your last landing), identify the boundary and duplicate from there:

- jj: `jj duplicate <new-tip>@sandbox-guest~<n>..<new-tip>@sandbox-guest -d @`
  (where `<n>` is the count of new commits since the last landing, so the range
  excludes the ones you already duplicated).
- git: `git cherry-pick <last-landed-guest-sha>..<new-tip>` (the `..` range
  excludes the left boundary, skipping previously-landed commits).

Never `jj rebase` the `sandbox-guest/<name>` bookmark. jj's immutability default
refuses it on a remote bookmark, and even if overridden it would move the
bookmark and break the idempotency the design relies on.

### Conflict reconciliation

Reconciling a conflict is nothing special — it's ordinary work you delegate to a
sandbox, exactly like the original task. Spawn one, brief it well (the original
directive, the prior agent's `done` summary, which files conflict, and the goal:
"rebase this onto HEAD, resolve preserving both intents, commit and push"), then
collect and land its branch by **this same procedure** — recursively, if its own
result conflicts.

Every normal delegation behavior applies unchanged: it works the task, `report`s
`done` or `blocked`, you answer a `blocked` by relaying it to the user and
sending the reply with `sandbox prompt`, and you involve the user directly only
when the agent truly can't proceed. There is no conflict-specific role, ceiling,
or path.

One general gotcha — true of any delegated follow-up, not just conflicts: spawn
a **fresh** instance so its mirror clones the repo _as it is now_; it then sees
both the current HEAD and the fetched branch. A resumed named instance keeps its
mirror frozen at _its_ launch and can't see a newer HEAD.

### Parallel fan-out

Fan several tasks out by giving each its own sub-agent: in a single batch, spawn
one sub-agent per task and have each launch its `--name`d VM, drive it to
`done`, and return its branch name plus the agent's `done` summary. The launches
then run concurrently through the same parallel-sub-agent loop you already use
for non-sandboxed work, and each VM's drive loop stays in its own context. Each
sandbox is an independent VM with its own branch. Drive a lone sandbox directly
— the extra sub-agent layer earns its keep once you fan out.

Keep integration in the orchestrator and run it serially: as each sub-agent
returns, land that one branch so the working tip advances and the next rebases
onto it. Single-threading the one shared working copy this way keeps the
landings clean, and a sub-agent that hits a `blocked` relays it back so you can
surface it to the user. Scope each fanned-out task to disjoint files when you
write the directives and most landings stay fast-forwards. (A later branch may
still land on accumulated work and need a follow-up sandbox, exactly as above.)

## Observing & lifecycle

```sh
sandbox status                  # list instances: #, state, mode, persist, graphics rung, liveness, work state
sandbox status <name|#>         # detail, incl. the ssh command to watch live
sandbox attach <name|#>         # ssh in + attach the agent's tmux session live
sandbox screenshot <name|#> [path] # PNG of the headless-sway output (graphics opt-in; "-" = stdout)
sandbox stop [--remove] <name|#> # stop (and remove a named instance with --remove)
```

`sandbox status` numbers every instance in a `#` column. That index is an
alternative to the full suffixed name for **every** instance-taking command
(`prompt`, `status`, `attach`, `fetch`, `screenshot`, `stop`) — handy
interactively, but positional, so it can shift as instances appear or disappear;
re-check `sandbox status` before trusting a number across a change.

To watch the agent work live, run `sandbox attach <name|#>` — it SSHes in, pins
`TERM=xterm-256color` (so terminals like ghostty don't trip up the guest's
`tmux`), and attaches the running `katsuobushi` tmux session.
`sandbox status <name>` still prints the raw ssh command if you need it.

**Which log, and when.** A launch has two phases with two different logs, and
reaching for the wrong one is how a healthy launch gets mistaken for a hang:

| Phase                                                         | State reported          | Log                                 |
| ------------------------------------------------------------- | ----------------------- | ----------------------------------- |
| **Pre-boot** — mirror clone, nix DB snapshot, context/secrets | `provisioning (<step>)` | `provision.log` in the state dir    |
| **Booted** — the VM itself                                    | `running`               | `console.log` (teed serial console) |

`console.log` does not exist until QEMU starts, so during provisioning it is
`provision.log` you want — it carries a `::: <step>` / `::: done (Ns)` marker
pair for every step, written the moment it happens. `sandbox status <inst>`
names the step in flight and points at the file. **A launch that sits in one
provisioning step for minutes is usually working, not stuck** — a cold nix DB
snapshot or mirror clone is genuinely slow. Read the log before intervening, and
never kill a provisioning step.

`sandbox status <inst>` also reports a `store db:` line once the guest has
booted. `system-only` there means the guest did **not** get the host's Nix
database — every host-built path on the shared store is invalid inside the VM,
so the agent will fail to run gates. Relaunch rather than burning a session on
it.

**Reading the work state.** The `WORK` column in `sandbox status` (and
`work state:` in the detail view) tells you what the agent is doing without
attaching:

| Reading    | Meaning                                                                                                                                                                                                                                                               | What to do                                                                                              |
| ---------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| `Active`   | At least one heartbeat is beating; the agent is working. When a heartbeat exceeds its `timeout`, the display shows `Active (Late)` — a flag on this state, not a separate state. A past-timeout heartbeat is still beating; one that stops shows as `Idle`, not late. | Wait. If `(Late)`, look at the heartbeat's label; decide whether to wait or attach and inspect.         |
| `Finished` | The agent filed a terminal report (`done`/`blocked`).                                                                                                                                                                                                                 | Collect the report from `sandbox status <name>` (`last report:`) or `reports.ndjson`.                   |
| `Idle`     | No heartbeat beats and no terminal report was filed.                                                                                                                                                                                                                  | The sandbox auto-nudges an idle agent. If `Idle` persists, attach and inspect, or stop and re-dispatch. |

Unnamed instances are ephemeral (removed on stop). `--name` makes an instance
persistent: it keeps its branch. A provided `--name` is suffixed with random
entropy at launch (e.g. `--name build` → `build-a3f9c2d1`) so each launch is a
fresh, collision-free instance — never a silent resume of an older same-named
branch. Drive the instance (prompt/status/fetch/stop) by the full suffixed name
that launch prints; relaunch with that full name to resume the agent's
accumulated work.

A persistent instance is kept only while its work is still in flight. Once that
unit of work is **complete and accepted** — its branch landed, or otherwise
signed off — the instance is spent: remove it with
`sandbox stop --remove <name>`. Don't leave accepted sandboxes lingering; the
`sandbox/<name>` ref is the durable artifact, not the VM.

## Authoring heartbeats

Heartbeat files live at `.katsuobushi/heartbeats/` in the workspace repo —
version-controlled and seeded to the VM through the ordinary git clone. Every
heartbeat appears in the diff before dispatch; an agent can write the file, but
it cannot do so quietly — a reviewer sees it.

Each file is YAML with five fields:

| Field      | Required | Description                                                             |
| ---------- | -------- | ----------------------------------------------------------------------- |
| `label`    | yes      | Short phrase shown in the `WORK` column (`Compiling`, `Running tests`)  |
| `timeout`  | yes      | How long an unbroken beat run may last before `Active (Late)` is raised |
| `check`    | yes      | Shell body — exits 0 to beat, anything else to miss                     |
| `interval` | no       | Polling cadence; defaults to `10s` when absent                          |
| `detail`   | no       | Optional shell body; first stdout line annotates the status line        |

Duration values are `<n>s` (seconds) or `<n>m` (minutes) — no other units.

Rules to write against:

- **The check runs as the agent user, with the workspace as its working
  directory.**
- **A check that outlives its interval is killed — and does not beat.** The
  entire process group is killed (shell plus any spawned children), so a slow
  pipeline leaves no orphans. Write checks that finish well inside the interval.
- **Duration counts from the first beat of an _unbroken_ run.** A single miss
  resets the clock. Write a check that does not flicker — a momentary non-zero
  exit during what should be continuous activity resets the duration and makes
  `Active (Late)` fire prematurely.
- **A check on a process group survives a single process exit; a check on one
  process identifier does not.** Use `pgrep -f` or similar for work that may
  span multiple processes or restart a child.
- **Project heartbeats add to the shipped set; they do not replace it.** A
  project heartbeat with the same label as a shipped heartbeat does not override
  it — both run, but the work state holds one slot per label, so whichever
  update arrives last masks the other. Use a distinct label so both results are
  visible.

**Block-scalar syntax.** The `check` and `detail` bodies use YAML block-scalar
style: a pipe (`|`) on the field line and every shell line indented two spaces
relative to the `check:` key. The YAML parser strips those two leading spaces
before handing the body to `sh(1)`. That indentation is YAML structure, not part
of the shell script. Use `|` (literal), not `>` (folded) — a folded body
collapses newlines into spaces and turns a multi-line script into a one-liner
that silently exits 0 and beats on every tick regardless of what the work is
actually doing.

**Comparison-type checks and the first-beat problem.** Any check that compares a
current reading against a previous one — CPU usage, a file mtime, a counter —
has no previous reading on the first run. Without handling that case, the check
exits 0 and fires a spurious beat before any real work has happened. The
pattern: on the first run, write the baseline and exit 1 (no beat). On the next
tick, compare and exit 0 only when the value has advanced.

A process-existence check (`pgrep -f`) avoids this problem by nature — the
process either exists or it does not. Use the baseline pattern for any check
that measures change rather than presence:

```yaml
label: Compiling
timeout: 45m
interval: 10s
check: |
  prev=/run/katsuobushi/control/hb-build.prev
  cur=$(stat -c %Y target/debug/my-binary 2>/dev/null) || exit 1
  if [ -f "$prev" ]; then
    old=$(cat "$prev")
    printf '%s\n' "$cur" > "$prev"
    [ "$cur" -gt "$old" ]
  else
    printf '%s\n' "$cur" > "$prev"
    exit 1
  fi
```

A process-existence check that does not need the baseline:

```yaml
label: Compiling
timeout: 45m
interval: 10s
check: |
  pgrep -f 'rustc|cargo' >/dev/null
```

A heartbeat file that will not parse reports one error for that path, then is
silently skipped on every subsequent tick while it stays broken; the other files
are unaffected.

## Notes

- One serial session per VM: reports answer prompts in order. `done`/`blocked`
  are the signals to act on; the pushed branch is the deliverable.
- Agent mode relies on Claude Code's experimental "channels" feature; if a
  launch never arms the channel, check `provision.log` (during provisioning) or
  `console.log` (after boot) — see "Which log, and when" above. For a running
  agent that looks stuck, check the `WORK` column of `sandbox status` rather
  than the log; `console.log` stops at the login prompt and says nothing about
  agent runtime.
- Treat the OAuth token as a live credential; it stays on subscription billing.
