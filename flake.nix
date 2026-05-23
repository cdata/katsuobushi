{
  description = "Katsuobushi";

  inputs = { };

  outputs =
    { self }:
    {
      overlays.default = final: prev: {
        katsuobushi = import ./lib { pkgs = final; };
      };

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
