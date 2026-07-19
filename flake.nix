{
  description = "Katsuobushi";

  # Katsuobushi owns its infrastructure dependencies and passes them through to
  # consumers transitively, so a consuming flake declares Katsuobushi (plus its
  # own nixpkgs) and inherits crane / nix-filter / rust-overlay / microvm without
  # having to name them. Each infra input `follows` our nixpkgs so the dependency
  # graph unifies on a single nixpkgs; a consumer overrides any of them with
  # `inputs.katsuobushi.inputs.<name>.follows = "<name>";`. See MIGRATING.md and
  # section 8 of for the rationale.
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
      projectLib = import ./lib/project;
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

      # Project-backlog helpers: wraps the `katsuctl project` domain (the
      # Obsidian-Kanban-native board) as dev-shell menu commands. Called with the
      # consumer's `pkgs` and the built `katsuctl` (`packages.<system>.katsuctl`).
      lib.project = projectLib;

      # Agent sandbox helpers: assembles a microvm.nix guest that boots into a
      # working dev environment in which an agent harness can run with a bounded
      # blast radius. See.
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
        # inputs (github + cache.nixos.org). The extra origins serve in-guest
        # Rust builds:
        #   - static.rust-lang.org — the Rust toolchain dist server; `nix develop`
        #     provisions the toolchain via rust-overlay from here (only a fallback
        #     for a *new* toolchain the host hasn't built, since importHostStoreDb
        #     lets the guest reuse the host's built toolchain offline).
        #   - crates.io / index.crates.io / static.crates.io — the cargo registry
        #     API, the sparse index, and crate tarball downloads. Without these a
        #     guest `cargo build`/`test` can't fetch dependencies and must fall
        #     back to a brittle `--offline` build against crane-vendored deps;
        #     allowing them lets dispatched agents build the crate normally.
        sandbox = sandboxLib {
          inherit pkgs;
          workspaceRoot = ./.;
          projectId = "cdata/katsuobushi";
          allowedOrigins = [
            "static.rust-lang.org"
            "crates.io"
            "index.crates.io"
            "static.crates.io"
          ];
          # Carry local agent context into the VM (both are gitignored).
          workspaceContext = [
            ".claude"
            "design"
          ];
          graphics.enable = true;
          secrets.CLAUDE_CODE_OAUTH_TOKEN.fromEnv = "HARNESS_OAUTH_TOKEN";
          # The agent harness is just a package. We use the latest Claude Code
          # from numtide/llm-agents.nix (newer than nixpkgs, and pre-built so no
          # allowUnfree needed).
          packages = [
            pkgs.mesa-demos
            llm-agents.packages.${system}.claude-code
          ];
        };

        # In-tree Rust: the host↔guest sandbox controller crate for agent mode.
        # Built via lib.rust (crane) so the build is reproducible and sandboxed,
        # with deps vendored from the root Cargo.lock. The guest crate and the
        # sandbox *domain* are Linux-only (tokio-vsock), but `katsuctl` itself now
        # cfg's the sandbox domain out on non-Linux, so it builds everywhere.
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
        # Host-side sandbox controller. Built the same way; cross-platform (the
        # sandbox domain cfg's out on non-Linux), so it goes into `packages` and
        # on the devshell PATH on every system.
        controlCrates.katsuctl = rust.buildCrate {
          pname = "katsuctl";
          cargoExtraArgs = "--package katsuobushi-controller";
        };

        # Dogfood the project backlog on a fresh root `project/kanban/`. Shares
        # the one katsuctl build; cross-platform, like the `project` domain of the
        # crate (the `katsuctl project` subcommands build on every OS).
        project = projectLib {
          inherit pkgs;
          katsuctl = controlCrates.katsuctl;
          workspaceRoot = ./.;
        };

        menu = makeMenu {
          title = "Katsuobushi";
          graphicFile = ./hero.ansi;
          colorizeGraphic = false;
          # Each library configuration contributes its own namespaced group
          # (e.g. a `design` branch with format/lint subcommands, a `markdown`
          # branch, a `sandbox` branch); there is no global aggregate command.
          commands =
            markdown.menuCommands
            // project.menuCommands
            // (pkgs.lib.optionalAttrs isLinux sandbox.menuCommands);
        };
      in
      {
        devShells.default = pkgs.mkShell {
          name = "katsuobushi";
          nativeBuildInputs =
            menu.commands
            ++ [
              markdown.prettier
              # Toolchain for working on the in-tree sandbox controller crate.
              rust.rustToolchain
              # Used by the sandbox lifecycle commands (QMP over the qemu monitor)
              # and by the sandbox controller spike harness.
              pkgs.socat
              # A bare `katsuctl` on the PATH for power users (additive). The
              # `sandbox:*` commands invoke it by absolute store path, so this is
              # not required for them to work. Cross-platform now (the sandbox
              # domain cfg's out on non-Linux), so it is on the PATH everywhere.
              controlCrates.katsuctl
            ];
          shellHook = rust.rustEnvironmentHook + makeDevShellHook menu;
        };
        # `katsuctl` builds on every system (its sandbox domain cfg's out on
        # non-Linux), so expose it — and only it — cross-platform. The Linux-only
        # guest crate is added to `packages` under the isLinux guard below.
        packages.katsuctl = controlCrates.katsuctl;
      }
      // pkgs.lib.optionalAttrs isLinux {
        # `nix run .#sandbox [-- --agent [--prompt "…"] | --name N]`
        apps.sandbox = sandbox.apps.sandbox;
        # The full controller crate set (adds the Linux-only guest server); this
        # supersedes the cross-platform `packages.katsuctl` above on Linux, and
        # `controlCrates.katsuctl` is the same derivation, so nothing is lost.
        packages = controlCrates;
        # CI builds the guest image, and clippy/rustfmt/workspace-dep hygiene
        # on the controller crate, so a broken config or crate fails fast.
        checks = {
          sandbox = sandbox.checks.sandbox;
        }
        // rust.cargoChecks
        // markdown.checks
        // project.checks;
      }
    );
}
