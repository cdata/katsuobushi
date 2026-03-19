{
  description = "My project";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    katsuobushi.url = "github:cdata/katsuobushi";
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
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ katsuobushi.overlays.default ];
        };

        # Import the katsuobushi menu helpers
        inherit (pkgs.katsuobushi) makeMenu makeDevShellHook;

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
          nativeBuildInputs = menu.commands;
          shellHook = makeDevShellHook menu;
        };
      }
    );
}
