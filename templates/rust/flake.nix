{
  description = "My project";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    katsuobushi.url = "github:cdata/katsuobushi";

    # Required by katsuobushi.lib.rust: crane drives the Rust builds,
    # nix-filter scopes the source filter, and rust-overlay provides
    # `pkgs.rust-bin` from which the helper resolves `rust-toolchain.toml`.
    crane.url = "github:ipetkov/crane";
    nix-filter.url = "github:numtide/nix-filter";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      katsuobushi,
      crane,
      nix-filter,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            (import rust-overlay)
            katsuobushi.overlays.default
          ];
        };

        # Import the katsuobushi menu helpers
        inherit (pkgs.katsuobushi) makeMenu makeDevShellHook;

        # Rust build helpers, pulled from the katsuobushi flake so upstream
        # fixes propagate here without a local copy to maintain. The helper
        # expects a Cargo workspace with crates under `rust/`; if your layout
        # differs, pass `sourceInclude` (see below) to point the source filter
        # elsewhere.
        rustHelpers = katsuobushi.lib.rust {
          inherit pkgs crane;
          filter = nix-filter.lib;
          workspaceRoot = ./.;
          # Owner-qualified identifier; namespaces the out-of-tree cargo target
          # dir (see `cargoTargetDir` in katsuobushi's lib/rust.nix). Change me.
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

        # Markdown design-doc helpers: a shared rumdl configuration driving
        # the `format:design` menu command and the design check below.
        markdown = katsuobushi.lib.markdown {
          inherit pkgs;
          workspaceRoot = ./.;
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
            markdown.rumdl
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
