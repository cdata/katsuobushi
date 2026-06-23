{
  description = "My project with a Katsuobushi agent sandbox";

  # Katsuobushi carries the sandbox infra (microvm.nix) as a transitive input,
  # so this flake declares only nixpkgs, flake-utils, and katsuobushi — plus any
  # *project-data* sources you want pinned (reference repos, dotfiles). Those
  # stay consumer-declared; they are not absorbed by the toolkit (design 4.16).
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    katsuobushi.url = "github:cdata/katsuobushi";
    katsuobushi.inputs.nixpkgs.follows = "nixpkgs";

    # Optional: track the latest Claude Code (usually newer than nixpkgs) and
    # pass it as `claudeCodePackage` below. Pre-built upstream, so it does not
    # need `allowUnfree`.
    llm-agents.url = "github:numtide/llm-agents.nix";

    # --- Project-data sources for the sandbox (optional) ---
    # A reference repo to clone read-only-provenance into the VM. `flake = false`
    # means "just fetch the tree"; it is pinned in this flake's flake.lock and
    # updated with `nix flake update`.
    rust-overlay-src = {
      url = "github:oxalica/rust-overlay";
      flake = false;
    };
    # A personal config repo holding, e.g., a universal AGENTS.md to map into the
    # guest agent's ~/.claude/CLAUDE.md.
    nixos-config = {
      url = "github:cdata/nixos-config";
      flake = false;
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      katsuobushi,
      llm-agents,
      rust-overlay-src,
      nixos-config,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        # claude-code is unfree; allow it so the guest can install the harness.
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
          overlays = [ katsuobushi.overlays.default ];
        };

        inherit (pkgs.katsuobushi) makeMenu makeDevShellHook;

        sandbox = katsuobushi.lib.sandbox {
          inherit pkgs;
          workspaceRoot = ./.;
          # Owner-qualified; names the in-guest project path and the per-instance
          # host state dirs (~/.local/state/katsuobushi/<projectId>/<instance>).
          projectId = "my-org/my-project";

          # --- Network egress (appended to the lean Anthropic+Nix baseline) ---
          # Hostnames only; no implicit wildcards. Port 443 (HTTPS) is assumed.
          allowedOrigins = [
            "crates.io"
            "static.crates.io"
          ];
          # The only "removal" mechanism is a wholesale override of the baseline:
          #   baseAllowedOrigins = [ "api.anthropic.com" "cache.nixos.org" ];

          # --- Reference repos: build-time pinned, writable copies in the VM ---
          # `source` is any store path (a `flake = false` input, or a fetcher
          # like `pkgs.fetchFromGitHub { ... }`). `dest` mirrors the host
          # ~/Git/<host>/<owner>/<repo> convention so the agent finds them where
          # it expects. One-way; no sync-back, and the repo's host does NOT need
          # to be in allowedOrigins.
          extraRepos = [
            {
              source = rust-overlay-src;
              dest = "Git/github.com/oxalica/rust-overlay";
            }
          ];

          # --- Untracked context: project-scoped, one-way host -> guest ---
          # Extra paths (relative to the project root) carried into the workspace
          # on top of the mirror clone. Absolute paths and ".." are rejected at
          # eval time; symlinks escaping the tree are dropped at copy time.
          #
          # Per-project Claude Code config travels in here too: a project
          # `.claude/settings.json` (committed, or carried as untracked context)
          # is read in the guest. To default the in-VM agent to a model, put
          #   { "model": "claude-opus-4-8" }
          # in your project's `.claude/settings.json` — the guest TUI then shows
          # "Using Opus 4.8 (from .claude/settings.json)". Use the explicit model
          # id, not the "opus" alias (alias resolution needs network the sandbox
          # denies).
          workspaceContext = [
            ".claude"
            "notes"
          ];

          # --- git-source -> guest-home file mappings ---
          homeFiles = {
            # Map a universal AGENTS.md into the agent's CLAUDE.md, read-only
            # even against the agent (RO bind mount).
            ".claude/CLAUDE.md" = {
              source = nixos-config;
              path = "AGENTS.md";
              mode = "immutable"; # also: "seed" (editable copy) | "link" (symlink)
            };
          };

          # --- Runtime secrets: read from the host at launch; never in Nix ---
          # Generate the token on the host with `claude setup-token` and export
          # it before `nix run .#sandbox`. The runner fails fast if it is missing.
          secrets = {
            CLAUDE_CODE_OAUTH_TOKEN = {
              fromEnv = "CLAUDE_CODE_OAUTH_TOKEN";
            };
            # Or read from a file:
            #   SOME_TOKEN = { fromFile = "/run/secrets/some-token"; };
          };

          # The Claude Code harness to install in the guest. Defaults to
          # nixpkgs' `claude-code`; track the newer llm-agents build instead:
          claudeCodePackage = llm-agents.packages.${system}.claude-code;

          # --- Resources ---
          vcpu = 4;
          mem = 8192; # MiB (avoid exactly 2048 — qemu hangs)
          storeOverlaySize = "8G"; # tmpfs writable /nix/store overlay

          # --- Escape hatch: extra NixOS modules merged into the guest ---
          # guestModules = [ ./guest-extra.nix ];
        };

        menu = makeMenu {
          title = "My Project";
          commands = {
            greet = {
              description = "Print a friendly greeting";
              command = ''echo "Hello from My Project!"'';
            };
          }
          # Adds `sandbox` plus `sandbox-list` / `sandbox-status <inst>` /
          # `sandbox-fetch <inst>` / `sandbox-stop <inst>` to the dev shell.
          // sandbox.menuCommands;
        };
      in
      {
        # `nix run .#sandbox [-- --task "..." | --task-file P | --keep-alive | --name N]`
        apps.sandbox = sandbox.apps.sandbox;

        # CI catches a broken sandbox config by building the guest image.
        checks.sandbox = sandbox.checks.sandbox;

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = menu.commands;
          shellHook = makeDevShellHook menu;
        };
      }
    );
}
