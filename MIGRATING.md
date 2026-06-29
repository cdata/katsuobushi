# Migrating Katsuobushi

Katsuobushi is versioned with Git tags (SemVer); pin a release as
`github:cdata/katsuobushi/v0.1.0`. While in `0.x`, any release may break.

Each version heading below covers the changes **from the version immediately
beneath it up to that version**. The top heading is the current release. `0.1.0`
is the first tagged release, so it covers everything up to the first tag — i.e.
the changes anyone tracking untagged `main` should know about.

## 0.2.3

No action required.

## 0.2.2

**Action required: rebuild your dev shell.**

The sandbox gains an opt-in `graphics` capability (a headless compositor plus a
paravirtual GPU). It is **off by default**, so **existing consumers need no
change** — a sandbox without a `graphics` block behaves exactly as before.

The one thing everyone must do is rebuild: the instance spec bumps to
`specVersion 3`, and a stale v2 spec is now rejected loudly. Run `nix develop`
(or otherwise rebuild your dev shell) so the spec re-renders; no config changes
are required.

Per-instance state also bumps: `instance.json` is now `instanceVersion 2` (it
records the resolved graphics rung shown in `sandbox:status`). A v1 instance
state from `0.2.1` is rejected on read, so recreate any persistent (`--name`d)
instance after upgrading — ephemeral instances are unaffected.

If you _do_ enable graphics, two things are worth knowing — both covered in
[`lib/sandbox/README.md`](lib/sandbox/README.md#graphics-opt-in):

- It widens the host-facing attack surface (a GPU rung parses the guest's GPU
  command stream inside the host QEMU process). Pin `gpu = ["software"]` to keep
  the full original boundary at a performance cost.
- Set a higher resource floor yourself (`vcpu ≥ 4`, `mem ≥ 8192`) — the library
  does not auto-bump them — and ensure your uid can open a host render node (the
  `graphics` row in `sandbox:status` checks this and names the fix).

## 0.2.1

**Action required: rebuild your dev shell.**

Agent-mode sandboxes gain turn/transport liveness: heartbeats, a durable
`turn-state.json` on the share, a host watchdog with ack-and-resend and a
ready-gate, and a `sandbox:status` liveness line. It is additive, so there is
**no action for devshell users** — except that the instance spec bumps to
`specVersion 2`, and a stale v1 spec is now rejected loudly. Run `nix develop`
(or otherwise rebuild your dev shell) so the spec re-renders; no config changes
are required.

The seven liveness knobs (`heartbeatSecs`, `heartbeatMiss`, `progressStallSecs`,
`deliveryDeadlineSecs`, `deliveryRetries`, `readyGateSecs`, `stopGraceMs`) ship
with sensible defaults and need no consumer action.

## 0.2.0

**Host sandbox control is now `katsuctl` — `sandbox:*` behavior is unchanged.**

The host side of the sandbox (`sandbox:start` / `sandbox:prompt` /
`sandbox:status` / `sandbox:fetch` / `sandbox:stop` / `sandbox:attach`) is
reimplemented as a tested Rust binary, `katsuctl`, behind the **same** devshell
command names. **No action for devshell users** — the command names and behavior
are unchanged, verified end-to-end on a real boot. The win is internal: the host
logic now lives in compiled, tested Rust instead of an untested shell pile.

**The three in-tree Rust crates are renamed.**

Only relevant if your flake references these crates or their build outputs
directly:

- `katsuctl` → **`katsuobushi-controller`** — still produces the `katsuctl`
  binary, and `nix build .#katsuctl` is unchanged.
- `katsuobushi-protocol` → **`katsuobushi-sandbox-protocol`**.
- `katsuobushi-sandbox-control` → **`katsuobushi-sandbox-guest`** — its guest
  controller server binary (and the agent-mode MCP/channel server name) renames
  with it; the flake output is now `.#katsuobushi-sandbox-guest`.

If you build a specific crate via `nix build .#<crate>`, update the attribute to
the new name (except `.#katsuctl`, which is unchanged).

**`sandbox:status` no longer lists the SSH and CID columns.**

The list view (`sandbox:status` with no argument) drops the `SSH` (ssh port) and
`CID` (vsock CID) columns — they are plumbing you do not type by hand. Both
remain in the **per-instance detail view** (`sandbox:status <name>`), alongside
the ready-to-run ssh and `sandbox:prompt` commands, and in the `--json` output.
Tooling that parsed those two columns from the list table should read the detail
view or `--json` instead.

## 0.1.10

**`lib.sandbox`: writable scratch is now disk-backed — `storeOverlaySize` is removed.**

The guest's writable scratch — the writable `/nix/store` overlay, the workspace
clone and its build artifacts, the `cargo`/`rustup`/XDG caches, and the guest
Nix database — now lives on per-instance **sparse disk images** instead of a
tmpfs. This lifts the old cap (a fraction of `mem`) that let a single Rust
`target/` exhaust guest RAM: capacity now scales with host disk, and peak RAM
tracks the working set. The guest root `/` stays a tmpfs.

**Action required only if you set `storeOverlaySize`.** That single tmpfs-size
string is gone, replaced by three image sizes (in MiB, sparse):
`storeVolumeSize` (default `16384`), `scratchVolumeSize` (default `32768`), and
`dbVolumeSize` (default `4096`). Rename and re-express in MiB — e.g.
`storeOverlaySize = "8G"` → `storeVolumeSize = 8192`. If you never set it, no
action is needed; the defaults are generous and the images are sparse, so host
disk usage tracks real content rather than these caps.

Two behavioral notes, no action:

- A **named** instance keeps its images across a stop/restart, so warm build
  caches survive a pause. As a consequence, its guest Nix database is seeded
  from the host **once** (on first launch) and then accumulates the agent's own
  in-VM registrations; a resumed instance therefore does **not** pick up host
  paths built _after_ its first launch. Discard it with `sandbox:stop --remove`
  to re-seed from a fresh host snapshot. Ephemeral instances seed every launch
  as before.
- Prompting a **paused** instance now auto-starts it (see below), so its
  disk-backed caches are warm when the work resumes.

**`sandbox:prompt` auto-starts a paused instance — no action required.**

Prompting a named instance that was stopped (but kept) now restarts it — booting
and arming the channel (~30–60s) before delivering the turn — instead of hanging
against the powered-off VM. The live conversation does not survive a pause (the
VM's RAM is gone); only the pushed branch does, so the resumed agent begins a
fresh session on top of its branch. Phrase such a prompt to stand on its own.

## 0.1.9

No action required.

## 0.1.8

**`lib.sandbox`: the guest now imports the host Nix DB by default — no action required in normal use**

`importHostStoreDb` defaults to `true`, so a launched sandbox now snapshots the
host's Nix database and the guest reuses every path the host has already built
(e.g. a `nix develop` toolchain) instead of re-downloading it. This is
transparent: it only changes what the guest's `nix` treats as valid, adds no
read exposure (the whole host store was already mounted read-only), and falls
back to the previous system-only behavior if the snapshot is missing or a
host/guest Nix schema mismatch is detected — so a sandbox always boots.

Two things worth knowing:

- Each launch writes a ~150 MB `nix-db.sqlite` into the per-instance host state
  dir and the guest copies it in at boot. For a persistent (`--name`d) instance
  this lives alongside its other state until teardown.
- To restore the old behavior (substitute everything from the allowlisted
  caches), pass `importHostStoreDb = false`.

## 0.1.7

No action required.

## 0.1.6

No action required.

## 0.1.5

No action required.

## 0.1.4

**`lib.sandbox`: named instances are suffixed with random entropy — action only if you script instance names**

A provided `--name foo` now boots an instance named `foo-<8 hex>` (e.g.
`foo-a3f9c2d1`) rather than `foo`. This makes every launch a fresh,
collision-free instance instead of a silent resume of an older same-named branch
— an easy footgun before. Two consequences:

- **Drive and resume by the full suffixed name.** Every other command
  (`sandbox:prompt` / `status` / `fetch` / `stop`) and a later resume key off
  the full name, not the bare `--name`. The full name is printed at launch and
  by `sandbox:stop`. If you have a script that assumes the instance name equals
  the `--name` you passed, capture and reuse the printed name instead.
- **Re-passing the bare `--name foo` mints a NEW instance**, it no longer
  resumes the old branch. To resume, relaunch with the full suffixed name. A
  name that already carries the 8-hex suffix is not re-suffixed, so passing a
  printed name back is safe and idempotent.

## 0.1.3

No action required.

## 0.1.2

No action required.

## 0.1.1

A small release: no library argument or output signatures changed, so a normal
upgrade needs no edits. The one behavioral change worth knowing is below; the
rest is additive or a bug fix (see [`CHANGELOG.md`](CHANGELOG.md)).

**`lib.sandbox`: `sandbox:status` now exits non-zero on a failed preflight — action only if you script its exit code**

A bare `sandbox:status` now runs an environment preflight before listing
instances (it prints an `environment:` block verifying each declared secret at
its host source and checking for `/dev/vhost-vsock`) and **exits with the count
of missing prerequisites** instead of always exiting `0`. The instance listing
is unchanged.

This is a feature — the exit status is now a usable launch gate — but if you
have a script or CI step that runs a bare `sandbox:status` and treats a non-zero
exit as failure, it will now fail when a prerequisite is missing rather than
silently succeeding. Pass an explicit instance name (`sandbox:status <inst>`) to
get just that instance's details without the preflight gate.

**`lib.sandbox`: guest push to the 9p mirror now works — no action needed**

The per-instance bare mirror is now shared into the guest with
`security_model=mapped-xattr` (was `none`), so the unprivileged in-guest agent
owns the files it creates and `git push` back to the host succeeds. Previously
the push failed and no work crossed the sandbox boundary. This is a pure fix; no
consumer change is required.

## 0.1.0

The first tagged release. The notes below matter to anyone who was tracking
untagged `main`; on a fresh install there is nothing to migrate.

**Action required: transitive dependency inheritance (`lib.rust`)**

Katsuobushi now owns its infrastructure dependencies (`crane`, `nix-filter`,
`rust-overlay`, and `microvm` for the sandbox lib) and passes them through to
consumers transitively. Two consequences for `lib.rust` callers:

1. **Drop the infra inputs and arguments.** `lib.rust` no longer requires
   `crane`, `nix-filter` (`filter`), or `rust-overlay` — it inherits them from
   Katsuobushi. Your consumer flake collapses from six inputs to two (plus
   `flake-utils`):

   ```nix
   # Before
   inputs = {
     nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
     flake-utils.url = "github:numtide/flake-utils";
     katsuobushi.url = "github:cdata/katsuobushi";
     crane.url = "github:ipetkov/crane";
     nix-filter.url = "github:numtide/nix-filter";
     rust-overlay = { url = "github:oxalica/rust-overlay"; inputs.nixpkgs.follows = "nixpkgs"; };
   };
   # ...and the call threaded them:
   rustHelpers = katsuobushi.lib.rust { inherit pkgs crane; filter = nix-filter.lib; /* ... */ };

   # After
   inputs = {
     nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
     flake-utils.url = "github:numtide/flake-utils";
     katsuobushi.url = "github:cdata/katsuobushi";
     katsuobushi.inputs.nixpkgs.follows = "nixpkgs";   # unify on your nixpkgs
   };
   # ...and the call no longer names them:
   rustHelpers = katsuobushi.lib.rust { inherit pkgs; /* ... */ };
   ```

   Each infra dep remains an _optional_ argument, so you can still override one
   per-call (`katsuobushi.lib.rust { …; crane = myCrane; }`) or flake-wide
   (`inputs.katsuobushi.inputs.crane.follows = "crane";`).

2. **Drop `(import rust-overlay)` from your overlays.** `lib.rust` now applies
   rust-overlay internally, so `pkgs` is plain nixpkgs + the katsuobushi
   overlay:

   ```nix
   # Before
   pkgs = import nixpkgs { inherit system; overlays = [ (import rust-overlay) katsuobushi.overlays.default ]; };
   # After
   pkgs = import nixpkgs { inherit system; overlays = [ katsuobushi.overlays.default ]; };
   ```

**Trade-off:** menu-only consumers now pull `crane`/`microvm` into their
transitive `flake.lock` (they are not _built_ unless used, and `nixpkgs.follows`
prevents nixpkgs duplication). This is the accepted price of a dramatically
smaller consumer flake.

**New library: `lib.sandbox`**

`katsuobushi.lib.sandbox` assembles a `microvm.nix` guest that boots into a
working dev environment in which an agent harness (Claude Code by default) can
run with its blast radius bounded by a real VM. It ships born with the
transitive-dependency pattern (no legacy signature). It returns `apps.sandbox`
(`nix run .#sandbox`), `menuCommands` (`sandbox:start`, `sandbox:prompt`,
`sandbox:status`, `sandbox:fetch`, `sandbox:stop`), `checks.sandbox` (builds the
guest image), and `nixosConfiguration`. Scaffold a worked example with
`nix flake init -t github:cdata/katsuobushi#sandbox`; see
[`lib/sandbox/README.md`](lib/sandbox/README.md).

**Action required: `lib.markdown` uses Prettier, scoped by include/exclude**

`lib.markdown` switched its engine from `rumdl` to [Prettier], which handles GFM
tables natively (rumdl misidentified them). The argument and output surface
changed with it.

**Arguments.** `docsDir` is gone; scope is now two workspace-relative glob
lists, `include` and `exclude`, plus a `name` that labels this invocation's
outputs.

| Old arg                       | New arg(s)                                                  |
| ----------------------------- | ----------------------------------------------------------- |
| `docsDir`                     | `include` (globs, default `[ "**/*.md" ]`) + `name` (label) |
| —                             | `exclude` (globs → a Prettier ignore file, default `[ ]`)   |
| `settings` (rumdl rule table) | `settings` (Prettier options — different keys)              |

```nix
# Before (rumdl)
markdown = katsuobushi.lib.markdown {
  inherit pkgs;
  workspaceRoot = ./.;
  docsDir = "docs";
  settings = { MD013.line-length = 100; };
};

# After (Prettier)
markdown = katsuobushi.lib.markdown {
  inherit pkgs;
  workspaceRoot = ./.;
  name = "docs"; # labels `format:docs` / `lint:docs` and the `docs` check
  include = [ "docs" ]; # path(s)/glob(s); Prettier expands globs, honors .gitignore
  # exclude = [ "docs/vendor/**" ];
  settings = { printWidth = 100; }; # Prettier options, not rumdl rules
};
```

**`settings` is now [Prettier options][options]**, merged over the defaults
(`proseWrap = "always"`, `printWidth = 80`, `tabWidth = 2`). Translate the rumdl
rules you relied on — e.g. `MD013.line-length = 100` → `printWidth = 100`. Rules
with no Prettier equivalent simply disappear: Prettier does not flag inline HTML
or a missing top-level heading, so the `MD033` / `MD041` opt-outs some configs
needed for an HTML hero banner are **no longer necessary** — drop them.

**Default scope changed.** The old `docsDir` default was `"design"`; the new
`include` default is every tracked `.md` file (`[ "**/*.md" ]`). If you relied
on the old default to lint only `design/`, set `include = [ "design" ]`
explicitly.

**Outputs.** `rumdl` / `rumdlConfig` became `prettier` / `prettierConfig` /
`prettierIgnore` (update dev-shell `nativeBuildInputs` from `markdown.rumdl` to
`markdown.prettier`). Each invocation contributes its OWN namespaced pair of
menu commands — `format:<name>` (rewrite in place) and `lint:<name>` (read-only
check) — and its own check `checks.<name>`; there is no shared/global command.

**Behavioral notes.** Both commands run from the repository root; `include`
becomes Prettier's path arguments (everything matched is parsed as Markdown via
`--parser markdown`, so point `include` at Markdown). The check runs from the
workspace root, so every included file must be **tracked** — a flake check
cannot reach `.gitignore`'d paths, which are not part of the flake source;
format those with the menu command instead.

**Action required: `lib.rust`: renamed input arguments**

The two input-list arguments were renamed to match nixpkgs vocabulary (the old
`buildInputs` confusingly fed `nativeBuildInputs`, and `libraries` fed
`buildInputs`).

| Old arg       | New arg             | Feeds                                        |
| ------------- | ------------------- | -------------------------------------------- |
| `buildInputs` | `nativeBuildInputs` | derivation `nativeBuildInputs` (build tools) |
| `libraries`   | `buildInputs`       | derivation `buildInputs` (link libraries)    |

```nix
# Before
rustHelpers = katsuobushi.lib.rust {
  inherit pkgs crane;
  # ...
  buildInputs = with pkgs; [ pkg-config ];   # build tools
  libraries   = with pkgs; [ webkitgtk ];    # link libs
};

# After
rustHelpers = katsuobushi.lib.rust {
  inherit pkgs crane;
  # ...
  nativeBuildInputs = with pkgs; [ pkg-config ];   # build tools
  buildInputs       = with pkgs; [ webkitgtk ];    # link libs
};
```

Both now default to `[ ]` (previously `buildInputs` was required), so tool-only
projects can omit them entirely.

**`lib.rust`: wasm-bindgen version is derived — action required for non-default wasm builds**

The `wasm-bindgen-cli` version is no longer hard-pinned in the lib; it is read
from your `Cargo.lock`. The lib ships hashes for **0.2.108** as the default.

- If you build wasm **and** your lock file pins a different `wasm-bindgen`, eval
  now fails fast with a copy-pasteable fix (previously you would have silently
  received a mismatched 0.2.108 CLI — a latent runtime bug):

  ```nix
  rustHelpers = katsuobushi.lib.rust {
    # ...
    wasmBindgenHashes."0.2.99" = {
      hash      = pkgs.lib.fakeHash;   # build once, copy the real hash from the error
      cargoHash = pkgs.lib.fakeHash;
    };
  };
  ```

- If you are on 0.2.108, or you do not build wasm: no change needed. The
  `Cargo.lock` read is lazy, so native-only projects and the bare template never
  trigger it.

[Prettier]: https://prettier.io
[options]: https://prettier.io/docs/options
