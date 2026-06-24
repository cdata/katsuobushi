# Katsuobushi Sandbox

`katsuobushi.lib.sandbox` boots a hermetic [`microvm.nix`][microvm] guest — a
real NixOS system under QEMU with a genuine kernel boundary — that comes up as a
working local dev environment in which an **agent harness (e.g. Claude Code) can
run with its blast radius bounded by the VM** rather than by host permission
prompts.

Inside the VM the agent gets a working clone of your project, a writable Nix
store overlay, and a default-deny network where the _only_ way out is an HTTPS
proxy restricted to an allowlist you control. It cannot touch the host, other
projects, or the open internet. When it is done, it returns work the ordinary
way: it pushes a git branch.

There are two ways to drive it:

- **Interactive** — you `ssh` in and use the agent (or a shell) by hand.
- **Agent mode** — a _dormant_ Claude session sits inside the VM and you, the
  host operator (a human, or an orchestrating agent), push it prompts over a
  private host↔guest channel and watch status come back. This is the
  channel-driven _sandbox controller_.

> Agent mode drives a long-lived **interactive** session, so it stays on
> subscription billing — unlike a headless `claude -p`, which is moving toward
> requiring API-key billing.

## Host requirements

- **Linux with KVM** (`/dev/kvm`). The guest is a Linux microvm, so the sandbox
  app and checks are Linux-only.
- **Agent mode also needs vsock**: the host `vhost_vsock` kernel module
  (`/dev/vhost-vsock`). Load it with `sudo modprobe vhost_vsock` if absent;
  `sandbox:status` flags it and the runner warns at launch when it is missing.
- Nix with flakes.

## Quick start

The fastest path is the template:

```sh
nix flake init -t github:cdata/katsuobushi#sandbox
```

That gives you a flake that calls `katsuobushi.lib.sandbox` and wires the
lifecycle commands into a dev-shell menu. Edit `projectId`, the network
allowlist, and the packages (your agent harness), then:

```sh
# Generate a subscription token on the host and export it (see "Auth", below).
export CLAUDE_CODE_OAUTH_TOKEN="$(claude setup-token)"

nix develop          # drops you into the menu; `showMenu` lists commands
sandbox:start        # interactive: boots a VM and ssh's you in
```

To add the sandbox to an existing flake, call the library with your `pkgs` and a
project id; see
[`templates/sandbox/flake.nix`](../../templates/sandbox/flake.nix) for the
fully-commented reference. The most important arguments:

| Argument                                        | Purpose                                                                                                               |
| ----------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `workspaceRoot`                                 | Your project root (e.g. `./.`). Used to build the per-instance mirror at launch; not baked into the image.            |
| `projectId`                                     | Owner-qualified id (e.g. `"my-org/my-project"`). Names the in-guest path and per-instance host state dirs.            |
| `allowedOrigins`                                | Extra reachable hostnames, appended to the lean Anthropic+Nix baseline (`baseAllowedOrigins`). No implicit wildcards. |
| `packages`                                      | Goes on the guest `PATH` — **this is where your agent harness goes** (e.g. `claude-code`).                            |
| `secrets`                                       | `NAME -> { fromEnv \| fromFile }`. Read from the host at launch, injected via `fw_cfg`; never in the store.           |
| `extraRepos` / `workspaceContext` / `homeFiles` | Pin reference repos, carry untracked project context, and map files into the agent's home.                            |
| `vcpu` / `mem` / `storeOverlaySize`             | Resources (avoid `mem = 2048` exactly — QEMU hangs).                                                                  |

### A comprehensive example

The call lives in your flake's per-system outputs, alongside the dev-shell menu
and the `apps.sandbox` / `checks.sandbox` wiring (see
[`templates/sandbox/flake.nix`](../../templates/sandbox/flake.nix) for the full
flake). The library call itself, exercising every consumer-facing argument:

```nix
sandbox = katsuobushi.lib.sandbox {
  inherit pkgs;

  # Identity
  workspaceRoot = ./.;                 # project root; builds the per-instance mirror at launch
  projectId = "my-org/my-project";     # owner-qualified; names the in-guest path + host state dirs

  # Network egress (appended to the lean Anthropic+Nix baseline)
  #
  # Hostnames only, no implicit wildcards ("github.com" ≠ ".github.com").
  # HTTPS (443) is assumed; everything else is default-deny.
  allowedOrigins = [
    "crates.io"
    "static.crates.io"
    "index.crates.io"
  ];
  # To *remove* a baseline host there is no per-entry subtraction — override
  # the whole baseline instead:
  #   baseAllowedOrigins = [ "api.anthropic.com" "platform.claude.com" ];

  # Guest PATH: the agent harness + any tooling it needs
  #
  # The library ships no harness; you supply it like any other package.
  packages = [
    llm-agents.packages.${system}.claude-code   # or pkgs.claude-code (unfree)
    pkgs.cargo
    pkgs.ripgrep
  ];

  # Runtime secrets: read from the host at launch; never in the store
  #
  # The guest always sees CLAUDE_CODE_OAUTH_TOKEN; `fromEnv` chooses which *host*
  # variable supplies it. If an agent harness will launch the sandbox, source it
  # from a differently-named var — it scrubs CLAUDE_CODE_OAUTH_TOKEN from its
  # children (see "Auth"). `sandbox:status` reports which host var feeds each.
  secrets = {
    CLAUDE_CODE_OAUTH_TOKEN.fromEnv = "CLAUDE_CODE_OAUTH_TOKEN";
    # SOME_API_KEY.fromFile = "/run/secrets/some-api-key";
  };

  # Reference repos: build-time pinned, writable copies in the VM
  #
  # `source` is any store path (a `flake = false` input, or a fetcher like
  # pkgs.fetchFromGitHub). `dest` mirrors the ~/Git/<host>/<owner>/<repo>
  # layout. One-way; the repo's host need NOT be in allowedOrigins.
  extraRepos = [
    {
      source = rust-overlay-src;       # e.g. inputs.rust-overlay-src (flake = false)
      dest = "Git/github.com/oxalica/rust-overlay";
    }
  ];

  # Untracked project context carried into the workspace (host -> guest)
  #
  # Project-relative paths overlaid on the mirror clone. Absolute paths and ".."
  # are rejected at eval; symlinks escaping the tree are dropped at copy time.
  workspaceContext = [
    ".claude"                          # per-project Claude Code config (e.g. a pinned model)
    "notes"
  ];

  # Files mapped into the agent's home
  #
  # dest -> { source; path?; mode }; mode is one of:
  #   "immutable" (read-only bind mount) | "seed" (editable copy) | "link" (symlink)
  homeFiles = {
    ".claude/CLAUDE.md" = {
      source = nixos-config;           # e.g. inputs.nixos-config (flake = false)
      path = "AGENTS.md";              # optional subpath within source
      mode = "immutable";
    };
  };

  # Resources
  vcpu = 4;
  mem = 8192;                          # MiB — avoid exactly 2048 (QEMU hangs)
  storeOverlaySize = "8G";             # tmpfs writable /nix/store overlay; raise for heavy builds

  # Escape hatch: extra NixOS modules merged into the guest
  #
  guestModules = [ ./guest-extra.nix ];
};
```

`llm-agents`, `rust-overlay-src`, and `nixos-config` above are flake inputs you
declare; `system` comes from `flake-utils`. The internal `microvm` / `rust` /
`controlSrc` arguments are supplied by Katsuobushi and are not set by consumers.

## Auth

The agent harness inside the guest authenticates with a **subscription OAuth
token**, delivered as a runtime secret. The guest always reads it from
`CLAUDE_CODE_OAUTH_TOKEN`, but the **host** environment variable it is sourced
from is whatever your `secrets` config names via `fromEnv` — the two need not
share a name. With the template's default mapping
(`CLAUDE_CODE_OAUTH_TOKEN.fromEnv = "CLAUDE_CODE_OAUTH_TOKEN"`):

```sh
export CLAUDE_CODE_OAUTH_TOKEN="$(claude setup-token)"
```

> **Launching from inside an agent harness?** A harness like Claude Code scrubs
> `CLAUDE_CODE_OAUTH_TOKEN` from its own child environment before it finishes
> starting up, so an orchestrating agent cannot pass the token straight through
> under that name. Map the guest secret from a **differently-named** host
> variable instead, e.g.
> `secrets.CLAUDE_CODE_OAUTH_TOKEN.fromEnv = "HARNESS_OAUTH_TOKEN"`, and export
> `HARNESS_OAUTH_TOKEN` on the host.

You never have to guess the name: `sandbox:status` reports exactly which host
variable feeds each secret and flags any that is missing (see
[Checking your setup](#checking-your-setup)). The runner also fails fast at
launch if it is unset. The token is injected into the guest via `fw_cfg` into a
RAM-backed file — never written to the Nix store, argv, or disk.

## Checking your setup

A bare `sandbox:status` doubles as a preflight: before it lists instances it
prints an `environment:` block that verifies **each declared secret at its host
source** (the `fromEnv` variable is set, or the `fromFile` path is readable) and
checks for `/dev/vhost-vsock`. A clean run — every row `ok`, exit status `0` —
means a launch has what it needs; any `MISSING` row names exactly what to fix,
and the command exits non-zero. That makes "is this host ready?" a single check
with no project-specific knowledge required:

```text
environment:
  CLAUDE_CODE_OAUTH_TOKEN  ok (host env HARNESS_OAUTH_TOKEN is set)
  vhost-vsock              ok
```

## Interactive mode

```sh
sandbox:start                 # ephemeral instance, ssh attaches
sandbox:start --name work     # named (persistent) instance you can restart
```

You land in the project workspace with the agent harness on `PATH`. The VM is
torn down when you disconnect (named instances keep their branch for restart).

## Agent mode

```sh
# Boot a dormant agent VM that lingers; returns immediately.
nix run .#sandbox -- --agent --name task1
# …or send an initial directive and stream its reports:
nix run .#sandbox -- --agent --name task1 --prompt "Refactor X; commit on the branch; report done."
```

Agent-mode VMs **linger** (they outlive the launching process). A dormant Claude
session runs inside a detached tmux session with the _sandbox controller_ armed.
You drive it by pushing prompts; it works the directive, commits + pushes its
branch, and reports status back.

### Driving it

```sh
sandbox:prompt task1 "Now add tests for the new module, then report done."
```

Each prompt is the next turn in the **same** conversation — context is retained
across pokes, with no `--resume` plumbing. The host iterates: _"do X" → done →
"now Y" → done → "looks good, finish"_. `sandbox:prompt` streams the agent's
status lines until it reports `done` or `blocked`:

- `working` — progress (optional).
- `done` — the turn is complete; the work product is the pushed branch.
- `blocked` — it needs something; it then waits for your next directive.
- `info` — anything else worth surfacing.

### Watching it work

A real human can attach to the live session over ssh:

```sh
# `sandbox:status <instance>` prints the exact ssh command for an instance.
ssh … agent@127.0.0.1 -t 'tmux attach -t katsuobushi'
```

The serial console is also teed to `console.log` in the instance's state dir.

### Ending a session

The agent powers the VM off itself when you tell it you are finished (it runs
`systemctl poweroff`), or you stop it from the host with `sandbox:stop`.

## Getting work back

Work returns as **ordinary git**: the agent commits on `sandbox/<instance>` and
pushes to a per-instance bare mirror. Pull it into your repo with:

```sh
sandbox:fetch task1          # fetches sandbox/task1 into this repo
```

The channel only ever carries control + status — never code. The pushed branch
is the artifact.

## Lifecycle commands

| Command                                             | Description                                                                                        |
| --------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| `sandbox:start [--agent] [--prompt "…"] [--name N]` | Launch a VM (interactive, or lingering agent mode). Alias: `nix run .#sandbox -- …`.               |
| `sandbox:prompt <instance> "<text>"`                | Push a prompt to a running agent instance and stream its reports.                                  |
| `sandbox:status [instance]`                         | List instances (running/stopped, ephemeral/named), or detail one (ssh command, agent CID, branch). |
| `sandbox:fetch <instance>`                          | Fetch the instance's `sandbox/<instance>` branch into this repo.                                   |
| `sandbox:stop [--remove] <instance>`                | Stop a VM (and remove a named instance's state with `--remove`).                                   |

Unnamed instances are **ephemeral** (removed on stop); `--name` makes an
instance **persistent** — it keeps its branch. To keep names collision-free, a
provided `--name foo` is suffixed with random entropy at launch (e.g.
`foo-a3f9c2d1`), so every launch is a fresh instance rather than a silent resume
of an older same-named branch. The full suffixed name is printed at launch (and
by `sandbox:stop`); pass *that* full name to restart and resume the agent's
accumulated work.

## What the boundary enforces

- **Default-deny egress.** No general internet; DNS is disabled. The only way
  out is the HTTPS proxy, restricted to your allowlist and enforced _below_ the
  agent's privilege level (nftables, a dedicated proxy uid).
- **Unprivileged agent.** No root, no sudo; the firewall is a genuine boundary
  against it.
- **Kernel isolation.** A real VM — the agent cannot reach the host or other
  projects.
- **Nothing persists** beyond the branch it pushes and files written to the
  shared state dir.
- **Agent-mode control is host-only.** The controller channel rides vsock and is
  gated to the host CID, so the in-guest agent cannot inject prompts into its
  own session — only the host can. vsock bypasses the IP stack entirely, so it
  is invisible to the egress firewall and cannot be used for exfiltration.

## Caveats

- **Channels are a research preview.** Agent mode's inbound prompt injection
  rides Claude Code's experimental "channels" feature. The sandbox forces the
  settings it needs (`channelsEnabled`, the dev-channels load, the bypass-prompt
  skip) via guest config, and re-enables the feature-flag traffic that gates
  channel availability. If Anthropic changes or removes channels, agent mode is
  the part at risk — interactive mode is unaffected.
- **Billing.** Agent mode deliberately drives an interactive session to stay on
  subscription billing. Treat the token as a live credential.
- **Allowlist.** The baseline is intentionally lean (Anthropic inference + auth,
  Nix substituters, GitHub flake hosts). Add only what your project needs.

[microvm]: https://github.com/microvm-nix/microvm.nix
