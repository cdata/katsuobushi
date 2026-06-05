{
  description = "Katsuobushi";

  inputs = { };

  outputs =
    { self }:
    {
      overlays.default = final: prev: {
        katsuobushi = import ./lib { pkgs = final; };
      };

      # Rust build helpers, shared so downstream projects track upstream
      # updates instead of carrying a local copy. It's a function — consuming
      # flakes call it with their own `pkgs`, `crane`, etc. (see the rust
      # template's flake.nix for the full call).
      lib.rust = import ./lib/rust.nix;

      templates = {
        default = {
          path = ./templates/default;
          description = "A barebones flake with flake-utils and a katsuobushi dev shell menu";
        };

        rust = {
          path = ./templates/rust;
          description = "A katsuobushi template for Rust projects";
        };
      };
    };
}
