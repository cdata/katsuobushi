{
  description = "My project";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    katsuobushi.url = "github:cdata/katsuobushi";

    # Required by ./nix/rust.nix: crane drives the Rust builds, nix-filter
    # scopes the source filter, and rust-overlay provides `pkgs.rust-bin`
    # from which the helper resolves `rust-toolchain.toml`.
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

        # The helper expects a Cargo workspace with crates under `rust/`;
        # adjust the `rustSource` filter inside `nix/rust.nix` if your layout
        # differs.
        rustHelpers = import ./nix/rust.nix {
          inherit pkgs crane;
          filter = nix-filter.lib;
          workspaceRoot = ./.;
          # Tools every Rust derivation needs at build time.
          buildInputs = with pkgs; [ pkg-config ];
        };

        inherit (rustHelpers)
          rustToolchain
          ;

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
          };
        };
      in
      {
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = menu.commands ++ [ rustToolchain ];
          shellHook = makeDevShellHook menu;
        };

        # Wire built crates and cargo checks once your workspace exists:
        #   packages.default = my-crate;
        #   checks = cargoChecks;
      }
    );
}
