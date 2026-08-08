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

  # The project's own cargo directory, as an absolute path in whatever root the
  # builds actually see (a store copy under flakes). Entries are matched against
  # this rather than against a bare `.cargo` substring: the file that matters is
  # `workspaceRoot/.cargo/config.toml` specifically, so a crate-local
  # `rust/demo/.cargo` or a `vendor/.cargo` says nothing about whether the root
  # config was carried in.
  cargoDirAbs = toString (workspaceRoot + "/.cargo");

  # Does `sourceInclude` carry the project's `.cargo/` into the Nix builds?
  #
  # nix-filter's `include` accepts a string, a path, or a matcher function. The
  # first two we can read; a function we cannot, so it counts as covering — that
  # is the one place we deliberately stay silent rather than risk crying wolf.
  #
  # Everything else is anchored at the root, in both the relative spelling
  # (".cargo", ".cargo/config.toml", with an optional leading "./") and the
  # absolute one (paths render absolute under `toString`). An entry naming some
  # *other* `.cargo` does not silence the warning, and neither does one pointing
  # outside the tree.
  cargoDirIncluded = builtins.any (
    entry:
    if !(builtins.isString entry || builtins.isPath entry) then
      true # a matcher function (or anything else we can't read) — assume covering
    else
      let
        raw = toString entry;
        rel = pkgs.lib.removePrefix "./" raw;
      in
      rel == ""
      || rel == "."
      || rel == ".cargo"
      || pkgs.lib.hasPrefix ".cargo/" rel
      || raw == cargoDirAbs
      || pkgs.lib.hasPrefix "${cargoDirAbs}/" raw
  ) sourceInclude;

  # `.cargo/config.toml` holds the flags cargo builds under — a linker choice,
  # a target-cpu, a rustflags block. When it exists but is filtered out of the
  # Nix source, every Nix-built artifact is compiled under *different* flags than
  # the dev shell uses, and cargo silently discards those artifacts rather than
  # reporting a mismatch: the operator sees only a long rebuild. The default
  # `sourceInclude` carries `.cargo`; this catches the project that overrode it
  # for an unrelated reason and dropped `.cargo` without noticing.
  #
  # NB: under flakes `workspaceRoot` is the store copy of the *tracked* tree, so
  # this sees a committed (or at least staged) config — an untracked one is
  # invisible here exactly as it is invisible to the build it would have
  # affected.
  cargoConfigPresent = builtins.pathExists (workspaceRoot + "/.cargo/config.toml");

  # The cargo config's content hash, recorded in every deps bundle's alignment
  # manifest. Content rather than path: two checkouts at different paths with an
  # identical config are aligned, and the same path with edited flags is not.
  # Gated on `cargoDirIncluded` as well as existence: when `.cargo` is filtered
  # out of the source, the build did NOT compile under that config, so recording
  # its hash would let a live shell holding the same file compare equal and read
  # as aligned in precisely the case 43bc8f exists to warn about. Null here means
  # "the build saw no cargo config", which is the truth we can defend.
  cargoConfigHash =
    if cargoConfigPresent && cargoDirIncluded then
      builtins.hashFile "sha256" (workspaceRoot + "/.cargo/config.toml")
    else
      null;

  # An explicit line list rather than a `''` block, so the paragraph breaks are
  # unambiguous at the call site. (Nix hang-indents the continuation lines under
  # its own "evaluation warning: " prefix when it renders this.)
  cargoConfigWarning = builtins.concatStringsSep "\n" [
    "katsuobushi.lib.rust: this project has a .cargo/config.toml, but `sourceInclude` does not carry `.cargo` into the Nix builds."
    ""
    "The flags in that file (rustflags, linker, target-cpu, …) apply to your dev shell but NOT to anything Nix builds. Cargo folds those flags into every unit hash, so a Nix-built artifact is silently discarded by an interactive cargo — you get a full rebuild and no error explaining why."
    ""
    "Add \".cargo\" to `sourceInclude` (it is in the default set), or drop the config file if it is no longer wanted."
  ];

  # Filter source to only Rust-relevant files.
  rustSource =
    pkgs.lib.warnIf (cargoConfigPresent && !cargoDirIncluded) cargoConfigWarning
      (filter {
        root = workspaceRoot;
        include = sourceInclude;
      });

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
    "0.2.126" = {
      hash = "sha256-H6Is3fiZVxZCfOMWK5dWMSrtn50VGv0sfdnsT+cTtyk=";
      cargoHash = "sha256-VucqkXbCi4qtQzY/HrXiDnbSURsagPsdNVMn1Tw3UiY=";
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

  # The Cargo build profile the helpers fall back to when a `profile` argument
  # is omitted. Crane drives the profile through a `CARGO_PROFILE` convention
  # (see crane's `configureCargoCommonVars` / `cargoWithProfile`): its default
  # build commands emit `--release` when `CARGO_PROFILE` is `release` and
  # `--profile <name>` otherwise. Release is left byte-identical to the
  # pre-profile behavior — we leave `CARGO_PROFILE` unset for it and rely on
  # crane's own `CARGO_PROFILE-release` default — so existing consumers see no
  # gratuitous rebuild.
  defaultProfile = "release";

  # The `CARGO_PROFILE` env attribute for a profile (empty for release, per the
  # note above) and the matching derivation-name suffix. The suffix keeps a
  # non-default profile's deps bundle from colliding with the release bundle in
  # the store.
  profileAttrs = profile: if profile == defaultProfile then { } else { CARGO_PROFILE = profile; };
  profileSuffix = profile: if profile == defaultProfile then "" else "-${profile}";

  # Crane attribute overlay for a given Cargo target triple and build profile.
  # `target == null` selects the host target; `profile` defaults to
  # `defaultProfile`. Targets are named by their full triple so that members of
  # a family that don't share a toolchain stay distinct — e.g.
  # `wasm32-unknown-unknown` (browser, driven by wasm-bindgen) versus
  # `wasm32-wasip3` (WASI).
  attributesFor =
    {
      target ? null,
      profile ? defaultProfile,
    }:
    commonAttributes
    // (if target == null then { } else { CARGO_BUILD_TARGET = target; })
    // profileAttrs profile;

  # Deps-only artifacts for a (target triple, profile) pair. Each pair has its
  # own dependency closure — distinct `std`, distinct `*-sys` builds, distinct
  # optimization — so bundles are never shared across targets or profiles. Equal
  # `{ target, profile }` values evaluate to the same derivation, so crane
  # builds each pair's deps exactly once even across many crates. Building the
  # top-level crate and this bundle under the same profile is what lets cargo
  # reuse the bundle instead of recompiling the closure.
  depsFor =
    {
      target ? null,
      profile ? defaultProfile,
    }:
    let
      pname =
        "${projectName}-workspace-deps"
        + (if target == null then "" else "-${target}")
        + profileSuffix profile;
    in
    craneLib.buildDepsOnly (
      let
        base = attributesFor { inherit target profile; };
      in
      base
      // {
        inherit pname;
        # Append to the *merged* attrs, not to `commonAttributes`: `attributesFor`
        # does not touch `nativeBuildInputs` today, but reaching past it would
        # silently drop whatever it adds the day that changes.
        nativeBuildInputs = base.nativeBuildInputs ++ [ pkgs.jq ];
        # Alignment manifest, written beside `target.tar.zst`. Cargo folds the
        # rustc identity, the target triple, the profile and the effective flags
        # into every unit hash, and (measured — see design/warm-artifacts.md
        # §10.1) it also refuses artifacts built against a different vendored
        # source directory. Any of those diverging makes this bundle *silently*
        # useless: cargo discards it and rebuilds, reporting nothing. Recording
        # them here is what lets a consumer refuse to seed and say why.
        #
        # `runHook postInstall` fires after crane's `mkdir -p $out` and before
        # its own artifact-install hook, so $out exists and nothing has packed
        # yet. jq rather than printf so a flag containing quotes cannot produce
        # malformed JSON.
        postInstall = ''
          jq -n \
            --argjson schema 2 \
            --arg pname ${pkgs.lib.escapeShellArg pname} \
            --arg profile ${pkgs.lib.escapeShellArg profile} \
            --arg target "''${CARGO_BUILD_TARGET:-${pkgs.stdenv.hostPlatform.rust.rustcTarget}}" \
            --arg rustc "$(rustc --version)" \
            --arg rustflags "''${RUSTFLAGS:-}" \
            --arg encodedRustflags "''${CARGO_ENCODED_RUSTFLAGS:-}" \
            --arg cargoBuildRustflags "''${CARGO_BUILD_RUSTFLAGS:-}" \
            --arg cargoConfigSha256 ${pkgs.lib.escapeShellArg (if cargoConfigHash == null then "" else cargoConfigHash)} \
            --arg vendorDir "''${cargoVendorDir:-}" \
            '{
               schema: $schema,
               pname: $pname,
               profile: $profile,
               target: $target,
               rustc: $rustc,
               rustflags: $rustflags,
               encodedRustflags: $encodedRustflags,
               cargoBuildRustflags: $cargoBuildRustflags,
               cargoConfigSha256: (if $cargoConfigSha256 == "" then null else $cargoConfigSha256 end),
               vendorDir: (if $vendorDir == "" then null else $vendorDir end)
             }' > "$out/manifest.json"
        '';
      }
    );

  # Host-target, default-profile deps, also consumed by the cargo checks below.
  nativeArtifacts = depsFor { };

  # Resolve the cargo configuration that a *live* invocation would actually see,
  # so the checker below compares against reality rather than a guess. Cargo
  # merges `.cargo/config.toml` from the cwd upward plus `$CARGO_HOME`, with the
  # nearest winning, and the decisive field for artifact reuse — measured, see
  # design/warm-artifacts.md §10.1 — is the `[source.crates-io] replace-with`
  # directory. That is a real TOML merge, not something to grep for; python's
  # stdlib tomllib does it honestly.
  resolveCargoConfig = pkgs.writeText "katsuobushi-resolve-cargo-config.py" ''
    import hashlib, json, os, sys, tomllib

    # Cargo errors out on a config it cannot parse, so "unreadable" and
    # "malformed" are reported to the caller rather than silently treated as
    # absent — a checker that says "no source replacement" when cargo would
    # refuse to run at all names the wrong cause, which is the one thing this
    # tool exists not to do.
    problems = []

    # `load` runs once per merge pass, so record each file's problem once rather
    # than repeating it per pass.
    def note(path, kind, detail):
        for existing in problems:
            if existing["file"] == path and existing["kind"] == kind:
                return
        problems.append({"file": path, "kind": kind, "detail": detail})

    def load(path):
        try:
            with open(path, "rb") as fh:
                return tomllib.load(fh)
        except tomllib.TOMLDecodeError as exc:
            note(path, "malformed", str(exc))
            return None
        except OSError as exc:
            note(path, "unreadable", str(exc))
            return None

    start = os.getcwd()
    # Nearest first, exactly as cargo walks it.
    chain, cur = [], start
    while True:
        # `.cargo/config` (no extension) is the legacy spelling; cargo still
        # reads it, and config.toml wins when both exist.
        for name in ("config.toml", "config"):
            candidate = os.path.join(cur, ".cargo", name)
            if os.path.isfile(candidate):
                chain.append(candidate)
        parent = os.path.dirname(cur)
        if parent == cur:
            break
        cur = parent
    home = os.environ.get("CARGO_HOME") or os.path.join(os.path.expanduser("~"), ".cargo")
    for name in ("config.toml", "config"):
        home_cfg = os.path.join(home, name)
        if os.path.isfile(home_cfg) and home_cfg not in chain:
            chain.append(home_cfg)

    # The workspace root is the nearest ancestor holding a Cargo.lock; that is the
    # directory the Nix side hashed a config from, so it is what we can compare.
    ws, cur = None, start
    while True:
        if os.path.isfile(os.path.join(cur, "Cargo.lock")):
            ws = cur
            break
        parent = os.path.dirname(cur)
        if parent == cur:
            break
        cur = parent

    ws_cfg_sha = None
    ws_cfg = os.path.join(ws, ".cargo", "config.toml") if ws else None
    if ws_cfg and os.path.isfile(ws_cfg):
        try:
            with open(ws_cfg, "rb") as fh:
                ws_cfg_sha = hashlib.sha256(fh.read()).hexdigest()
        except OSError as exc:
            note(ws_cfg, "unreadable", str(exc))

    # Merge shallowly, nearest wins, over the keys we care about.
    vendor_dir, replace_with, cfg_rustflags = None, None, None
    cfg_rustflags_source = None
    for path in reversed(chain):
        data = load(path)
        if not data:
            continue
        sources = data.get("source", {})
        crates_io = sources.get("crates-io", {})
        if "replace-with" in crates_io:
            replace_with = crates_io["replace-with"]
        build = data.get("build", {})
        if "rustflags" in build:
            cfg_rustflags = build["rustflags"]
            cfg_rustflags_source = path
    for path in reversed(chain):
        data = load(path) or {}
        if replace_with:
            entry = data.get("source", {}).get(replace_with, {})
            if "directory" in entry:
                # Cargo resolves a relative `directory` against the config file's
                # own location, not the cwd.
                vendor_dir = os.path.normpath(
                    os.path.join(os.path.dirname(os.path.dirname(path)), entry["directory"])
                    if not os.path.isabs(entry["directory"])
                    else entry["directory"]
                )

    json.dump(
        {
            "vendorDir": vendor_dir,
            "replaceWith": replace_with,
            "configFiles": chain,
            "workspaceRoot": ws,
            "workspaceConfigSha256": ws_cfg_sha,
            "configRustflags": cfg_rustflags,
            "configRustflagsSource": cfg_rustflags_source,
            "workspaceConfigPath": ws_cfg if (ws_cfg and os.path.isfile(ws_cfg)) else None,
            "problems": problems,
        },
        sys.stdout,
    )
  '';

  # Compare a deps bundle's alignment manifest against the environment a cargo
  # invocation would actually run under, and render the difference. Exit 0 means
  # the bundle's artifacts are reusable here; exit 1 means cargo would discard
  # them, and the output says which field diverged and what that implies.
  #
  # This is a *diagnostic*, not a gate: nothing here changes a build. It exists
  # because the failure it names is silent — the only symptom of a misaligned
  # bundle is a long rebuild, which is indistinguishable from an honest one. That
  # is also why it must never answer "aligned" on incomplete information: a false
  # green here is worse than no tool, because it sends the reader to look for the
  # problem somewhere it isn't.
  checkArtifactAlignment = pkgs.writeShellApplication {
    name = "katsuobushi-check-artifact-alignment";
    runtimeInputs = [
      pkgs.jq
      pkgs.coreutils
      pkgs.python3
    ];
    text = ''
      wantedProfile=""
      if [ "''${1:-}" = "--profile" ]; then
        wantedProfile="''${2:-}"
        shift 2 || true
      fi
      manifest="''${1:-}"
      if [ -z "$manifest" ]; then
        echo "usage: katsuobushi-check-artifact-alignment [--profile <name>] <manifest.json|bundle-dir>" >&2
        exit 2
      fi
      shift
      if [ "$#" -gt 0 ]; then
        # `check <bundle> --profile dev` used to be accepted and silently ignored.
        echo "katsuobushi: unexpected argument(s): $*" >&2
        echo "  usage: katsuobushi-check-artifact-alignment [--profile <name>] <manifest.json|bundle-dir>" >&2
        exit 2
      fi
      # Accept a bundle directory as a convenience — that is what a caller has.
      if [ -d "$manifest" ]; then
        manifest="$manifest/manifest.json"
      fi
      if [ ! -f "$manifest" ]; then
        echo "katsuobushi: no alignment manifest at $manifest" >&2
        echo "  The bundle predates alignment manifests; rebuild it to get one." >&2
        exit 2
      fi

      field() { jq -r --arg k "$1" '.[$k] // ""' "$manifest"; }

      wantRustc="$(field rustc)"
      wantRustflags="$(field rustflags)"
      wantEncoded="$(field encodedRustflags)"
      wantConfigHash="$(field cargoConfigSha256)"
      wantVendor="$(field vendorDir)"
      wantProfile="$(field profile)"
      wantTarget="$(field target)"

      if ! live="$(python3 ${resolveCargoConfig} 2>&1)"; then
        echo "katsuobushi: could not resolve this environment's cargo configuration" >&2
        printf '%s\n' "$live" | sed 's/^/  /' >&2
        echo "  Verdict UNKNOWN — this is not a mismatch, it is a broken check." >&2
        exit 2
      fi
      livefield() { printf '%s' "$live" | jq -r --arg k "$1" '.[$k] // ""'; }

      haveRustc="$(rustc --version 2>/dev/null || echo "<no rustc on PATH>")"
      haveRustflags="''${RUSTFLAGS:-}"
      haveEncoded="''${CARGO_ENCODED_RUSTFLAGS:-}"
      haveConfigHash="$(livefield workspaceConfigSha256)"
      haveVendor="$(livefield vendorDir)"
      haveTarget="''${CARGO_BUILD_TARGET:-$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')}"

      mismatches=0
      report() { # field, want, have, consequence
        printf '  %-14s bundle %-38s shell %s\n' "$1:" "''${2:-<unset>}" "''${3:-<unset>}"
        printf '  %-14s %s\n' "" "$4"
        mismatches=$((mismatches + 1))
      }

      if [ "$wantRustc" != "$haveRustc" ]; then
        report "rustc" "$wantRustc" "$haveRustc" \
          "cargo keys every unit on the compiler identity; nothing in the bundle is reusable."
      fi
      if [ "$wantRustflags" != "$haveRustflags" ]; then
        report "RUSTFLAGS" "$wantRustflags" "$haveRustflags" \
          "flags are folded into every unit hash; the bundle will be discarded wholesale."
      fi
      if [ "$wantEncoded" != "$haveEncoded" ]; then
        report "ENCODED_FLAGS" "$wantEncoded" "$haveEncoded" \
          "CARGO_ENCODED_RUSTFLAGS takes precedence over RUSTFLAGS and differs here."
      fi
      wantBuildRustflags="$(field cargoBuildRustflags)"
      haveBuildRustflags="''${CARGO_BUILD_RUSTFLAGS:-}"
      if [ "$wantBuildRustflags" != "$haveBuildRustflags" ]; then
        report "BUILD_RUSTFLAGS" "$wantBuildRustflags" "$haveBuildRustflags" \
          "CARGO_BUILD_RUSTFLAGS feeds every unit hash just as RUSTFLAGS does."
      fi

      # Flags can also arrive from a cargo config file. The manifest can only
      # speak for the workspace config Nix built under (its hash), so any
      # config-supplied rustflags coming from anywhere else — a global
      # $CARGO_HOME config, an ancestor directory — are flags the build never saw.
      # Measured: `[build] rustflags` in $CARGO_HOME made cargo rebuild all 91
      # units while the checker reported aligned.
      liveCfgFlags="$(printf '%s' "$live" | jq -c '.configRustflags // empty')"
      liveCfgSource="$(livefield configRustflagsSource)"
      liveWsConfig="$(livefield workspaceConfigPath)"
      if [ -n "$liveCfgFlags" ]; then
        if [ "$liveCfgSource" != "$liveWsConfig" ] || [ -z "$wantConfigHash" ]; then
          report "config rustflags" "<none the build saw>" "$liveCfgFlags" \
            "set by $liveCfgSource, which the Nix build never read; cargo folds them into every unit hash."
        fi
      fi

      if [ "$wantTarget" != "$haveTarget" ]; then
        report "target" "$wantTarget" "$haveTarget" \
          "a bundle is per-target; this one holds a different triple's closure."
      fi
      if [ "$wantConfigHash" != "$haveConfigHash" ]; then
        report ".cargo config" "$wantConfigHash" "$haveConfigHash" \
          "the workspace cargo config differs from the one Nix built under — check \`sourceInclude\` carries \".cargo\"."
      fi

      # The decisive field. b2c8e5 measured that a bundle built against crane's
      # vendor directory is reused ONLY when the live cargo resolves through that
      # same directory; without it, every dependency recompiles and the seeded
      # target dir buys exactly nothing. Never report "aligned" without it.
      # crane records the vendor *root* (`cargoVendorDir`) while the generated
      # config points at the hashed sources directory nested inside it, so a
      # descendant counts as the same vendor set — measured working in b2c8e5's
      # arm 2, which paired exactly these two paths and got full reuse.
      # A textual prefix test is not enough: `<root>/..` walks straight out of the
      # recorded root, and a path that simply does not exist is not a vendor
      # directory at all (a garbage-collected one is the realistic case, and it
      # rebuilds the whole closure while looking fine).
      #
      # Both sides normalize LEXICALLY (`-s -m`): crane's vendor root is a
      # directory of symlinks into other store paths, so dereferencing the live
      # side detaches it from the root that was recorded and rejects the one
      # configuration cargo actually reuses (measured: full reuse in 2.23s while
      # the checker said "not aligned"). The trade-off is that a symlink escaping
      # the vendor root is no longer detected as an escape — a shape crane never
      # emits, and one the dereferencing version did not catch either.
      vendorMatches() {
        local want have
        [ -n "$1" ] && [ -n "$2" ] || return 1
        [ -d "$2" ] || return 1
        want="$(realpath -s -m "$1" 2>/dev/null)" || return 1
        have="$(realpath -s -m "$2" 2>/dev/null)" || return 1
        if [ "$want" = "$have" ]; then
          return 0
        fi
        case "$have" in
          "$want"/*) return 0 ;;
          *) return 1 ;;
        esac
      }

      # Cargo requires a `directory` source to hold vendored *crates*, each with a
      # .cargo-checksum.json. Pointing it at crane's vendor ROOT satisfies every
      # path test and still fails at cargo (measured: exit 101, "no matching
      # package named `serde` found"), so shape is checked, not just identity.
      looksLikeVendorDir() {
        [ -d "$1" ] || return 1
        find -L "$1" -mindepth 2 -maxdepth 2 -name .cargo-checksum.json -print -quit 2>/dev/null | grep -q .
      }

      if [ -z "$wantVendor" ]; then
        report "vendored src" "<not recorded>" "''${haveVendor:-<none>}" \
          "this bundle predates vendorDir recording; rebuild it before trusting a verdict."
      elif vendorMatches "$wantVendor" "$haveVendor" && ! looksLikeVendorDir "$haveVendor"; then
        report "vendored src" "$wantVendor" "$haveVendor (NOT A VENDOR DIR)" \
          "the path matches but holds no vendored crates; cargo cannot resolve against it."
      elif ! vendorMatches "$wantVendor" "$haveVendor"; then
        if [ -z "$haveVendor" ]; then
          report "vendored src" "$wantVendor" "<none — cargo resolves from the registry>" \
            "THE decisive field: without source replacement cargo rebuilds the whole closure (measured)."
        elif [ ! -d "$haveVendor" ]; then
          report "vendored src" "$wantVendor" "$haveVendor (MISSING)" \
            "cargo is pointed at a vendor directory that does not exist — collected, or a stale config."
        elif ! looksLikeVendorDir "$haveVendor"; then
          report "vendored src" "$wantVendor" "$haveVendor (NOT A VENDOR DIR)" \
            "no vendored crates there — cargo will fail to resolve dependencies at all."
        else
          report "vendored src" "$wantVendor" "$haveVendor" \
            "a different vendor directory fingerprints differently; the bundle is not reusable."
        fi
      fi

      # A config cargo would refuse to parse is not "no source replacement" — say
      # what is actually wrong, or the reader chases the wrong thing.
      problemCount="$(printf '%s' "$live" | jq '.problems | length')"
      if [ "$problemCount" -gt 0 ]; then
        echo "katsuobushi: cannot judge alignment — this environment's cargo configuration is broken" >&2
        printf '%s' "$live" | jq -r '.problems[] | "  \(.kind): \(.file)\n    \(.detail)"' >&2
        # NOT a mismatch: cargo would error out rather than rebuild, so calling
        # this "the bundle will be discarded" would be a lie. Same action for the
        # caller (do not seed), different and truthful reason.
        echo "  Verdict UNKNOWN — fix the config before trusting any verdict." >&2
        exit 2
      fi

      # Profile is per-invocation in cargo — there is no ambient value to read —
      # so it is compared only when the caller states which one it will build.
      if [ -n "$wantedProfile" ] && [ "$wantProfile" != "$wantedProfile" ]; then
        report "profile" "$wantProfile" "$wantedProfile" \
          "a bundle holds one profile's closure; another profile shares nothing with it."
      fi

      if [ "$mismatches" -eq 0 ]; then
        if [ -n "$wantedProfile" ]; then
          echo "katsuobushi: bundle is aligned with this environment ($(field pname), profile $wantProfile)"
        else
          echo "katsuobushi: bundle is aligned with this environment ($(field pname))"
        fi
        if [ -z "$wantedProfile" ]; then
          echo "  profile NOT compared — cargo picks it per invocation; pass --profile $wantProfile to check it."
        fi
        echo "  vendored sources: $haveVendor"
        echo "  cargo config seen: $(printf '%s' "$live" | jq -r '.configFiles | if length == 0 then "<none>" else join(", ") end')"
        exit 0
      fi

      echo "katsuobushi: NOT aligned — cargo would rebuild rather than reuse this bundle" >&2
      exit 1
    '';
  };

  # The browser wasm target. The wasm-bindgen / wasm-opt / esbuild toolchain in
  # `buildWasmCrate` and `buildTrunkCrate` is specific to this triple; other
  # wasm targets (e.g. `wasm32-wasip3`) go through the generic `buildCrate` with
  # an explicit `target`.
  browserWasmTarget = "wasm32-unknown-unknown";

  # Build a crate for any Cargo target. `target` is a triple string (e.g.
  # "wasm32-wasip3") or `null`/omitted for the host; `profile` is the Cargo
  # build profile (default `release`, e.g. `dev` for an unoptimized build). Both
  # are consumed here — the rest of the attributes pass through to crane.
  buildCrate =
    {
      target ? null,
      profile ? defaultProfile,
      ...
    }@attributes:
    craneLib.buildPackage (
      attributesFor { inherit target profile; }
      // {
        cargoArtifacts = depsFor { inherit target profile; };
      }
      // removeAttrs attributes [ "target" "profile" ]
    );

  # Build a browser-targeted wasm crate (wasm32-unknown-unknown), surfacing the
  # pinned wasm-bindgen / wasm-opt / esbuild tools. `profile` is the Cargo build
  # profile (default `release`, e.g. `dev` for an unoptimized build).
  buildWasmCrate =
    {
      profile ? defaultProfile,
      ...
    }@attributes:
    craneLib.buildPackage (
      attributesFor { target = browserWasmTarget; inherit profile; }
      // {
        cargoArtifacts = depsFor { target = browserWasmTarget; inherit profile; };

        # These *_BIN envvars are conventional and consumed by build scripts
        # such as `worker-build`; they are also a convenient way to surface
        # the pinned tools to a custom buildPhase.
        WASM_OPT_BIN = "${pkgs.binaryen}/bin/wasm-opt";
        WASM_BINDGEN_BIN = "${wasm-bindgen-cli}/bin/wasm-bindgen";
        ESBUILD_BIN = "${pkgs.esbuild}/bin/esbuild";
      }
      // removeAttrs attributes [ "profile" ]
    );

  # Build a Trunk-bundled browser wasm app. `profile` is the Cargo build profile
  # (default `release`, e.g. `dev` for an unoptimized build). NOTE: crane's Trunk
  # builder only distinguishes release from non-release for Trunk's own
  # `trunk build` step, so `release` and `dev` thread through cleanly, but a
  # custom-named profile reaches the shared deps bundle without being passed to
  # `trunk build` itself.
  buildTrunkCrate =
    {
      profile ? defaultProfile,
      ...
    }@attributes:
    let
      crateRoot = builtins.dirOf attributes.trunkConfig;
    in
    craneLib.buildTrunkPackage (
      attributesFor { target = browserWasmTarget; inherit profile; }
      // {
        cargoArtifacts = depsFor { target = browserWasmTarget; inherit profile; };
        preBuild = ''
          cd ${crateRoot}
        '';
        inherit wasm-bindgen-cli;
      }
      // removeAttrs attributes [ "profile" ]
    );

  # Archive a workspace's tests for later execution. `profile` is the Cargo
  # build profile (default `release`, e.g. `dev` for an unoptimized build).
  buildTestArchive =
    {
      name,
      args ? "",
      target ? null,
      profile ? defaultProfile,
    }:
    craneLib.mkCargoDerivation (
      attributesFor { inherit target profile; }
      // {
        pname = "tests-${name}";
        cargoArtifacts = depsFor { inherit target profile; };

        # `cargo nextest archive` doesn't consult crane's `CARGO_PROFILE`, so we
        # pass the profile explicitly. This keeps the archived test binaries in
        # the same profile as the shared deps bundle above — otherwise nextest's
        # default test profile would diverge from it and recompile the closure.
        buildPhaseCargoCommand = ''
          cargo nextest archive \
            --cargo-profile ${profile} \
            ${args} \
            --archive-file ./tests-${name}.tar.zst
        '';

        installPhaseCommand = ''
          mkdir -p $out
          cp ./*.tar.zst $out/
        '';

        doInstallCargoArtifacts = false;
        nativeBuildInputs = (attributesFor { inherit target profile; }).nativeBuildInputs ++ [
          pkgs.cargo-nextest
        ];
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

  # Dev-shell menu commands for day-to-day Rust development. Merge into
  # makeMenu's `commands` table alongside the project's other library groups.
  # Each command runs against the whole workspace so a new crate is picked up
  # automatically — no edits required here.
  menuCommands = {
    rust = {
      description = "Build, test, lint and format the Rust workspace";
      subcommands = {
        build = {
          description = "Build all crates in the workspace";
          command = "cargo build --workspace";
        };
        test = {
          description = "Run all tests in the workspace";
          command = "cargo test --workspace";
        };
        lint = {
          description = "Lint the workspace — fails on any clippy warning";
          command = "cargo clippy --workspace --all-targets --all-features -- -D warnings";
        };
        fmt = {
          description = "Format or check the workspace source";
          subcommands = {
            format = {
              description = "Format all workspace sources with rustfmt";
              command = "cargo fmt --all";
            };
            check = {
              description = "Check formatting without writing (usable in CI)";
              command = "cargo fmt --all --check";
            };
          };
        };
      };
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
    checkArtifactAlignment
    menuCommands
    wasm-bindgen-cli
    ;
}
