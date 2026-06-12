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
        # differs, override the `rustSource` filter upstream in katsuobushi's
        # `lib/rust.nix`.
        rustHelpers = katsuobushi.lib.rust {
          inherit pkgs crane;
          filter = nix-filter.lib;
          workspaceRoot = ./.;
          # Owner-qualified identifier; namespaces the out-of-tree cargo target
          # dir (see `cargoTargetDir` in katsuobushi's lib/rust.nix). Change me.
          projectId = "my-org/my-project";
          # Tools every Rust derivation needs at build time.
          buildInputs = with pkgs; [ pkg-config ];
        };

        inherit (rustHelpers)
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
        #   checks = cargoChecks // markdownHelpers.checks;
        #   packages.default = my-crate;
        checks = markdown.checks;
      }
    );
}
