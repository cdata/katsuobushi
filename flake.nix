{
  description = "Katsuobushi";

  # Katsuobushi owns its infrastructure dependencies and passes them through to
  # consumers transitively, so a consuming flake declares Katsuobushi (plus its
  # own nixpkgs) and inherits crane / nix-filter / rust-overlay / microvm without
  # having to name them. Each infra input `follows` our nixpkgs so the dependency
  # graph unifies on a single nixpkgs; a consumer overrides any of them with
  # `inputs.katsuobushi.inputs.<name>.follows = "<name>";`. See MIGRATING.md and
  # section 8 of design/sandbox.md for the rationale.
  #
  # flake-utils is used only for this repo's own per-system outputs (the
  # dogfooding dev shell below); it is not part of the consumer-facing surface.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    nix-filter.url = "github:numtide/nix-filter";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    microvm = {
      url = "github:microvm-nix/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Used for the `sandbox` template
    # SEE: `templates/sandbox/flake.nix`
    llm-agents.url = "github:numtide/llm-agents.nix";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      crane,
      nix-filter,
      rust-overlay,
      microvm,
      llm-agents,
    }:
    let
      # The library functions, partial-applied with the pinned infra deps. Bound
      # here so both the consumer-facing `lib.*` outputs and this repo's own
      # dogfooding dev shell (below) share one definition.
      rustLib = import ./lib/rust { inherit crane nix-filter rust-overlay; };
      markdownLib = import ./lib/markdown;
      # sandbox carries the rust helper + this repo's workspace source so it can
      # build the in-tree sandbox controller crate (agent mode) internally — the
      # consumer never sees them. `./.` is katsuobushi's own root, pinned for
      # consumers via the flake input, so the crates build from a fixed source.
      sandboxLib = import ./lib/sandbox {
        inherit microvm;
        rust = rustLib;
        controlSrc = ./.;
      };

      # The menu helpers are the overlay's `pkgs.katsuobushi`; point straight at
      # the library (no `lib/default.nix` barrel — every library is `lib/<name>`).
      overlay = final: prev: {
        katsuobushi = import ./lib/menu { pkgs = final; };
      };

      # The guest is a Linux microvm, so the sandbox app/check are Linux-only.
      linuxSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      overlays.default = overlay;

      # Rust build helpers, shared so downstream projects track upstream
      # updates instead of carrying a local copy. A function — consuming flakes
      # call it with their own `pkgs` and config (see the rust template). The
      # infra deps are partial-applied as defaults and remain overridable
      # per-call.
      lib.rust = rustLib;

      # Markdown helpers: a shared Prettier configuration driving both a
      # dev-shell formatter command and a flake check. Called with the
      # consumer's `pkgs`.
      lib.markdown = markdownLib;

      # Agent sandbox helpers: assembles a microvm.nix guest that boots into a
      # working dev environment in which an agent harness can run with a bounded
      # blast radius. See design/sandbox.md.
      lib.sandbox = sandboxLib;

      templates = {
        default = {
          path = ./templates/default;
          description = "A barebones flake with flake-utils and a katsuobushi dev shell menu";
        };

        rust = {
          path = ./templates/rust;
          description = "A katsuobushi template for Rust projects";
        };

        sandbox = {
          path = ./templates/sandbox;
          description = "A katsuobushi template for an agent sandbox VM";
        };
      };
    }
    # Katsuobushi dogfoods its own libraries. The dev shell is built with
    # `makeMenu`/`makeDevShellHook`, formats its design docs with
    # `lib.markdown`, and ships a `lib.sandbox` configuration for working on
    # Katsuobushi itself inside a VM. `sandbox`-adjacent tools are built
    # using `lib.rust`.
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        isLinux = builtins.elem system linuxSystems;

        # allowUnfree so the sandbox guest can install the Claude Code harness.
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
          overlays = [ overlay ];
        };

        inherit (pkgs.katsuobushi) makeMenu makeDevShellHook;

        # Format + lint the tracked, shippable Markdown: the root README and each
        # library's README. Unlike the design docs above, these are tracked — so
        # this invocation also carries a flake `check`.
        markdown = markdownLib {
          inherit pkgs;
          workspaceRoot = ./.;
          include = [
            "README.md"
            "MIGRATING.md"
            "lib/*/README.md"
            "plugins/**/*.md"
          ];
        };

        # The Katsuobushi sandbox, configured for this repo. The lean
        # Anthropic+Nix baseline covers building the guest image and most flake
        # inputs (github + cache.nixos.org). The one extra origin is the Rust
        # toolchain dist server: `nix develop` provisions the toolchain via
        # rust-overlay, which fetches it from static.rust-lang.org. With
        # importHostStoreDb on (default) the guest reuses the host's already-built
        # toolchain offline, so this is only the fallback for picking up a *new*
        # toolchain the host hasn't built yet (e.g. after bumping
        # rust-toolchain.toml).
        sandbox = sandboxLib {
          inherit pkgs;
          workspaceRoot = ./.;
          projectId = "cdata/katsuobushi";
          allowedOrigins = [ "static.rust-lang.org" ];
          # Carry local agent context into the VM (both are gitignored).
          workspaceContext = [
            ".claude"
            "design"
          ];
          secrets.CLAUDE_CODE_OAUTH_TOKEN.fromEnv = "HARNESS_OAUTH_TOKEN";
          # The agent harness is just a package. We use the latest Claude Code
          # from numtide/llm-agents.nix (newer than nixpkgs, and pre-built so no
          # allowUnfree needed).
          packages = [
            llm-agents.packages.${system}.claude-code
          ];
        };

        # In-tree Rust: the host↔guest sandbox controller crate for agent mode
        # (see design/sandbox-agent-mode.md §6). Built via lib.rust (crane) so
        # the build is reproducible and sandboxed, with deps vendored from the
        # root Cargo.lock. tokio-vsock pins these to Linux, matching the
        # sandbox's own platform gate, so the packages/checks are isLinux-only.
        rust = rustLib {
          inherit pkgs;
          workspaceRoot = ./.;
          projectId = "cdata/katsuobushi";
        };
        # The guest controller server (katsuobushi-sandbox-guest). The host
        # client was retired into `katsuctl sandbox prompt`; the guest `report`
        # tool is a shell app built inside lib.sandbox, not a Rust crate.
        controlCrates.katsuobushi-sandbox-guest = rust.buildCrate {
          pname = "katsuobushi-sandbox-guest";
          cargoExtraArgs = "--package katsuobushi-sandbox-guest";
        };
        # Host-side sandbox controller (design/katsuctl.md). Built the same way;
        # stays in `packages` (= controlCrates) and goes on the devshell PATH.
        controlCrates.katsuctl = rust.buildCrate {
          pname = "katsuctl";
          cargoExtraArgs = "--package katsuobushi-controller";
        };

        menu = makeMenu {
          title = "Katsuobushi";
          graphicFile = ./hero.ansi;
          colorizeGraphic = false;
          # Each library configuration contributes its own namespaced commands
          # (e.g. format:design / lint:design, format:markdown / lint:markdown);
          # there is no global aggregate command.
          commands = markdown.menuCommands // (pkgs.lib.optionalAttrs isLinux sandbox.menuCommands);
        };
      in
      {
        devShells.default = pkgs.mkShell {
          name = "katsuobushi";
          nativeBuildInputs = menu.commands ++ [
            markdown.prettier
            # Toolchain for working on the in-tree sandbox controller crate.
            rust.rustToolchain
            # Used by the sandbox lifecycle commands (QMP over the qemu monitor)
            # and by the sandbox controller spike harness.
            pkgs.socat
          ]
          # `katsuctl` on the PATH for power users (additive). Linux-only, like
          # the sandbox controller crate it lives beside (tokio-vsock gate).
          ++ pkgs.lib.optionals isLinux [ controlCrates.katsuctl ];
          shellHook = rust.rustEnvironmentHook + makeDevShellHook menu;
        };
      }
      // pkgs.lib.optionalAttrs isLinux {
        # `nix run .#sandbox [-- --agent [--prompt "…"] | --name N]`
        apps.sandbox = sandbox.apps.sandbox;
        # The sandbox controller crate, built reproducibly via lib.rust/crane.
        packages = controlCrates;
        # CI builds the guest image, and clippy/rustfmt/workspace-dep hygiene
        # on the controller crate, so a broken config or crate fails fast.
        checks = {
          sandbox = sandbox.checks.sandbox;
        }
        // rust.cargoChecks
        // markdown.checks;
      }
    );
}
