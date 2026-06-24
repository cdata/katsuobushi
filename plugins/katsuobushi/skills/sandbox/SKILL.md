---
name: sandbox
description: Run agent work inside an ephemeral, network-restricted Katsuobushi sandbox VM with a bounded blast radius, and orchestrate it from the host. Use this skill when the user wants to "use the sandbox to…", delegate a task to a sandbox or VM, run risky / long-running / parallel work in isolation, spin up an agent-mode sandbox, push prompts to a running sandbox instance, check on or fetch a sandbox's work, or stop one — i.e. anything involving the sandbox:start / sandbox:prompt / sandbox:status / sandbox:fetch / sandbox:stop commands or `nix run .#sandbox`.
---

# Driving a Katsuobushi sandbox

A Katsuobushi sandbox is a hermetic `microvm.nix` guest (a real NixOS VM under
QEMU). An agent harness runs inside it with its blast radius bounded by the VM:
default-deny network behind an HTTPS allowlist, no host access, unprivileged.
Work returns as a pushed git branch.

You are the **host orchestrator**. You launch a sandbox, push it prompts over a
private host↔guest channel (the *sandbox controller*), read its status reports,
and collect its branch. The full human guide is at
<https://github.com/cdata/katsuobushi/blob/main/lib/sandbox/README.md>.

## When to use this

Delegate to a sandbox when work should be **isolated** or run **unattended /
in parallel**: risky refactors, running untrusted or experimental code, letting
an agent grind on a task with auto-approved tool use, or fanning out several
tasks at once. Each sandbox is an independent VM with its own branch.

Do **not** reach for it for quick edits in the current repo — it's for bounded,
delegated work.

## Prerequisites

**Run `sandbox:status` first — it is the preflight.** Before it lists instances
it prints an `environment:` block and exits non-zero if anything is missing, so
a single command tells you whether the host is ready. No project-specific
knowledge required — read what to fix off its output:

- If the command itself is **not found**, the `sandbox:*` tooling isn't on
  `PATH` — run inside `nix develop`, or launch via `nix run .#sandbox -- …`.
- Every `environment:` row should read `ok`. A `MISSING` row names exactly what
  to act on:
  - The **OAuth token** row names the *host* environment variable this project
    maps the guest's `CLAUDE_CODE_OAUTH_TOKEN` from. It is often
    `CLAUDE_CODE_OAUTH_TOKEN`, but can be any name (e.g. `HARNESS_OAUTH_TOKEN`) —
    don't assume it; read it off the status output. **You usually cannot export
    it yourself** — ask the user to export the named variable (via
    `claude setup-token`) and re-run `sandbox:status`, or to launch the VM, then
    take over driving.
  - A `vhost-vsock` `MISSING` row means agent mode's host↔guest channel is
    unavailable — the user loads it with `sudo modprobe vhost_vsock`.

## Configuring a project's sandbox

If a project doesn't yet expose the `sandbox:*` commands, wire the library into
its flake. The call lives in the per-system outputs (alongside `apps.sandbox` /
`checks.sandbox` and the dev-shell menu; see `templates/sandbox/flake.nix` in
the katsuobushi repo for the full flake). A comprehensive call exercising every
consumer-facing argument:

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
  #
  # guestModules = [ ./guest-extra.nix ];
};
```

`llm-agents` / `rust-overlay-src` / `nixos-config` are flake inputs the project
declares; `system` comes from `flake-utils`. The internal `microvm` / `rust` /
`controlSrc` arguments are supplied by Katsuobushi — consumers don't set them.
The fastest starting point is `nix flake init -t github:cdata/katsuobushi#sandbox`.

## Launching (agent mode)

```sh
# Boot a lingering agent VM; returns immediately once it's up.
nix run .#sandbox -- --agent --name <name>

# …or boot AND send the first directive, streaming reports until done/blocked:
nix run .#sandbox -- --agent --name <name> --prompt "<directive>"
```

(Inside the project's dev shell, `sandbox:start --agent …` is the equivalent
alias for `nix run .#sandbox -- --agent …`.)

Agent-mode VMs **linger** (they outlive the launch command). A dormant Claude
session runs inside the VM with the controller armed. After a no-`--prompt`
launch, the VM still needs ~30–60s to finish booting and arm the channel before
it will answer — if `sandbox:prompt` can't connect, wait and retry.

Give a directive that says how to finish, e.g.:
*"Do X. Commit and push on the branch. Run `report done \"<summary>\"` when
complete; `report blocked \"<what you need>\"` if you get stuck."*

## Driving it (multi-turn)

```sh
sandbox:prompt <name> "<the next directive>"
```

Each prompt is the next turn in the **same** conversation — context is retained
across pokes. Iterate: "do X" → done → "now Y" → done → "finish up". The command
streams the agent's status lines and returns when the agent reports a terminal
status:

- `working` — progress (optional, non-terminal).
- `done` — the turn is complete; the work product is the pushed branch.
- `blocked` — it needs something; address it and send the next prompt.
- `info` — anything else worth surfacing.

When the work is finished, tell the agent it's done — it powers the VM off
itself — or stop it from the host (below).

## Collecting work

Work returns as ordinary git — the agent commits on `sandbox/<name>` and pushes
to a per-instance mirror. Pull it into the repo:

```sh
sandbox:fetch <name>          # fetches branch sandbox/<name>
```

The channel never carries code; the branch is the artifact. Review it as a
normal branch (diff, test) before merging.

## Observing & lifecycle

```sh
sandbox:status                # list instances; running/stopped, agent CID, branch
sandbox:status <name>         # detail, incl. the ssh command to watch live
sandbox:stop [--remove] <name>  # stop (and remove a named instance with --remove)
```

To watch the agent work live, attach to its session over the ssh command that
`sandbox:status <name>` prints (it runs `tmux attach -t katsuobushi` in the VM).
The serial console is teed to `console.log` in the instance's state dir — read
it to diagnose a stuck boot.

Unnamed instances are ephemeral (removed on stop). `--name` makes an instance
persistent: it keeps its branch and can be restarted by launching with the same
name, resuming the agent's accumulated work.

## Notes

- One serial session per VM: reports answer prompts in order. `done`/`blocked`
  are the signals to act on; the pushed branch is the deliverable.
- Agent mode relies on Claude Code's experimental "channels" feature; if a
  launch never arms the channel, check `console.log` and `sandbox:status`.
- Treat the OAuth token as a live credential; it stays on subscription billing.
