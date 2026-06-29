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

| Argument                                                 | Purpose                                                                                                               |
| -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `workspaceRoot`                                          | Your project root (e.g. `./.`). Used to build the per-instance mirror at launch; not baked into the image.            |
| `projectId`                                              | Owner-qualified id (e.g. `"my-org/my-project"`). Names the in-guest path and per-instance host state dirs.            |
| `allowedOrigins`                                         | Extra reachable hostnames, appended to the lean Anthropic+Nix baseline (`baseAllowedOrigins`). No implicit wildcards. |
| `packages`                                               | Goes on the guest `PATH` — **this is where your agent harness goes** (e.g. `claude-code`).                            |
| `secrets`                                                | `NAME -> { fromEnv \| fromFile }`. Read from the host at launch, injected via `fw_cfg`; never in the store.           |
| `extraRepos` / `workspaceContext` / `homeFiles`          | Pin reference repos, carry untracked project context, and map files into the agent's home.                            |
| `importHostStoreDb`                                      | Default `true`. Reuse everything the host has already built (e.g. `nix develop` toolchains) offline; see below.       |
| `vcpu` / `mem`                                           | CPU + RAM (avoid `mem = 2048` exactly — QEMU hangs).                                                                  |
| `storeVolumeSize` / `scratchVolumeSize` / `dbVolumeSize` | Disk-backed scratch image sizes in MiB (sparse). Default 16384 / 32768 / 4096. See "Reusing the host's Nix store".    |

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
  storeVolumeSize = 16384;             # writable /nix/store overlay (MiB, sparse)
  scratchVolumeSize = 32768;           # workspace clone + cargo/rustup/XDG caches
  dbVolumeSize = 4096;                 # guest Nix database

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

Each prompt to a **running** instance is the next turn in the **same**
conversation — context is retained across pokes, with no `--resume` plumbing.
The host iterates: _"do X" → done → "now Y" → done → "looks good, finish"_.
`sandbox:prompt` streams the agent's status lines until it reports `done` or
`blocked`:

- `working` — progress (optional).
- `done` — the turn is complete; the work product is the pushed branch.
- `blocked` — it needs something; it then waits for your next directive.
- `info` — anything else worth surfacing.

If you prompt a **paused** instance (one stopped with `sandbox:stop` but kept
because it is named), `sandbox:prompt` restarts it for you — booting and arming
the channel (~30–60s) before delivering the turn — instead of hanging against
the powered-off VM. A pause discards the VM's RAM, so the live conversation does
not carry across it; only the pushed branch does. The restarted agent therefore
begins a fresh session on top of its branch rather than resuming the pre-pause
context, so phrase such a prompt to stand on its own.

### Watching it work

A real human can attach to the live agent session with one command:

```sh
sandbox:attach task1         # ssh in and attach the agent's tmux session
sandbox:attach 2             # …or reference it by its sandbox:status index
```

`sandbox:attach` SSHes into the instance, pins `TERM=xterm-256color` for the
remote session (so terminals like ghostty don't confuse the guest's `tmux`), and
attaches to the running `katsuobushi` tmux session. `sandbox:status <instance>`
still prints the raw ssh command if you want to build on it.

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

| Command                                             | Description                                                                                                   |
| --------------------------------------------------- | ------------------------------------------------------------------------------------------------------------- |
| `sandbox:start [--agent] [--prompt "…"] [--name N]` | Launch a VM (interactive, or lingering agent mode). Alias: `nix run .#sandbox -- …`.                          |
| `sandbox:prompt <instance\|#> "<text>"`             | Push a prompt to an agent instance and stream its reports; auto-starts a paused (stopped-but-kept) one first. |
| `sandbox:status [instance\|#]`                      | List instances (numbered, running/stopped, ephemeral/named), or detail one (ssh command, agent CID, branch).  |
| `sandbox:attach <instance\|#>`                      | SSH into a running instance and attach the agent's `tmux` session (`TERM=xterm-256color`).                    |
| `sandbox:fetch <instance\|#>`                       | Fetch the instance's `sandbox/<instance>` branch into this repo.                                              |
| `sandbox:screenshot <instance\|#> [path]`           | Grab a PNG of the headless-sway output (requires the graphics opt-in). Default: timestamped PNG in the cwd.   |
| `sandbox:stop [--remove] <instance\|#>`             | Stop a VM (and remove a named instance's state with `--remove`).                                              |

Every command that takes an `<instance>` also accepts the **index** shown in the
`#` column of `sandbox:status` — a convenience shorthand for the full suffixed
name. The numbering is positional over the current instance list, so it can
shift as instances come and go; re-run `sandbox:status` to see the current map.

Unnamed instances are **ephemeral** (removed on stop); `--name` makes an
instance **persistent** — it keeps its branch. To keep names collision-free, a
provided `--name foo` is suffixed with random entropy at launch (e.g.
`foo-a3f9c2d1`), so every launch is a fresh instance rather than a silent resume
of an older same-named branch. The full suffixed name is printed at launch (and
by `sandbox:stop`); pass _that_ full name to restart and resume the agent's
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

## Graphics (opt-in)

By default the guest is headless and has no GPU: a browser or Wayland app has
nothing to render against. The optional `graphics` capability boots a headless
[sway][sway] compositor on a virtual output and gives the guest a paravirtual
GPU, so a browser (for WebDriver/Playwright tests) or a Wayland app can actually
render. It is **off by default**; existing consumers are unaffected.

Enable it with a `graphics` attrset on the `katsuobushi.lib.sandbox` call. The
browser/app itself is an ordinary package — it goes in the existing `packages`
list, not a graphics-specific channel:

```nix
sandbox = katsuobushi.lib.sandbox {
  inherit pkgs;
  # ... existing args ...

  graphics = {
    enable = true;                                # default false; opt-in

    # Host GPU selection: a role preference, resolved at launch. The first role
    # that is present and openable wins; "software" is the llvmpipe tail.
    gpu = [ "integrated" "discrete" "software" ]; # default

    # The headless sway virtual output.
    output = { width = 1920; height = 1080; refresh = 60; };   # default
  };

  # The browser / app / engine goes in the EXISTING packages list:
  packages = [ pkgs.firefox pkgs.playwright-driver /* ... */ ];
};
```

The compositor is not a knob (it is always headless sway); reach for
`guestModules` if you need to go beyond this surface.

### What graphics changes about the boundary

This is the **one place** graphics dents the sandbox's headline guarantee, so it
is stated plainly. When a GPU rung is selected, the guest's GPU command stream
is parsed by **virglrenderer running inside the host QEMU process**, and
virglrenderer has a history of guest→host escape CVEs. So the
[boundary](#what-the-boundary-enforces) guarantee that "a real VM — the agent
cannot reach the host or other projects" becomes "…except via the GPU command
parser, if it has an exploitable bug."

This surface **does not exist** with graphics off (today's behavior), and it
**does not exist** on the `software` rung — there is no GPU device and no
command parser in the host process there. What still contains a GPU rung: QEMU
runs as your unprivileged launching uid (a successful escape lands with that
user's privileges, the same ceiling as any QEMU escape), and nothing new is
exposed to the network — virtio-gpu is a local device, the default-deny egress
firewall is untouched, and `sandbox:screenshot` rides ssh-over-loopback. The
delta is purely a host-_integrity_ surface, not a data-egress one. A launch-time
notice repeats this when you enable graphics, mirroring the
`vhost-vsock`-missing warning.

### The software escape valve

A project that cannot accept that delta pins:

```nix
graphics = {
  enable = true;
  gpu = [ "software" ];
};
```

That keeps the **full original boundary** — llvmpipe (CPU) rendering, no GPU
device handed to QEMU, no command parser in the host process — at a performance
cost. It is the recommended posture for any graphics-enabled instance running
untrusted code where host integrity outweighs frame rate.

### Resources — a recommended floor

A compositor and (especially) software rendering are CPU/RAM-hungry, and
venus/virgl add host GPU memory pressure. When graphics is enabled, raise the
resource arguments yourself — the library does **not** auto-bump them behind
your back:

- `vcpu ≥ 4` (llvmpipe is embarrassingly CPU-bound on the `software` rung).
- `mem ≥ 8192` MiB (browser + compositor) — and still avoid exactly `2048`, the
  QEMU hang noted under [Quick start](#quick-start).
- A larger `scratchVolumeSize` if the project caches large browser/engine
  assets.

### The `render`-group prerequisite

Opening a host render node (`/dev/dri/renderD128`) is the one host prerequisite.
The portable default for those nodes is `root:render 0660`, so the launching
user's uid may need to be in the `render` group before a GPU rung is usable. You
do not have to guess: when graphics is enabled, `sandbox:status` adds a
`graphics` preflight row that runs the real GPU resolver against the host now
and reports the outcome —

```text
environment:
  CLAUDE_CODE_OAUTH_TOKEN  ok (host env HARNESS_OAUTH_TOKEN is set)
  vhost-vsock              ok
  graphics                 ok (will render on integrated: /dev/dri/renderD128)
```

It resolves to `ok (will render on …)` when a GPU rung wins,
`ok (software fallback — no usable GPU)` when it falls to the `software` tail,
and — only when your `gpu` list has **no** `software` tail and no node is
openable —
`MISSING - no render node openable by uid <N>; add yourself to the 'render' group`,
which exits non-zero exactly like a missing secret.

### Grabbing a screenshot

```sh
sandbox:screenshot task1            # timestamped PNG in the cwd
sandbox:screenshot task1 shot.png   # to a path; "-" streams the PNG to stdout
```

`sandbox:screenshot` runs `grim` over the existing loopback ssh against the
headless-sway output and streams the PNG back — no daemon, no new port, no new
channel. It works in both interactive and agent mode, and requires the graphics
opt-in (with graphics off it fails with a clear "graphics not enabled" message
rather than a cryptic error).

One expected behavior to know: it captures **what is composited on the sway
output**. A workload that renders purely offscreen — to its own FBO or swapchain
— and never puts a surface on the compositor will **screenshot as blank**. That
is correct, not a bug: `grim` can only see what is on the output.

[sway]: https://swaywm.org/

## Disk-backed writable scratch

Everything the guest writes — the workspace clone with its build artifacts, the
relocated `cargo`/`rustup`/XDG caches, the writable `/nix/store` overlay, and
the guest Nix database — lives on **sparse disk images**, not RAM. (The guest
root `/` stays a tmpfs; only the scratch surfaces are disk-backed.) microvm's
default is a tmpfs for all of it, which caps writable space at a fraction of
`mem` and makes a single Rust `target/` able to OOM the VM. Three per-instance
raw images (`storeVolumeSize` / `scratchVolumeSize` / `dbVolumeSize`, all MiB)
move that spill to host disk instead: capacity scales with disk, not RAM, and
peak guest RAM tracks the working set rather than the total bytes ever written.
The images are sparse and mounted with `discard`, so host usage follows real
content, not the (generous) nominal sizes.

The images are created (and `mkfs`'d) on the host at launch and reused if they
already exist. For a **named** instance they live in the kept state dir, so a
stop/restart resumes with **warm caches** — an incremental `cargo build` after a
pause is a no-op, and host-built store paths stay registered. An **ephemeral**
instance gets fresh images each launch, cleaned up with the rest of its state.

## Reusing the host's Nix store

The guest mounts the host `/nix/store` read-only, but a Nix store is files
_plus_ a validity database, and microvm only registers the guest's own
**system** closure in that DB. So by default everything else — your
`nix develop` toolchain, build deps the host already has — is present as bytes
on the mount yet treated as missing, and re-downloaded.

`importHostStoreDb` (default `true`) closes that gap. At launch the runner takes
a consistent SQLite snapshot of the host's `db.sqlite` (~0.5s) into the
per-instance share; a guest boot service then seeds it over the system-only DB,
_after_ microvm's own closure registration. Because the guest system was itself
built on the host, the host DB is a strict superset — the seed keeps the VM
bootable while marking every host-built path valid, served straight from the
shared store with **no network**. Dropping into `nix develop` inside the VM is
then offline for anything the host already has; only genuinely-new paths hit the
network (and only if their origin is on the allowlist — keep e.g.
`static.rust-lang.org` there to pick up a freshly-bumped Rust toolchain).

The guest DB is on its own persistent volume, so the seed runs **once** per
named instance (gated on a marker) and then accumulates whatever the agent
builds in-VM — keeping it consistent with the persistent store overlay across a
restart. The trade is that a resumed instance does not pick up host paths built
_after_ its first launch; discard it with `--remove` to re-seed from a fresh
snapshot. An ephemeral instance, with a fresh DB volume each launch, seeds every
boot as before.

It's best-effort: a missing snapshot or a host/guest Nix schema mismatch falls
back to a freshly re-registered system-only DB, so a sandbox always boots. There
is no new read exposure — the whole host store is already readable over the
mount; this only changes what Nix _trusts_. Set `importHostStoreDb = false` to
opt out (the guest then substitutes everything from the allowlisted caches).

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

## Future plans

- **macOS hosts.** The sandbox is Linux-only today (the guest needs `/dev/kvm`).
  Porting to macOS is wanted, and `importHostStoreDb` is the part to watch: it
  assumes the host store/DB is the same world as the guest, which holds because
  the Linux guest is built _on_ the Linux host. An Apple-Silicon host is
  `aarch64-darwin` while the guest is `aarch64-linux` — different architectures
  — so the host's Darwin store is useless to the guest and its DB doesn't
  contain the guest's own closure. The feature stays _safe_ there (the post-swap
  sanity check fails and the guest rolls back to its system-only DB), but it
  would be a wasteful no-op. The real fix is store provisioning: on macOS the
  guest's world is backed by a **Linux builder store** (nix-darwin's
  `linux-builder` or a remote builder), so the same snapshot/transplant strategy
  applies unchanged — it just has to read the builder's DB rather than the local
  host's. The seams a port would touch: gate the runner snapshot on host
  platform, and let the snapshot source be the builder store instead of the
  hardcoded local `/nix/var/nix/db`.

[microvm]: https://github.com/microvm-nix/microvm.nix
