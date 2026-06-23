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
    # Latest-tracking Claude Code (newer than nixpkgs); used for this repo's own
    # dogfood sandbox. Consumers of lib.sandbox can do the same and pass it as
    # `claudeCodePackage` (see templates/sandbox).
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
      rustLib = import ./lib/rust.nix { inherit crane nix-filter rust-overlay; };
      markdownLib = import ./lib/markdown.nix;
      sandboxLib = import ./lib/sandbox.nix { inherit microvm; };

      overlay = final: prev: {
        katsuobushi = import ./lib { pkgs = final; };
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

      # Markdown design-doc helpers: a shared rumdl configuration driving both a
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
    # Per-system outputs: Katsuobushi dogfoods its own libraries. The dev shell
    # is built with `makeMenu`/`makeDevShellHook`, formats its design docs with
    # `lib.markdown`, and ships a `lib.sandbox` configuration for working on
    # Katsuobushi itself inside a VM.
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

        # Format Katsuobushi's own (gitignored, local-only) design docs. We turn
        # off respect-gitignore so the command actually reaches them; there is
        # deliberately no matching flake `check`, since `design/` is untracked
        # and so cannot be carried into the Nix store.
        markdown = markdownLib {
          inherit pkgs;
          workspaceRoot = ./.;
          settings.global.respect-gitignore = false;
        };

        # The Katsuobushi sandbox, configured for this repo. The lean
        # Anthropic+Nix baseline already covers everything this pure-Nix flake
        # needs (github flake inputs + cache.nixos.org), so no extra origins.
        sandbox = sandboxLib {
          inherit pkgs;
          workspaceRoot = ./.;
          projectId = "cdata/katsuobushi";
          # Carry local agent context into the VM (both are gitignored).
          workspaceContext = [
            ".claude"
            "design"
          ];
          secrets.CLAUDE_CODE_OAUTH_TOKEN.fromEnv = "CLAUDE_CODE_OAUTH_TOKEN";
          # The agent harness is just a package. We use the latest Claude Code
          # from numtide/llm-agents.nix (newer than nixpkgs, and pre-built so no
          # allowUnfree needed).
          packages = [
            llm-agents.packages.${system}.claude-code
          ];
        };

        menu = makeMenu {
          title = "Katsuobushi";
          graphic = ''
            .  o   ..    ><(((°>
          '';
          commands =
            markdown.menuCommands
            // (pkgs.lib.optionalAttrs isLinux sandbox.menuCommands)
            // {
              check = {
                description = "Run the flake checks";
                command = "nix flake check";
              };
            };
        };
      in
      {
        devShells.default = pkgs.mkShell {
          name = "katsuobushi";
          nativeBuildInputs = menu.commands ++ [ markdown.rumdl ];
          shellHook = makeDevShellHook menu;
        };
      }
      // pkgs.lib.optionalAttrs isLinux {
        # `nix run .#sandbox [-- --task "…" | --keep-alive | --name N]`
        apps.sandbox = sandbox.apps.sandbox;
        # CI builds the guest image so a broken sandbox config fails fast.
        checks.sandbox = sandbox.checks.sandbox;
      }
    );
}
