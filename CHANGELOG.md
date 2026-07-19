# Changelog

All notable changes to Katsuobushi are recorded here, **newest first**. The
format follows [Keep a Changelog]; the project is versioned with Git tags
following [SemVer]. While in `0.x`, any release may break — consumer-facing
breaking and behavioral changes are detailed in [`MIGRATING.md`](MIGRATING.md).

## [0.3.1] — 2026-07-19

Makes `katsuctl` build on non-Linux (macOS): the `project` domain is
cross-platform, and the Linux-only `sandbox` domain is now conditionally
compiled out. Also fixes streamed agent reports being lost from a captured,
non-TTY stream. No spec or instance-state bump (`specVersion 4` /
`instanceVersion 2` unchanged). See [`MIGRATING.md`](MIGRATING.md#031).

### Added

- **`katsuctl` builds on non-Linux.** The `sandbox` domain (and its Linux-only
  `tokio-vsock` dependency) is gated behind `#[cfg(target_os = "linux")]`, so on
  macOS `katsuctl` compiles with the `project` board commands and omits the
  sandbox subcommands. `lib.project` and `packages.<system>.katsuctl` are now
  available on every system; the sandbox library stays Linux-only.

### Fixed

- **Streamed agent reports are no longer lost in captured output.**
  `sandbox prompt` / `sandbox dispatch` rendered live reports to stdout, which
  the `emitExec` menu wrappers capture and which races teardown in a
  non-TTY/backgrounded stream — so a terminal `report done` could vanish.
  Reports now stream to **stderr** (the reliable channel the other progress
  lines already use), and gate their color on stderr's TTY-ness; `--json`
  streaming stays on stdout.

### Changed

- The shared output/rendering module moved from `sandbox::output` to a top-level
  `output` module — an internal refactor enabling the cross-platform split.

## [0.3.0] — 2026-07-18

Adds a file-backed **project board** and agent **orchestration** built on it:
queue work as Obsidian-Kanban cards (`katsuctl project` / the `project` menu
command / `lib.project`), dispatch cards to sandbox VMs for implementation, and
peer-review in sandboxes. Ships two new skills. Additive for existing consumers
— no spec or instance-state bump (`specVersion 4` / `instanceVersion 2`
unchanged). See [`MIGRATING.md`](MIGRATING.md#030).

### Added

- **`project` board + `lib.project`.** A lightweight backlog rendered as an
  Obsidian Kanban board:
  `project init / new / status / status set / prioritize / lint`. Six-state
  lifecycle (to-do → in-progress → needs-review → ready → accepted, plus
  cancelled), 6-hex card ids, and `blocked_by` dependencies that clear their
  dependents at `ready`.
- **`project status`** — the unified view+mutate surface: bare lists the board,
  `<id>` shows one card, `--available` / `--lane` filter, `--json` is
  machine-readable. A card entering **Ready** auto-slots into suggested
  acceptance order (dependencies first, then oldest `created`) without
  disturbing a manual order.
- **`project status set --accept-all`** bulk-accepts every Ready card. Terminal
  transitions stamp a `disposition_at` timestamp; the human `project status`
  list shows only cards archived in the last 24h (`--json` returns all).
- **`sandbox dispatch <card>`** launches an agent VM to implement a board card,
  and **`sandbox fetch`** now reports whether committed work actually landed
  (branch tip vs. launch seed).
- **Two skills** — `project` (board mechanics) and `project-orchestration` (the
  implementor / peer-reviewer / product-owner / orchestrator roles, sandbox
  dispatch, the report bridge, and a host-core concurrency budget).

### Changed

- The Katsuobushi repo's own sandbox `allowedOrigins` now include the crates.io
  origins so in-guest `cargo` builds fetch dependencies normally.

## [0.2.9] — 2026-07-09

Fixes a regression from the 0.2.6 command-tree rename: prompting a paused, named
sandbox no longer fails trying to resume it. The fix is host-side, so a
dev-shell rebuild picks up the corrected controller; no spec or instance-state
bump (`specVersion 4` / `instanceVersion 2` unchanged). See
[`MIGRATING.md`](MIGRATING.md#029).

### Fixed

- **`sandbox prompt` resumes a paused named instance again.** The 0.2.6 rename
  removed the `sandbox:start` menu binary, but `sandbox prompt`'s auto-resume
  still shelled out to that name — so prompting a powered-off named instance
  errored instead of booting it to deliver the turn. Resume is now
  self-contained: it re-runs the `start` subcommand and execs the boot recipe
  that emits, via the pinned `katsuctl` / `bash` store paths, depending on no
  menu command being on `PATH`.
- **Stale `sandbox:*` command names in hints.** Status, attach, resolve, and
  stop messages — and the post-launch `start` echo — still suggested the removed
  colon-namespaced commands (`sandbox:status`, `sandbox:fetch`, …); they now
  name the current subcommand forms (`sandbox status`, `sandbox fetch`, …).

## [0.2.8] — 2026-07-06

Adds a built-in `menu` command to every dev shell, so the command table can be
reprinted on demand. Menu-only; no spec or instance-state bump (`specVersion 4`
/ `instanceVersion 2` unchanged). See [`MIGRATING.md`](MIGRATING.md#028).

### Added

- **Built-in `menu` command.** `makeMenu` now always contributes a `menu`
  command ("Print this menu.") that reprints the top-level command table. Like
  any command it shows its own figlet banner + description first, but it does
  not reprint the hero graphic — that stays a one-time greeting shown when you
  drop into the dev shell. Define your own `menu` in `commands` to override the
  built-in.

## [0.2.7] — 2026-07-06

A menu-polish follow-up to 0.2.6: subcommand banners regained their figlet
header, and all menu decoration moved to stderr so it can no longer muddy a
command's stdout. Menu-only; no spec or instance-state bump (`specVersion 4` /
`instanceVersion 2` unchanged). See [`MIGRATING.md`](MIGRATING.md#027).

### Fixed

- **Subcommand banners show the full command chain again.** 0.2.6's subcommand
  trees dropped the pretty figlet header other commands show — a branch printed
  none, and a leaf would at most have shown its own bare key. A resolved leaf
  now banners with its whole Nix-encoded chain (`sandbox status`, not `status`;
  `markdown format`), and a bare branch banners with its own name (`markdown`).

### Changed

- **Menu decoration goes to stderr.** The dev-shell greeting (hero + command
  table) and every command's figlet banner now write to stderr instead of
  stdout, so `nix develop -c 'sandbox status --json' | jq` — and any
  captured/piped menu command — keeps clean, pipeable stdout. The greeting still
  displays on the terminal and still clears the screen on entry; the per-command
  banner still precedes the command's real output.

## [0.2.6] — 2026-07-03

Dev-shell menu commands are now organized as subcommand trees, so a namespace
collapses to a single command + menu row instead of one row per verb. The
`sandbox:*` and `format:*` / `lint:*` commands are renamed accordingly — the one
consumer-facing break in this release. No spec or instance-state bump
(`specVersion 4` / `instanceVersion 2` unchanged). See
[`MIGRATING.md`](MIGRATING.md#026).

### Added

- **`makeMenu` command trees.** A menu command may now be a _branch_ — an entry
  with a `subcommands` attrset instead of a `command` — which compiles to one
  shell application that dispatches on its first argument and recurses to any
  depth. Both leaves and branches take an optional `help` string; running a
  branch bare (or with `-h` / `--help`) prints that preamble plus an aligned
  table of its subcommands. Flat command sets are unchanged, so an existing menu
  keeps working untouched.
- **Sandbox usage lines read as `sandbox …`.** clap prints its errors and
  `Usage:` lines qualified by katsuctl's real path (e.g.
  `katsuctl sandbox --config <CONFIG> attach <INSTANCE>`); the menu wrappers now
  rewrite that prefix back to the command the user typed
  (`sandbox attach <INSTANCE>`) in katsuctl's stderr. Only stderr is filtered,
  so streaming stdout — notably `sandbox prompt`'s live report stream — is
  untouched.

### Changed

- **`lib.sandbox` menu commands are now `sandbox <verb>`.** The seven
  `sandbox:*` entries collapse into one `sandbox` branch with `start`, `prompt`,
  `status`, `fetch`, `stop`, `attach`, and `screenshot` subcommands. Each verb
  keeps its exact behavior, and `nix run .#sandbox` is unchanged.
- **`lib.markdown` menu commands are now `<name> <verb>`.** Each invocation's
  `format:<name>` / `lint:<name>` pair becomes a single `<name>` branch with
  `format` and `lint` subcommands (default name `markdown`, so `markdown format`
  / `markdown lint`). The flake `check` name is unchanged.

### Removed

- **The colon-namespaced command names.** `sandbox:start` (and the other six
  `sandbox:*`), `format:<name>`, and `lint:<name>` no longer exist as dev-shell
  commands — use the subcommand forms above. Update any script, CI step, or
  `nix develop -c …` invocation that calls an old name.

## [0.2.5] — 2026-07-01

A hardening release from a full engineering audit of the sandbox feature: shell
quoting, secret staging, and teardown fixes on the host; turn-delivery
correctness fixes on both sides of the agent-mode channel; guest control-plane
hardening; and safe parallel launches. No spec or instance-state bump
(`specVersion 4` / `instanceVersion 2` unchanged) and no config changes — but
restart agent instances so the guest-side fixes take effect, and note the
tightened eval-time validation. See [`MIGRATING.md`](MIGRATING.md#025).

### Fixed

- **Recipes single-quote host paths.** The emitted start recipe double-quoted
  paths (the git toplevel, XDG-expanded roots, context entries), leaving `$`,
  backticks, and `\` shell-active; every path is now single-quoted with the same
  close-escape-reopen idiom the prompt payload already used, so a path
  containing shell-special characters is inert.
- **`fromEnv` secrets are born `0600`.** The credential file was created under
  the default umask and then chmod'd, leaving a brief window where the plaintext
  token was world-readable; it is now recreated under a subshell `umask 077`,
  matching the `install -m 0600` guarantee the `fromFile` branch already had.
- **`sandbox:stop` confirms the VM died before removing its state.** `quit` was
  fire-and-forget: a wedged monitor fell through to recursive removal, deleting
  the disk images out from under a still-running qemu while reporting success.
  Stop now polls the monitor after `quit` and refuses removal (loud, nonzero,
  nothing deleted) while it still answers; both dir removals are also attempted
  before an error surfaces, so a partial failure no longer strands a
  half-torn-down instance.
- **A failed first injection no longer wedges the turn.** The guest committed a
  turn to in-flight before the injection ran, so if the injection failed (the
  first-turn race) every host resend of that id was dedupe-dropped forever.
  Delivery is now tracked separately: an undelivered turn re-injects on resend,
  a delivered one dedupes, and a resend during the stop-grace window no longer
  creates a fresh turn (which would have executed it twice).
- **The turn-id counter never rewinds.** A corrupt (or schema-newer)
  `liveness.json` silently reset `nextTurnId` to 1, and the guest's turn-id
  dedupe would then drop the next genuinely-new prompt. A corrupt record now
  fails `sandbox:prompt` loudly instead, the best-effort heartbeat writers skip
  rather than clobber it, and unknown fields no longer fail the parse.
- **`sandbox:status` no longer reports a phantom active stream.** The
  `streamActive` flag is only cleared by a clean driver shutdown, so a
  panicked/killed driver left `status` claiming an active stream forever; the
  flag is now believed only while the recorded heartbeat is within the watchdog
  deadline.
- **A stale report cannot end the wrong turn.** Both sides applied
  accept/terminal transitions to whatever turn was in flight, so a late `done`
  from turn N could terminate turn N+1 and falsely satisfy its delivery ack; a
  report naming a different turn now relays without transitioning.
- **Parallel `sandbox:start`s cannot collide.** CID/port selection read sibling
  instances before either launch had persisted its claim (and a sibling's ssh
  port is not even bound until its qemu boots, so the bind probe alone could not
  see it). The planner now skips sibling-recorded ports and CIDs and holds an
  advisory `flock` under the project state root across the probe→persist window
  — swarm launches allocate safely.

### Changed

- **The guest bounds and times out its I/O.** Inbound lines on the control and
  report sockets are capped at 1 MiB (the report socket is reachable by the
  unprivileged in-guest agent, so an unterminated flood was an in-guest OOM),
  outbound writes to the host time out after 10s and drop a wedged connection
  instead of freezing the heartbeat behind it, and the `turn-state.json` persist
  moved off the async workers so a stalled 9p share cannot pin them.
- **Eval-time validation is tighter.** A `homeFiles` entry with an unknown
  `mode` now fails evaluation instead of silently never appearing in the guest,
  and `homeFiles`/`extraRepos` destinations get the same full `..` traversal
  check as `workspaceContext` (whose `/..`-suffix form `extraRepos` historically
  missed).
- **Ephemeral instance names are UTC-stamped.** The timestamp is now formatted
  in Rust (it was the lone bare-PATH `date` invocation in an otherwise
  pinned-tool contract) and uses UTC where the shell used host-local time.

## [0.2.4] — 2026-06-29

A packaging hotfix: the `sandbox:*` menu commands failed for consumers with
`katsuctl: command not found`. They invoked `katsuctl` by bare name and relied
on it already being on the dev shell's PATH — which only Katsuobushi's own dev
shell arranged, so a project that wired in just `sandbox.menuCommands` got
commands that could not find their own controller. The instance spec bumps to
`specVersion 4`; see [`MIGRATING.md`](MIGRATING.md#024).

### Fixed

- **`sandbox:*` commands work without `katsuctl` on PATH.** Every menu command
  (and `nix run .#sandbox`) now invokes the controller by its absolute store
  path, and the agent-mode `start` recipe self-references it through a new
  `tools.katsuctl` spec field instead of a bare `katsuctl … prompt` tail-call
  run in a child shell. A consumer that wires only `sandbox.menuCommands` into a
  dev shell no longer hits `katsuctl: command not found`. No PATH manipulation
  remains in any command.

### Added

- **`lib.sandbox` exposes `katsuctl`.** The host controller derivation (built
  via `lib.rust`/crane from Katsuobushi's pinned source) is now returned from
  `lib.sandbox` as `katsuctl`, so a project can put a bare `katsuctl` on its dev
  shell PATH for direct use. The sandbox template wires it in for power users;
  the `sandbox:*` commands no longer require it.

## [0.2.3] — 2026-06-29

A graphics hotfix: in a graphics guest an X11 app — or any tool that probes
`DISPLAY` — failed out of the box because only `WAYLAND_DISPLAY` was set. See
[`MIGRATING.md`](MIGRATING.md#023).

### Fixed

- **X11 apps work in a graphics guest.** The guest now exports `DISPLAY=:0`
  alongside `WAYLAND_DISPLAY` (in both the sshd `SetEnv` and the login shell)
  and ships `xwayland`, so sway's XWayland shim serves X clients on `:0`. A tool
  that probes `DISPLAY`, or an X-only app, now runs with no per-invocation
  ceremony. Gated on the graphics opt-in; a graphics-off guest is byte-for-byte
  unchanged.

## [0.2.2] — 2026-06-29

Opt-in **graphics**: a sandbox can now boot a headless compositor and a
paravirtual GPU so a browser or Wayland app actually renders. It is off by
default, so existing consumers are unaffected; enabling it widens the
host-facing attack surface (the GPU command stream is parsed in the host QEMU
process), which the README documents plainly. The instance spec bumps to
`specVersion 3`; see [`MIGRATING.md`](MIGRATING.md#022).

### Added

- **`lib.sandbox`: opt-in `graphics` capability.** A new `graphics` attrset
  (`enable`, default `false`; `gpu` role-preference list, default
  `["integrated" "discrete" "software"]`; `output`, default `1920×1080@60`)
  boots a headless sway compositor on a virtual output and, when a GPU rung
  resolves, hands QEMU a `virtio-gpu-gl` device against a host render node — so
  a browser (WebDriver/Playwright) or a Wayland app can render. The browser/app
  goes in the existing `packages` list. Pinning `gpu = ["software"]` keeps the
  full original boundary (llvmpipe, no GPU device) at a performance cost. When
  enabled, `sandbox:status` adds a `graphics` preflight row that runs the real
  GPU resolver against the host and flags a missing `render`-group membership,
  and a launch-time notice records the widened attack surface. See
  [`lib/sandbox/README.md`](lib/sandbox/README.md#graphics-opt-in).
- **`sandbox:screenshot <instance|#> [path]`.** A new menu command that grabs a
  PNG of the headless-sway output by running `grim` over the existing loopback
  ssh — no daemon, no new port. Defaults to a timestamped PNG in the cwd; `-`
  streams to stdout. Requires the graphics opt-in; a purely-offscreen workload
  that never composites a surface screenshots as blank (expected).
- **`sandbox:status` GRAPHICS column.** The instance list now shows the GPU rung
  each instance launched on — `integrated`, `discrete`, `software`, or `none`
  when graphics is off — recorded per-instance in `instance.json` (and surfaced
  in the detail view and `--json`).

### Changed

- **The instance spec is now `specVersion 3`** (carrying the `graphics` block);
  a stale v2 spec is rejected loudly. Rebuild your dev shell (`nix develop`) so
  the spec re-renders. No config changes are required.
- **`instance.json` is now `instanceVersion 2`** (it records the resolved
  graphics rung). A v1 instance state from an earlier release is rejected on
  read, so recreate any persistent (`--name`d) instance after upgrading —
  ephemeral instances are unaffected.

## [0.2.1] — 2026-06-28

Sandbox **liveness**: the host and guest now agree on when a turn started,
finished, or silently died — closing the first-turn race and surfacing
unreported hangs. An agent-mode VM emits heartbeats and lifecycle edges, and the
guest persists turn state to the share so `sandbox:status` can report it
out-of-band, even with nothing attached. No action for devshell users beyond
rebuilding (the instance spec bumps to `specVersion 2`); see
[`MIGRATING.md`](MIGRATING.md#021).

### Added

- **Turn/transport liveness machinery.** The guest controller runs a per-turn
  state machine and writes a durable `turn-state.json` to the share on every
  transition (`idle` → `in-flight` → `ended-ok` / `ended-unreported`), plus a
  periodic heartbeat. A `report hook <event>` bridge wires Claude Code's `Stop`,
  `SessionStart`, and `UserPromptSubmit` hooks (managed-settings tier) into that
  machine.
- **Host `drive` watchdog.** `sandbox:prompt` now runs a deadline-aware loop:
  ack-and-resend of an undelivered first turn, a heartbeat-deadline that detects
  a dead transport, a one-shot progress-stall notice, and a pre-send ready-gate
  that closes the first-turn race — a prompt to a just-booted instance no longer
  lands in the arming gap. Heartbeats are silent, so a backgrounded drive is
  never woken by a tick, and a monotonic, persisted `turn_id` makes resends
  safe.
- **`sandbox:status` liveness line.** Status reads `turn-state.json` (and the
  host-written `liveness.json`) to show per-instance turn/transport state with
  no connection — e.g. `turn 3 ended-unreported 14m ago · no active stream` —
  corroborated against QMP.
- **Seven liveness tunables** (`heartbeatSecs`, `heartbeatMiss`,
  `progressStallSecs`, `deliveryDeadlineSecs`, `deliveryRetries`,
  `readyGateSecs`, `stopGraceMs`), Nix-driven from one source into both the spec
  and the guest env.

### Changed

- **The instance spec is now `specVersion 2`** (carrying the liveness tunables);
  a stale v1 spec is rejected loudly. Rebuild your dev shell so it re-renders.

### Fixed

- **The per-instance share root is now guest-writable**, so the guest controller
  can create `turn-state.json` on a real boot. The `mapped-xattr` 9p share left
  the root root-owned; the launch recipe now opens it `a+rwX`, as it already did
  for `sync.git`.
- **`liveness.json` is written atomically** (temp + rename), so `sandbox:status`
  never reads a torn record.

## [0.2.0] — 2026-06-27

The host side of the sandbox is rewritten from an unmaintainable pile of
untested shell into a compiled, tested Rust binary, **`katsuctl`**. From a
devshell user's perspective this is a no-op — `sandbox:start`, `sandbox:prompt`,
`sandbox:status`, `sandbox:fetch`, `sandbox:stop`, and `sandbox:attach` keep
their names and behavior — but their logic now lives in
`katsuctl <domain> <command>` with unit, golden-snapshot, and seam-level tests,
verified end-to-end against a real KVM boot. The three in-tree Rust crates are
also renamed for clarity (breaking only for anyone depending on them directly —
see [`MIGRATING.md`](MIGRATING.md#020)).

### Added

- **`katsuctl` host-side controller** (`katsuctl sandbox <command>`) absorbing
  all the sandbox host logic: instance naming / ssh-port / vsock-CID /
  seed-commit decisions made in tested Rust, a Nix-rendered instance spec passed
  via `--config`, a native QMP client (liveness + quit), a consolidated
  `instance.json` per-instance metadata file, an emit-script harness for the
  `start`/`attach` terminal hand-offs, and dual human/`--json` output with
  strict color gating. Built reproducibly via the flake
  (`nix build .#katsuctl`).

### Changed

- **The six `sandbox:*` devshell commands are now thin `katsuctl` wrappers** —
  same names and behavior, but every decision is made in tested Rust and the
  shell that remains is a flat, generated recipe. Secrets are emitted as
  references, never values, and the start/attach recipes are golden-snapshotted.
- **Rust crates renamed** (see [`MIGRATING.md`](MIGRATING.md#020)): the host
  controller crate `katsuctl` → `katsuobushi-controller` (it still ships the
  `katsuctl` binary), `katsuobushi-protocol` → `katsuobushi-sandbox-protocol`,
  and `katsuobushi-sandbox-control` → `katsuobushi-sandbox-guest` (its guest
  channel-server binary renames with it).
- **`sandbox:status`** gains an aligned, color-coded table and a `--json` mode.
  The list shows `# / INSTANCE / STATE / MODE / PERSIST`; the ssh port and vsock
  CID moved to the per-instance detail view (`sandbox:status <name>`). A bare
  `status` doubles as the launch prerequisite gate — nonzero exit if a declared
  secret or `/dev/vhost-vsock` is missing.

### Removed

- **The old host-side shell** — `sandboxRunner`, the `isRunning` QMP probe,
  `instanceHelpers`, and `statusSecretChecks` — and the standalone
  `katsuobushi-sandbox-prompt` host-client binary, all replaced by `katsuctl`.

## [0.1.10] — 2026-06-26

A sandbox release with one consumer-facing breaking change: the guest's writable
scratch is now disk-backed instead of RAM-backed, and the single
`storeOverlaySize` argument is replaced by three sparse-image sizes. Also adds
auto-start when prompting a paused instance. See
[`MIGRATING.md`](MIGRATING.md#0110).

### Added

- **`sandbox:prompt` auto-starts a paused instance.** Prompting a named instance
  that was stopped (but kept) now restarts it — booting and arming the channel
  (~30–60s) before delivering the turn — instead of hanging against the
  powered-off VM. A pause discards the VM's RAM, so the live conversation does
  not survive it; only the pushed branch does, and the resumed agent begins a
  fresh session on top of its branch.

### Changed

- **Writable scratch is disk-backed, not RAM-backed.** The writable `/nix/store`
  overlay, the workspace clone (with build artifacts), the relocated
  `cargo`/`rustup`/XDG caches, and the guest Nix database now live on
  per-instance sparse disk images instead of tmpfs. Capacity scales with host
  disk rather than a fraction of `mem`, so a Rust `target/` can no longer
  exhaust guest RAM; the guest root `/` stays a tmpfs. A **named** instance
  keeps these images across a stop/restart, so warm build caches (and host-path
  registrations) survive a pause; ephemeral instances get fresh images each
  launch.
- **`importHostStoreDb`: the guest Nix database now persists and is seeded
  once.** On its own persistent volume, the host-DB snapshot is applied a single
  time per named instance (gated on a marker) and then accumulates the agent's
  in-VM registrations — keeping it consistent with the persistent store overlay
  across a restart, rather than re-seeding every boot.

### Removed

- **`storeOverlaySize` is replaced by `storeVolumeSize` / `scratchVolumeSize` /
  `dbVolumeSize`.** The old single tmpfs-size string is gone; the three new
  arguments size the disk images (in MiB, sparse). Defaults: 16384 / 32768
  / 4096. See [`MIGRATING.md`](MIGRATING.md#0110).

## [0.1.9] — 2026-06-26

A sandbox-ergonomics release: instances are now numbered, and there is a
one-shot command to attach to a running agent's live session. Purely additive —
no consumer config changes; see [`MIGRATING.md`](MIGRATING.md#019).

### Added

- **`sandbox:attach <instance|#>`.** A new menu command that SSHes into a
  running instance, pins `TERM=xterm-256color` in the remote session (so
  terminals like ghostty don't confuse the guest's `tmux`), and attaches the
  agent's `katsuobushi` tmux session — collapsing the ssh-then-`tmux attach`
  dance that `sandbox:status <instance>` used to print by hand.
- **Numeric instance references.** `sandbox:status` now prints a leading `#`
  column numbering each instance, and that index is accepted anywhere a name is
  — `sandbox:prompt`, `sandbox:status`, `sandbox:attach`, `sandbox:fetch`, and
  `sandbox:stop` all resolve an all-digit argument as a 1-based index into the
  current listing. The numbering is positional (it can shift as instances come
  and go); names remain the stable handle. Real instance names always carry a
  `-` from their timestamp or hex suffix, so a name is never mistaken for an
  index.

### Changed

- **`sandbox:status` listing gains a `#` column.** The instance table now leads
  with a 1-based index; anything parsing that table by column position should
  account for the extra leading field.

## [0.1.8] — 2026-06-25

A sandbox release. One default-on behavioral change — the guest now reuses the
host's Nix store instead of re-downloading what the host already built — that is
transparent in normal use; see [`MIGRATING.md`](MIGRATING.md#018).

### Added

- **`lib.sandbox`: `importHostStoreDb` option.** A new argument (default `true`)
  that makes the guest reuse everything the host has already built instead of
  re-downloading it. The guest already mounts the host `/nix/store` read-only,
  but microvm registers only the guest's _system_ closure as valid, so other
  host paths (e.g. a `nix develop` toolchain) were present on the mount yet
  re-substituted from the network. The runner now snapshots the host's
  `db.sqlite` at launch (a consistent SQLite `.backup`, ~0.5s) into the share,
  and a guest boot service transplants it over the system-only DB — after
  microvm's own closure registration — so every host-built path becomes valid
  with no network and no copying. The transplant is best-effort: a missing
  snapshot or a host/guest Nix schema mismatch rolls back to the system-only DB,
  so a sandbox always boots. No new read exposure — the whole host store was
  already readable over the mount. Set `importHostStoreDb = false` to opt out.

### Changed

- **`lib.sandbox` (this repo's own config): allowlist `static.rust-lang.org`.**
  Dropping into `nix develop` inside the sandbox provisions the Rust toolchain
  via rust-overlay, which fetches from `static.rust-lang.org`; that host was
  missing from the egress allowlist. With `importHostStoreDb` on, the toolchain
  is reused from the host offline, so this is only the fallback for picking up a
  newly bumped `rust-toolchain.toml`.

## [0.1.7] — 2026-06-25

A docs-and-features release; nothing to migrate (see
[`MIGRATING.md`](MIGRATING.md#017)).

### Added

- **`lib.menu.makeMenu`: `colorizeGraphic` option.** A new optional argument
  (default `true`, preserving current behavior) controls whether the ASCII art
  banner is run through the colorizer. Set `colorizeGraphic = false` to print
  the banner raw while still colorizing the title and command table. Has no
  effect when no banner is set.
- **`lib.menu.makeMenu`: `graphicFile` option.** A new optional argument
  (default `null`) supplies the banner from a file path that is `cat`ed at
  runtime rather than inlined as a string. This keeps raw bytes — notably ANSI
  escape (`U+001B`) sequences in pre-colorized terminal art — out of the
  `shellHook`, which `nix develop` would otherwise reject when serializing the
  shell environment to JSON. Takes precedence over `graphic`; pair with
  `colorizeGraphic = false` to preserve the art's embedded colors. Katsuobushi's
  own banner now ships as pre-colorized pixel art (`hero.ansi`) through this
  path.

### Changed

- **`lib.sandbox`: `sandbox:*` menu descriptions trimmed to short summaries.**
  The dev-shell menu entries for `sandbox:start` / `prompt` / `status` / `fetch`
  / `stop` dropped their inline usage hints (e.g. `sandbox:fetch <instance>`),
  leaving a one-line summary; full usage lives in the `sandbox` skill. Command
  names and behavior are unchanged.
- **`lib.sandbox`: `sandbox:status` preflight names the OAuth token fix.** When
  `CLAUDE_CODE_OAUTH_TOKEN`'s host source is missing, the `environment:` report
  now appends a `run 'claude setup-token'` hint alongside the variable to
  export.

## [0.1.6] — 2026-06-25

A skill-and-docs release; nothing to migrate (see
[`MIGRATING.md`](MIGRATING.md#016)).

### Changed

- **`sandbox` skill: fan out via sub-agents; refined jj landing guidance.** The
  skill now drives parallel fan-out by giving each task its own sub-agent — each
  launches and drives its own `--name`d VM to `done` and returns its branch plus
  the agent's `done` summary — while integration stays serial in the
  orchestrator. The jj landing step now anchors accepted work on the
  working-copy commit `@` (`jj new <tip>`) and leaves bookmark placement to the
  user, keeping landed work durable across the git imports the sandbox commands
  trigger. Touches `plugins/katsuobushi/skills/sandbox/SKILL.md` and
  `lib/sandbox/README.md`; no library change.

## [0.1.5] — 2026-06-24

A docs-only release; nothing to migrate (see
[`MIGRATING.md`](MIGRATING.md#015)).

### Changed

- **`sandbox` skill: remove an instance once its work is accepted.** The skill
  now directs tearing the sandbox down with `sandbox:stop --remove <name>` as
  soon as its unit of work is complete and accepted — both in the branch-landing
  workflow and in the lifecycle section — since the `sandbox/<name>` ref is the
  durable artifact, not the VM. No library change.

## [0.1.4] — 2026-06-24

### Changed

- **`lib.sandbox`: a provided `--name` is suffixed with random entropy.** At
  launch, `--name foo` now mints an instance named `foo-<8 hex>` (e.g.
  `foo-a3f9c2d1`), so every launch is a fresh, collision-free instance instead
  of a silent resume of an older same-named branch. The full suffixed name is
  printed at launch and by `sandbox:stop`; drive (`prompt`/`status`/`fetch`/
  `stop`) and resume with that full name. A name that already carries the 8-hex
  suffix is left as-is, so passing the printed name back is safe. See
  [`MIGRATING.md`](MIGRATING.md#014).

## [0.1.3] — 2026-06-24

A docs-and-internals release; nothing to migrate (see
[`MIGRATING.md`](MIGRATING.md#013)).

### Changed

- **`sandbox` skill docs substantially revised** — added the branch-landing /
  integration workflow, conflict-reconciliation-as-delegation guidance, and
  parallel fan-out notes.
- **`lib.sandbox`: `sandbox:status` preflight internals refactored.** The
  preflight now builds its report in a subshell and carries the problem count
  out via the subshell's exit status (the `|| errs=$?` is load-bearing under
  `inherit_errexit`). Observable behavior is unchanged from 0.1.1.

## [0.1.2] — 2026-06-24

A docs-only release; nothing to migrate (see
[`MIGRATING.md`](MIGRATING.md#012)).

### Changed

- **`sandbox` skill docs reworked** — clearer `sandbox:status` guidance, a note
  that `sandbox:*` are dev-shell menu commands (`nix develop -c sandbox:status`
  from outside the shell), and Prettier reflow.
- **Markdown linting now covers `plugins/**/\*.md`.\*\* Repo-internal; no
  consumer impact.

## [0.1.1] — 2026-06-24

### Added

- **`lib.sandbox`: `sandbox:status` preflight.** A bare `sandbox:status` now
  prints an `environment:` block before listing instances, verifying every
  declared secret at its **host** source (the `fromEnv` variable is set, or the
  `fromFile` path is readable) and checking for `/dev/vhost-vsock`. It names the
  exact host variable feeding each guest secret, so "is this host ready to
  launch?" is a single command with no project-specific knowledge required.

### Changed

- **`lib.sandbox`: `sandbox:status` exits non-zero when the preflight fails.**
  Previously the bare command always exited `0`; it now exits with the count of
  missing prerequisites, so its exit status alone is a usable gate. See
  [`MIGRATING.md`](MIGRATING.md#011).
- **Docs:** clarified that the guest always reads `CLAUDE_CODE_OAUTH_TOKEN`
  while `secrets.*.fromEnv` chooses which **host** variable supplies it, and
  documented the agent-harness workaround (a harness scrubs
  `CLAUDE_CODE_OAUTH_TOKEN` from its children, so source it from a
  differently-named host variable, e.g. `HARNESS_OAUTH_TOKEN`). Touches
  `lib/sandbox/README.md`, the `sandbox` skill, and the `sandbox` template.

### Fixed

- **`lib.sandbox`: the guest can push to the 9p sync mirror.** The per-instance
  bare mirror is now shared over 9p with `security_model=mapped-xattr` (was
  `security_model=none`), so files the guest creates are recorded as
  agent-owned. The unprivileged agent could previously never write its
  receive-pack quarantine dir, so `git push` failed and no work crossed the
  sandbox boundary. The mirror's pre-existing directories are also opened so the
  agent can create entries inside them.

## [0.1.0] — 2026-06-23

The first tagged release. Highlights below; consumer-facing migration notes for
everything tracked on untagged `main` up to this tag are in
[`MIGRATING.md`](MIGRATING.md#010).

### Added

- **`lib.sandbox`** — a new library that assembles a [`microvm.nix`] guest which
  boots into a working dev environment where an agent harness (Claude Code by
  default) runs with its blast radius bounded by a real VM. Provides
  `apps.sandbox` (`nix run .#sandbox`), the `sandbox:*` menu commands (`start`,
  `prompt`, `status`, `fetch`, `stop`), `checks.sandbox`, and
  `nixosConfiguration`. Scaffold with
  `nix flake init -t github:cdata/katsuobushi#sandbox`.
- **`sandbox` template** and **`sandbox` agent skill** for the above.
- **`rust` template** for scaffolding Rust projects.
- **Transitive infra dependency inheritance.** Katsuobushi now owns `crane`,
  `nix-filter`, `rust-overlay`, and `microvm`, passing them through to consumers
  so a `lib.rust` consumer flake collapses from six inputs to two.
- **`lib.rust`: `target` argument** on `buildCrate` / `buildTestArchive` for
  cross-compiling to arbitrary triples; **`sourceInclude`** argument for crates
  that do not live under `rust/`.

### Changed

- **`lib.markdown` now uses [Prettier]** instead of `rumdl`, which mishandled
  GFM tables. Scope is now `include` / `exclude` glob lists plus a `name` label
  (replacing `docsDir`); `settings` takes Prettier options; outputs and menu
  commands are namespaced per invocation (`format:<name>` / `lint:<name>`).
- **`lib.rust` input arguments renamed** to match nixpkgs vocabulary:
  `buildInputs` → `nativeBuildInputs` (build tools) and `libraries` →
  `buildInputs` (link libraries); both now default to `[ ]`.
- **`lib.rust` wasm-bindgen version is derived from `Cargo.lock`** rather than
  hard-pinned, failing fast with a copy-pasteable fix on a mismatch. Default
  hashes ship for `0.2.108`.
- **`lib.rust` crate version is derived from `Cargo.toml`** instead of a
  hardcoded `0.1.0`; derivation name prefix derives from `projectId`.

See [`MIGRATING.md`](MIGRATING.md#010) for the full upgrade details.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/
[SemVer]: https://semver.org/spec/v2.0.0.html
[Prettier]: https://prettier.io
[`microvm.nix`]: https://github.com/astro/microvm.nix
