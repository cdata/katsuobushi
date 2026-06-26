---
name: sandbox
description:
  Run agent work inside an ephemeral, network-restricted Katsuobushi sandbox VM
  with a bounded blast radius, and orchestrate it from the host. Use this skill
  when the user wants to "use the sandbox to…", delegate a task to a sandbox or
  VM, run risky / long-running / parallel work in isolation, spin up an
  agent-mode sandbox, push prompts to a running sandbox instance, check on or
  fetch a sandbox's work, attach to a running sandbox's live session, or stop
  one — i.e. anything involving the sandbox:start / sandbox:prompt /
  sandbox:status / sandbox:attach / sandbox:fetch / sandbox:stop commands or
  `nix run .#sandbox`.
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

**Run `sandbox:status` to get your bearings** before you do anything else.

It may reveal the following problems:

- Missing command; if `sandbox:status` is not available, then you probably need
  to drop into the Nix dev shell. `nix develop -c sandbox:status` should work if
  you are in a folder with a viable dev shell.
- Missing environment variables; if any are detected as missing, share which
  ones with the user and ask them to export them by name (give them a helpful
  example).
- Missing `vhost-vsock`; if `qemu` is not compiled with this feature, or if the
  kernel module is not available you won't be able to communicate with the
  sandboxed agent; the user may need to load it with `sudo modprobe vhost_vsock`
  (although this probably isnt the "fix" on a NixOS system).

## Configuring a project's sandbox

If a project doesn't yet expose the `sandbox:*` commands, offer to wire the
library into the local flake. The call lives in the per-system outputs
(alongside `apps.sandbox` / `checks.sandbox` and the dev-shell menu; see
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
  # named var (e.g. "HARNESS_OAUTH_TOKEN"). `sandbox:status` reports which.
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

  # Resources
  vcpu = 4;
  mem = 8192;                          # MiB — avoid exactly 2048 (QEMU hangs)
  storeOverlaySize = "8G";             # tmpfs writable /nix/store overlay

  # Escape hatch: extra NixOS modules merged into the guest
  guestModules = [ ./guest-extra.nix ];
};
```

`llm-agents` / `rust-overlay-src` / `nixos-config` are flake inputs the project
declares; `system` comes from `flake-utils`. The internal `microvm` / `rust` /
`controlSrc` arguments are supplied by Katsuobushi — consumers don't set them.
The fastest starting point is
`nix flake init -t github:cdata/katsuobushi#sandbox`.

## Launch a sandboxed agent

```sh
# Boot a lingering agent VM; returns immediately once it's up.
sandbox:start --agent --name "<name>"

# …or boot AND send the first directive, streaming reports until done/blocked:
sandbox:start --agent --name "<name>" --prompt "<directive>"

# Alternatively for debugging you can invoke the sandbox binary directly e.g.,
# nix run .#sandbox -- --agent --name <name>
```

Agent-mode VMs **linger** (they outlive the launch command). A dormant Claude
session runs inside the VM with the controller armed. After a no-`--prompt`
launch, the VM still needs ~30–60s to finish booting and arm the channel before
it will answer — if `sandbox:prompt` can't connect, wait and retry.

Give a directive that says how to finish, e.g.: _"Do X. Commit and push on the
branch. Run `report done \"<summary>\"` when complete;
`report blocked \"<what you need>\"` if you get stuck."_

## Driving the agent

```sh
sandbox:prompt <name> "<the next directive>"
```

Each prompt to a **running** instance is the next turn in the **same**
conversation — context is retained across pokes. Iterate: "do X" → done → "now
Y" → done → "finish up". The command streams the agent's status lines and
returns when the agent reports a terminal status:

- `working` — progress (optional, non-terminal).
- `done` — the turn is complete; the work product is the pushed branch.
- `blocked` — it needs something; address it and send the next prompt.
- `info` — anything else worth surfacing.

**Prompting a paused instance auto-starts it.** `sandbox:stop <name>` on a named
instance _pauses_ it: the VM powers off but its state dir (and branch) are kept.
If you `sandbox:prompt` a paused instance, the command restarts it for you —
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

```sh
sandbox:fetch <name>            # git fetch <mirror> sandbox/<name>:sandbox/<name>
```

`sandbox:fetch` brings the branch into your repo but **never merges**.
Integration is yours to drive, and the goal is to land the work as automatically
as a built-in sub-agent would — pausing only on a genuine dead-end. A sandbox is
meant to be a _more secure_ substitute for sub-agent spawning, so the back-half
should feel just as hands-off.

### Change integration

When an agent reports `done`, integrate **without asking**. The sandbox already
bounded the _execution_; the safety net for the _diff_ is that everything you
land stays revertable — the `sandbox/<name>` ref is preserved — not a pre-merge
prompt.

Speak the user's VCS tool: `.jj/` present → use `jj`; else `.git` → `git`; if
neither or it's ambiguous, ask. The sync layer is always git (the mirror +
`sandbox:fetch`), but the host-side landing is done in their tool.

**Land a single branch via rebase workflow:**

1. `sandbox:fetch <name>`.
2. **Snapshot the host first.** If the working copy is dirty, capture it as a
   `wip: …` commit (jj: the working copy already _is_ a commit; git: commit the
   dirty tree) — never a stash. Concurrent host edits must survive the landing.
3. **Rebase** the sandbox commits onto the current tip of your work (`jj rebase`
   / `git rebase`).
4. **Clean → land it, then remove the sandbox.** In `jj`, advance the
   working-copy pointer `@` onto the rebased commits (`jj new <tip>`) and leave
   bookmark placement to the user — anchoring accepted work on `@` keeps it
   durable across the git imports the sandbox commands trigger. In `git`,
   fast-forward your branch onto the landed commits. Either way, confirm the
   files materialize in the working copy, then run
   `sandbox:stop --remove <name>` — the instance's unit of work is accepted, so
   it's spent (a plain `sandbox:stop` removes an ephemeral instance; `--remove`
   also tears down a named one). Keep the `sandbox/<name>` ref as the revert
   artifact, and surface the agent's `done` summary plus a diffstat of what
   landed — that digest is the orchestrator's "return value".
5. **Doesn't land cleanly →** treat the reconciliation as ordinary delegated
   work, not a special case (below).

### Conflict reconciliation

Reconciling a conflict is nothing special — it's ordinary work you delegate to a
sandbox, exactly like the original task. Spawn one, brief it well (the original
directive, the prior agent's `done` summary, which files conflict, and the goal:
"rebase this onto HEAD, resolve preserving both intents, commit and push"), then
collect and land its branch by **this same procedure** — recursively, if its own
result conflicts.

Every normal delegation behavior applies unchanged: it works the task, `report`s
`done` or `blocked`, you answer a `blocked` by relaying it to the user and
sending the reply with `sandbox:prompt`, and you involve the user directly only
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
sandbox:status                  # list instances; numbered, running/stopped, agent CID, branch
sandbox:status <name|#>         # detail, incl. the ssh command to watch live
sandbox:attach <name|#>         # ssh in + attach the agent's tmux session live
sandbox:stop [--remove] <name|#> # stop (and remove a named instance with --remove)
```

`sandbox:status` numbers every instance in a `#` column. That index is an
alternative to the full suffixed name for **every** instance-taking command
(`prompt`, `status`, `attach`, `fetch`, `stop`) — handy interactively, but
positional, so it can shift as instances appear or disappear; re-check
`sandbox:status` before trusting a number across a change.

To watch the agent work live, run `sandbox:attach <name|#>` — it SSHes in, pins
`TERM=xterm-256color` (so terminals like ghostty don't trip up the guest's
`tmux`), and attaches the running `katsuobushi` tmux session.
`sandbox:status <name>` still prints the raw ssh command if you need it. The
serial console is teed to `console.log` in the instance's state dir — read it to
diagnose a stuck boot.

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
`sandbox:stop --remove <name>`. Don't leave accepted sandboxes lingering; the
`sandbox/<name>` ref is the durable artifact, not the VM.

## Notes

- One serial session per VM: reports answer prompts in order. `done`/`blocked`
  are the signals to act on; the pushed branch is the deliverable.
- Agent mode relies on Claude Code's experimental "channels" feature; if a
  launch never arms the channel, check `console.log` and `sandbox:status`.
- Treat the OAuth token as a live credential; it stays on subscription billing.
