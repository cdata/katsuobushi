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
{
  pkgs,
  filter,
  crane,
  workspaceRoot,
  buildInputs,
  # A stable, globally-unique identifier for the project, supplied by the
  # importer (e.g. "my-org/my-project"). Namespaces the out-of-tree Cargo
  # target directory (`cargoTargetDir`) so unrelated projects that happen to
  # share a workspace name don't collide in the user's global cache. Qualify
  # it with the owner/origin to be safe.
  projectId,
}:

let
  # Cargo dependencies that are Git repositories. When this table is empty,
  # crane falls back to `builtins.fetchGit` (impure, network-required at
  # eval). To pin a dependency for fully offline builds, add an entry whose
  # key is the exact `source` string from `Cargo.lock` (including the `git+`
  # prefix and the trailing `#<resolved-rev>`) and whose value is the
  # SRI-encoded sha256 of the checked-out tree. To bootstrap a new entry,
  # use `pkgs.lib.fakeHash` and let the failing build report the real hash.
  cargoGitDependencies = {
    "git+https://github.com/dialog-db/dialog-db.git?rev=af442cac90d72c9da8be9c71799f497bddc62f0b#af442cac90d72c9da8be9c71799f497bddc62f0b" =
      "sha256-wOHAALeYydBd05RQw0+Ge3rJF+HVuW8EFoPMzYOLpVs=";
  };

  # Filter source to only Rust-relevant files.
  rustSource = filter {
    root = workspaceRoot;
    include = [
      ".cargo"
      "Cargo.lock"
      "Cargo.toml"
      "rust-toolchain.toml"
      "rust"
    ];
  };

  rustToolchain = pkgs.rust-bin.fromRustupToolchainFile (workspaceRoot + "/rust-toolchain.toml");
  craneLib = (crane.mkLib pkgs).overrideToolchain (_: rustToolchain);

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

  # wasm-bindgen-cli must match the wasm-bindgen crate version used by the
  # workspace exactly, or the generated bindings will fail to load. The pin
  # below MUST track `wasm-bindgen` in the root `Cargo.toml`'s
  # `[workspace.dependencies]` table.
  wasm-bindgen-cli =
    with pkgs;
    buildWasmBindgenCli rec {
      src = fetchCrate {
        pname = "wasm-bindgen-cli";
        version = "0.2.108";
        hash = "sha256-UsuxILm1G6PkmVw0I/JF12CRltAfCJQFOaT4hFwvR8E=";
      };

      cargoDeps = rustPlatform.fetchCargoVendor {
        inherit src;
        inherit (src) pname version;
        hash = "sha256-iqQiWbsKlLBiJFeqIYiXo3cqxGLSjNM8SOWXGM9u43E=";
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

  nativeBuildInputs = buildInputs ++ [
    rustToolchain
  ];

  commonAttributes = {
    src = rustSource;
    strictDeps = true;
    inherit nativeBuildInputs;
    buildInputs = [ ];

    # Git dependencies with hashes for offline evaluation. Crane will
    # automatically find Cargo.lock from src.
    outputHashes = cargoGitDependencies;
    doCheck = false;
  };

  # Build native dependencies once for entire workspace.
  nativeArtifacts = craneLib.buildDepsOnly (
    commonAttributes
    // {
      pname = "fox-star-workspace-deps";
    }
  );

  wasmAttributes = commonAttributes // {
    CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
  };

  wasmArtifacts = craneLib.buildDepsOnly (
    wasmAttributes
    // {
      pname = "fox-star-workspace-wasm-deps";
    }
  );

  buildCrate =
    attributes:
    craneLib.buildPackage (
      commonAttributes
      // {
        version = "0.1.0";
        cargoArtifacts = nativeArtifacts;
      }
      // attributes
    );

  buildWasmCrate =
    attributes:
    craneLib.buildPackage (
      wasmAttributes
      // {
        cargoArtifacts = wasmArtifacts;

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
      wasmAttributes
      // {
        cargoArtifacts = wasmArtifacts;
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
    let
      targetAttributes = if target == "wasm32-unknown-unknown" then wasmAttributes else commonAttributes;

      targetArtifacts = if target == "wasm32-unknown-unknown" then wasmArtifacts else nativeArtifacts;
    in
    craneLib.mkCargoDerivation (
      targetAttributes
      // {
        pname = "tests-${name}";
        cargoArtifacts = targetArtifacts;

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
        nativeBuildInputs = (targetAttributes.nativeBuildInputs or [ ]) ++ [ pkgs.cargo-nextest ];
      }
    );

  cargoChecks = {
    clippy = craneLib.cargoClippy (
      commonAttributes
      // {
        pname = "fox-star-cargo-clippy-check";
        cargoArtifacts = nativeArtifacts;
        cargoClippyExtraArgs = "--all-targets --all-features -- -D warnings";
      }
    );

    rustfmt = craneLib.cargoFmt {
      src = rustSource;
      pname = "fox-star-cargo-fmt-check";
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
