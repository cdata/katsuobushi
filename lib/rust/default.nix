# This module contains helpers for building Rust-based artifacts.
# It exists because we're using [crane](https://crane.dev) to do the building,
# and correct crane usage is somewhat nuanced compared to the built-in Nix
# tools (such as buildRustPackage). Using the helpers here means you can
# maximize the amount of sharing / re-use of dependencies across Rust
# projects.
#
# Adapted from dialog-db's nix/rust.nix
# (https://github.com/dialog-db/dialog-db/blob/main/nix/rust.nix), via
# wasm-component-model-polyfill's port of the same.
#
# This module is partial-applied by the Katsuobushi flake with the pinned infra
# dependencies (`{ crane, nix-filter, rust-overlay }`); the resulting function is
# what consumers call as `katsuobushi.lib.rust { inherit pkgs; ... }`. Each infra
# dep is exposed as an optional argument defaulting to the pinned version, so a
# consumer can still override one per-call (e.g. `crane = myCrane;`) or flake-wide
# via `inputs.katsuobushi.inputs.crane.follows`. The consumer passes plain `pkgs`;
# the rust-overlay is applied internally (see `pkgsWithRust`), so they no longer
# add it to their own overlays.
defaults:
{
  pkgs,
  workspaceRoot,
  # Infra dependencies, defaulting to the versions Katsuobushi pins. Override
  # per-call only when you need a different pin than the toolkit ships.
  crane ? defaults.crane,
  filter ? defaults.nix-filter.lib,
  rust-overlay ? defaults.rust-overlay,
  # Build-time tools made available to every derivation (e.g. `pkg-config`,
  # `wrapGAppsHook`). These go into each derivation's `nativeBuildInputs`.
  nativeBuildInputs ? [ ],
  # Target libraries every derivation links against (e.g. WebKitGTK and the
  # GTK family for a Tauri app). These go into each derivation's `buildInputs`
  # so that — under `strictDeps` — a `*-sys` crate's build script resolves them
  # on the target pkg-config path. Defaults to empty for tool-only projects.
  buildInputs ? [ ],
  # Workspace-relative paths kept by the Rust source filter. Defaults to a
  # layout with crates under `rust/`; override when your crates live elsewhere
  # (for example at the repository root).
  sourceInclude ? [
    ".cargo"
    "Cargo.lock"
    "Cargo.toml"
    "rust-toolchain.toml"
    "rust"
  ],
  # A stable, globally-unique identifier for the project, supplied by the
  # importer (e.g. "my-org/my-project"). Namespaces the out-of-tree Cargo
  # target directory (`cargoTargetDir`) so unrelated projects that happen to
  # share a workspace name don't collide in the user's global cache. Qualify
  # it with the owner/origin to be safe.
  projectId,
  # wasm-bindgen-cli must exactly match the `wasm-bindgen` crate version your
  # workspace resolves, or generated bindings fail to load at runtime. The
  # version is read from `Cargo.lock` automatically; all you may need to supply
  # are the fixed-output hashes for that version. This lib ships hashes for the
  # version it was last validated against (see `defaultWasmBindgenHashes`); if
  # your lock file pins a different one, add an entry keyed by the exact version
  # string. Bootstrap both fields with `pkgs.lib.fakeHash` and let the failing
  # build report the real values.
  wasmBindgenHashes ? { },
  # Cargo dependencies that are Git repositories, supplied by the importer.
  # Each project pins its own upstreams, so this table lives in the consuming
  # flake rather than here. When it is empty (the default), crane falls back
  # to `builtins.fetchGit` (impure, network-required at eval). To pin a
  # dependency for fully offline builds, add an entry whose key is the exact
  # `source` string from `Cargo.lock` (including the `git+` prefix and the
  # trailing `#<resolved-rev>`) and whose value is the SRI-encoded sha256 of
  # the checked-out tree. To bootstrap a new entry, use `pkgs.lib.fakeHash`
  # and let the failing build report the real hash.
  cargoGitDependencies ? { },
}:

let
  # Apply rust-overlay internally so the consumer passes plain `pkgs` without
  # adding `(import rust-overlay)` to their own overlays. An overlay is
  # `final: prev:`, so extending `pkgs` with it yields `pkgs.rust-bin`. This is a
  # superset of `pkgs`, safe to use everywhere a toolchain-aware package is
  # wanted; we use it specifically for `rust-bin` below.
  pkgsWithRust = pkgs.extend (import rust-overlay);

  # Filter source to only Rust-relevant files.
  rustSource = filter {
    root = workspaceRoot;
    include = sourceInclude;
  };

  rustToolchain = pkgsWithRust.rust-bin.fromRustupToolchainFile (workspaceRoot + "/rust-toolchain.toml");
  craneLib = (crane.mkLib pkgs).overrideToolchain (_: rustToolchain);

  # The project's bare name, used as the `pname` prefix for the shared
  # workspace derivations below. Derived from `projectId` by dropping any
  # owner/origin qualifier (e.g. "my-org/my-project" → "my-project") so the
  # derivations carry a readable, project-specific name.
  projectName = baseNameOf projectId;

  # Known fixed-output hashes for `wasm-bindgen-cli`, keyed by version. The
  # entry here tracks the version this lib was last validated against and acts
  # as the default; consumers extend or override it via the `wasmBindgenHashes`
  # argument when their Cargo.lock pins a different `wasm-bindgen`.
  defaultWasmBindgenHashes = {
    "0.2.108" = {
      hash = "sha256-UsuxILm1G6PkmVw0I/JF12CRltAfCJQFOaT4hFwvR8E=";
      cargoHash = "sha256-iqQiWbsKlLBiJFeqIYiXo3cqxGLSjNM8SOWXGM9u43E=";
    };
  };
  wasmBindgenHashesResolved = defaultWasmBindgenHashes // wasmBindgenHashes;

  # Out-of-tree `CARGO_TARGET_DIR` for host-side cargo. When the project has
  # no git working tree — a non-colocated jj repo, or any jj workspace — every
  # flake command copies the whole untracked tree into the store; keeping
  # `target/` out of it avoids a multi-gigabyte copy. Harmless under plain
  # git, where `target/` is already gitignored out of the copy. See
  # NixOS/nix#15651: https://github.com/NixOS/nix/issues/15651
  #
  # Keyed by `projectId` (namespace) plus the runtime workspace basename and a
  # short `$PWD` hash, so sibling workspaces and same-named checkouts don't
  # collide. Uses `$PWD`, not `workspaceRoot` (which pure eval collapses to a
  # churning `…-source` path); expanded by the shell via `rustEnvironmentHook`.
  cargoTargetDir = ''''${XDG_CACHE_HOME:-$HOME/.cache}/cargo-target/${projectId}/$(basename "$PWD")-$(printf '%s' "$PWD" | sha256sum | cut -c1-12)'';

  # A ready-to-splice dev-shell fragment that prepares the host Rust
  # environment — currently steering cargo at the out-of-tree target dir
  # above. Concatenate it into your devShell's `shellHook`. It must run in the
  # shell rather than via mkShell's `env`, which bakes values literally and
  # would not expand `$PWD`; cargo creates the dir on first build, so no mkdir
  # is needed.
  rustEnvironmentHook = ''
    export CARGO_TARGET_DIR="${cargoTargetDir}"
  '';

  # wasm-bindgen-cli, built to match the `wasm-bindgen` version resolved in the
  # workspace's Cargo.lock. The version is derived; only the per-version hashes
  # come from `wasmBindgenHashesResolved`. The Cargo.lock read is lazy — forced
  # only when something actually builds a wasm artifact — so tool-only and
  # native-only consumers (and the bare template) never need a lock file.
  wasm-bindgen-cli =
    let
      cargoLock = builtins.fromTOML (builtins.readFile (workspaceRoot + "/Cargo.lock"));
      wasmBindgenPackages = builtins.filter (p: p.name == "wasm-bindgen") (cargoLock.package or [ ]);
      version =
        if wasmBindgenPackages == [ ] then
          throw "lib/rust/default.nix: building wasm-bindgen-cli requires a `wasm-bindgen` entry in Cargo.lock, but none was found."
        else
          (builtins.head wasmBindgenPackages).version;
      hashes =
        wasmBindgenHashesResolved.${version} or (throw ''
          lib/rust/default.nix: Cargo.lock pins wasm-bindgen ${version}, but no wasm-bindgen-cli hashes are known for it. Add an override:

            wasmBindgenHashes."${version}" = {
              hash = pkgs.lib.fakeHash;
              cargoHash = pkgs.lib.fakeHash;
            };

          then let the failing build report the real hashes.'');
    in
    with pkgs;
    buildWasmBindgenCli rec {
      src = fetchCrate {
        pname = "wasm-bindgen-cli";
        inherit version;
        inherit (hashes) hash;
      };

      cargoDeps = rustPlatform.fetchCargoVendor {
        inherit src;
        inherit (src) pname version;
        hash = hashes.cargoHash;
      };
    };

  # Workspace hygiene: enforces that every crate inherits its dependencies
  # from `[workspace.dependencies]` rather than pinning its own versions.
  enforce-workspace-deps =
    with pkgs;
    rustPlatform.buildRustPackage rec {
      pname = "cargo-enforce-shared-workspace-deps";
      version = "0.1.0";
      buildInputs = [ rustToolchain ];

      src = fetchCrate {
        inherit pname version;
        sha256 = "sha256-XOdKeg9tNt/HT+WO9QKtdX3fUMUssVTlXRV0LOIMMzc=";
      };

      cargoHash = "sha256-O6DQXK8/VVwTLuFlSyh8jtBJyAFMfAUNXnTeMWrXTCM=";
    };

  commonAttributes = {
    src = rustSource;
    strictDeps = true;
    nativeBuildInputs = nativeBuildInputs ++ [ rustToolchain ];
    inherit buildInputs;

    # Git dependencies with hashes for offline evaluation. Crane will
    # automatically find Cargo.lock from src.
    outputHashes = cargoGitDependencies;
    doCheck = false;
  };

  # Crane attribute overlay for a given Cargo target triple. `null` selects the
  # host target. Targets are named by their full triple so that members of a
  # family that don't share a toolchain stay distinct — e.g.
  # `wasm32-unknown-unknown` (browser, driven by wasm-bindgen) versus
  # `wasm32-wasip3` (WASI).
  attributesForTarget =
    target: commonAttributes // (if target == null then { } else { CARGO_BUILD_TARGET = target; });

  # Deps-only artifacts for a target triple. Each triple has its own dependency
  # closure (distinct `std`, distinct `*-sys` builds), so they are never shared
  # across targets. Equal `target` values evaluate to the same derivation, so
  # crane builds each target's deps exactly once even across many crates.
  depsForTarget =
    target:
    craneLib.buildDepsOnly (
      attributesForTarget target
      // {
        pname = "${projectName}-workspace-deps" + (if target == null then "" else "-${target}");
      }
    );

  # Host-target deps, also consumed by the cargo checks below.
  nativeArtifacts = depsForTarget null;

  # The browser wasm target. The wasm-bindgen / wasm-opt / esbuild toolchain in
  # `buildWasmCrate` and `buildTrunkCrate` is specific to this triple; other
  # wasm targets (e.g. `wasm32-wasip3`) go through the generic `buildCrate` with
  # an explicit `target`.
  browserWasmTarget = "wasm32-unknown-unknown";

  # Build a crate for any Cargo target. `target` is a triple string (e.g.
  # "wasm32-wasip3") or `null`/omitted for the host; all other attributes pass
  # through to crane.
  buildCrate =
    { target ? null, ... }@attributes:
    craneLib.buildPackage (
      attributesForTarget target
      // {
        cargoArtifacts = depsForTarget target;
      }
      // removeAttrs attributes [ "target" ]
    );

  # Build a browser-targeted wasm crate (wasm32-unknown-unknown), surfacing the
  # pinned wasm-bindgen / wasm-opt / esbuild tools.
  buildWasmCrate =
    attributes:
    craneLib.buildPackage (
      attributesForTarget browserWasmTarget
      // {
        cargoArtifacts = depsForTarget browserWasmTarget;

        # These *_BIN envvars are conventional and consumed by build scripts
        # such as `worker-build`; they are also a convenient way to surface
        # the pinned tools to a custom buildPhase.
        WASM_OPT_BIN = "${pkgs.binaryen}/bin/wasm-opt";
        WASM_BINDGEN_BIN = "${wasm-bindgen-cli}/bin/wasm-bindgen";
        ESBUILD_BIN = "${pkgs.esbuild}/bin/esbuild";
      }
      // attributes
    );

  buildTrunkCrate =
    attributes:
    let
      crateRoot = builtins.dirOf attributes.trunkConfig;
    in
    craneLib.buildTrunkPackage (
      attributesForTarget browserWasmTarget
      // {
        cargoArtifacts = depsForTarget browserWasmTarget;
        preBuild = ''
          cd ${crateRoot}
        '';
        inherit wasm-bindgen-cli;
      }
      // attributes
    );

  buildTestArchive =
    {
      name,
      args ? "",
      target ? null,
    }:
    craneLib.mkCargoDerivation (
      attributesForTarget target
      // {
        pname = "tests-${name}";
        cargoArtifacts = depsForTarget target;

        buildPhaseCargoCommand = ''
          cargo nextest archive \
            ${args} \
            --archive-file ./tests-${name}.tar.zst
        '';

        installPhaseCommand = ''
          mkdir -p $out
          cp ./*.tar.zst $out/
        '';

        doInstallCargoArtifacts = false;
        nativeBuildInputs = (attributesForTarget target).nativeBuildInputs ++ [ pkgs.cargo-nextest ];
      }
    );

  cargoChecks = {
    clippy = craneLib.cargoClippy (
      commonAttributes
      // {
        pname = "${projectName}-cargo-clippy-check";
        cargoArtifacts = nativeArtifacts;
        cargoClippyExtraArgs = "--all-targets --all-features -- -D warnings";
      }
    );

    rustfmt = craneLib.cargoFmt {
      src = rustSource;
      pname = "${projectName}-cargo-fmt-check";
    };

    sharedWorkspaceDeps = buildCrate {
      pname = "shared-workspace-deps-check";
      buildPhase = ''
        ${enforce-workspace-deps}/bin/cargo-enforce-shared-workspace-deps
      '';
      installPhase = ''
        touch $out
      '';
    };
  };
in
{
  inherit
    buildCrate
    buildWasmCrate
    buildTrunkCrate
    buildTestArchive
    rustEnvironmentHook
    rustToolchain
    cargoChecks
    wasm-bindgen-cli
    ;
}
