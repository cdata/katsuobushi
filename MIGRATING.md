# Migrating Katsuobushi

Breaking and behavioral changes to the Katsuobushi libraries, **newest first**.

Katsuobushi is versioned with Git tags (SemVer); pin a release as
`github:cdata/katsuobushi/v0.1.0`. While in `0.x`, any release may break.

Each version heading below covers the changes **from the version immediately
beneath it up to that version**. The top heading is the current release. `0.1.0`
is the first tagged release, so it covers everything up to the first tag — i.e.
the changes anyone tracking untagged `main` should know about.

## 0.1.1

A small release: no library argument or output signatures changed, so a normal
upgrade needs no edits. The one behavioral change worth knowing is below; the
rest is additive or a bug fix (see [`CHANGELOG.md`](CHANGELOG.md)).

### `lib.sandbox`: `sandbox:status` now exits non-zero on a failed preflight — action only if you script its exit code

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

### `lib.sandbox`: guest push to the 9p mirror now works — no action needed

The per-instance bare mirror is now shared into the guest with
`security_model=mapped-xattr` (was `none`), so the unprivileged in-guest agent
owns the files it creates and `git push` back to the host succeeds. Previously
the push failed and no work crossed the sandbox boundary. This is a pure fix; no
consumer change is required.

## 0.1.0

The first tagged release. The notes below matter to anyone who was tracking
untagged `main`; on a fresh install there is nothing to migrate.

### Transitive dependency inheritance (`lib.rust`) — action required

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

### New library: `lib.sandbox`

`katsuobushi.lib.sandbox` assembles a `microvm.nix` guest that boots into a
working dev environment in which an agent harness (Claude Code by default) can
run with its blast radius bounded by a real VM. It ships born with the
transitive-dependency pattern (no legacy signature). It returns `apps.sandbox`
(`nix run .#sandbox`), `menuCommands` (`sandbox:start`, `sandbox:prompt`,
`sandbox:status`, `sandbox:fetch`, `sandbox:stop`), `checks.sandbox` (builds the
guest image), and `nixosConfiguration`. Scaffold a worked example with
`nix flake init -t github:cdata/katsuobushi#sandbox`; see
[`lib/sandbox/README.md`](lib/sandbox/README.md).

### `lib.markdown`: now Prettier, scoped by include/exclude — action required

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

### `lib.rust`: renamed input arguments — action required

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

### `lib.rust`: wasm-bindgen version is derived — action required for non-default wasm builds

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

### `lib.rust`: informational changes — no action needed

- **`buildCrate` gained a `target` argument** (`{ target ? null, ... }`).
  Omitting it builds for the host, exactly as before. New:
  `buildCrate { target = "wasm32-wasip3"; ... }` builds for any triple. Note
  that `target` is now a reserved attribute name on `buildCrate` and
  `buildTestArchive` — if you were passing a `target` attribute straight through
  to crane, it is now intercepted.
- **`buildWasmCrate` / `buildTrunkCrate` are unchanged** — same names, same
  browser (`wasm32-unknown-unknown`) behavior, including the wasm-bindgen /
  wasm-opt / esbuild tooling.
- **Crate version derivation:** the hardcoded `version = "0.1.0"` is gone; crane
  now derives the version from `Cargo.toml`. Build-artifact version metadata and
  store-path names change, but flake output attribute names do not.
- **Deps-derivation `pname`s changed:** the host deps stay
  `<project>-workspace-deps`; the wasm deps went from
  `<project>-workspace-wasm-deps` to
  `<project>-workspace-deps-wasm32-unknown-unknown`. Internal only — your
  `checks` / `packages` attribute names are unaffected.
- **`sourceInclude` argument** added (defaults to the previous hard-coded list:
  `.cargo`, `Cargo.lock`, `Cargo.toml`, `rust-toolchain.toml`, `rust`). Override
  it when your crates do not live under `rust/`.
- **Derivation name prefix** now derives from `projectId` (e.g.
  `my-org/my-project` → `my-project`) instead of a hardcoded `fox-star`.

[Prettier]: https://prettier.io
[options]: https://prettier.io/docs/options
