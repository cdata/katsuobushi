{
  description = "My project";

  # Katsuobushi carries the Rust build infra (crane, nix-filter, rust-overlay)
  # as transitive inputs, so this flake declares only nixpkgs, flake-utils, and
  # katsuobushi. `nixpkgs.follows` unifies the dependency graph on your nixpkgs.
  # To override an inherited infra dep, add e.g.
  #   katsuobushi.inputs.crane.follows = "crane";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    katsuobushi.url = "github:cdata/katsuobushi";
    katsuobushi.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      katsuobushi,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        # Plain nixpkgs with only the katsuobushi overlay (for the menu helpers).
        # The Rust helper applies rust-overlay internally, so you no longer add
        # `(import rust-overlay)` here.
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ katsuobushi.overlays.default ];
        };

        # Import the katsuobushi menu helpers
        inherit (pkgs.katsuobushi) makeMenu makeDevShellHook;

        # Rust build helpers, pulled from the katsuobushi flake so upstream
        # fixes propagate here without a local copy to maintain. crane,
        # nix-filter, and rust-overlay are inherited from katsuobushi; override
        # one per-call only if you need a different pin (e.g. `crane = ...;`).
        # The helper expects a Cargo workspace with crates under `rust/`; if your
        # layout differs, pass `sourceInclude` (see below) to point the source
        # filter elsewhere.
        rustHelpers = katsuobushi.lib.rust {
          inherit pkgs;
          workspaceRoot = ./.;
          # Owner-qualified identifier; namespaces the out-of-tree cargo target
          # dir (see `cargoTargetDir` in katsuobushi's lib/rust/default.nix). Change me.
          projectId = "my-org/my-project";
          # Tools every Rust derivation needs at build time.
          nativeBuildInputs = with pkgs; [ pkg-config ];
          # Libraries every derivation links against (e.g. for a Tauri app):
          #   buildInputs = with pkgs; [ webkitgtk_4_1 ];
          # Workspace-relative paths kept by the source filter. Defaults to a
          # layout with crates under `rust/`; override when yours live elsewhere:
          #   sourceInclude = [ ".cargo" "Cargo.lock" "Cargo.toml" "rust-toolchain.toml" "crates" ];
          # wasm-bindgen-cli is built to match the `wasm-bindgen` version in your
          # Cargo.lock automatically. If that version isn't one this lib ships
          # hashes for, the build will tell you to add an entry here:
          #   wasmBindgenHashes."0.2.99" = { hash = pkgs.lib.fakeHash; cargoHash = pkgs.lib.fakeHash; };
          # Fixed-output hashes for git dependencies pinned in Cargo.lock, so
          # builds are reproducible and offline. Leave empty and crane falls
          # back to an impure `fetchGit`. The key is the exact `source` string
          # from Cargo.lock; bootstrap the value with `pkgs.lib.fakeHash` and
          # let the failing build report the real hash. Example:
          #
          #   cargoGitDependencies = {
          #     "git+https://github.com/owner/repo.git?rev=<rev>#<rev>" =
          #       "sha256-...";
          #   };
          cargoGitDependencies = { };
        };

        inherit (rustHelpers)
          buildCrate
          cargoChecks
          rustEnvironmentHook
          rustToolchain
          ;

        # Markdown helpers: a shared Prettier configuration driving the
        # `format:markdown` / `lint:markdown` menu commands and the check below.
        # Formats every tracked `.md` file by default; scope with `include`.
        markdown = katsuobushi.lib.markdown {
          inherit pkgs;
          workspaceRoot = ./.;
          # include = [ "README.md" "design" ];
        };

        # Example crate — uncomment once you have a Cargo workspace under
        # `rust/`. `pname` must match the crate's `[package].name`.
        #
        # my-crate = buildCrate {
        #   pname = "my-crate";
        #   cargoExtraArgs = "--package my-crate";
        # };

        menu = makeMenu {
          title = "My Project";
          graphic = ''
                     ___________
                      change me
                    ,-----------
              /\_/\\
             ( o.o )
              > ^ <
             /|   |\\
            (_|   |_)
          '';
          commands = {
            greet = {
              description = "Print a friendly greeting";
              command = ''echo "Hello from My Project!"'';
            };
            build = {
              description = "Build the project";
              command = "echo TODO: add your build command here";
            };
            test = {
              description = "Run the test suite";
              command = "echo TODO: add your test command here";
            };
          }
          // markdown.menuCommands;
        };
      in
      {
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = menu.commands ++ [
            markdown.prettier
            rustToolchain
          ];
          shellHook = rustEnvironmentHook + makeDevShellHook menu;
        };

        # Merge cargo checks once your workspace exists:
        #   checks = cargoChecks // markdown.checks;
        #   packages.default = my-crate;
        checks = markdown.checks;
      }
    );
}
