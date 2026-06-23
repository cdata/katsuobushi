{
  description = "Katsuobushi";

  # Katsuobushi owns its infrastructure dependencies and passes them through to
  # consumers transitively, so a consuming flake declares Katsuobushi (plus its
  # own nixpkgs) and inherits crane / nix-filter / rust-overlay / microvm without
  # having to name them. Each infra input `follows` our nixpkgs so the dependency
  # graph unifies on a single nixpkgs; a consumer overrides any of them with
  # `inputs.katsuobushi.inputs.<name>.follows = "<name>";`. See MIGRATING.md and
  # section 8 of design/sandbox.md for the rationale.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
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
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      nix-filter,
      rust-overlay,
      microvm,
    }:
    {
      overlays.default = final: prev: {
        katsuobushi = import ./lib { pkgs = final; };
      };

      # Rust build helpers, shared so downstream projects track upstream
      # updates instead of carrying a local copy. A function — consuming flakes
      # call it with their own `pkgs` and config (see the rust template). The
      # infra deps are partial-applied as defaults and remain overridable
      # per-call.
      lib.rust = import ./lib/rust.nix { inherit crane nix-filter rust-overlay; };

      # Markdown design-doc helpers: a shared rumdl configuration driving both a
      # dev-shell formatter command and a flake check. Called with the
      # consumer's `pkgs`.
      lib.markdown = import ./lib/markdown.nix;

      # Agent sandbox helpers: assembles a microvm.nix guest that boots into a
      # working dev environment in which an agent harness can run with a bounded
      # blast radius. See design/sandbox.md.
      lib.sandbox = import ./lib/sandbox.nix { inherit microvm; };

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
    };
}
