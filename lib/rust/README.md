# Katsuobushi Rust Helpers

A convenience wrapper over [Crane] that reduces boilerplate in Rust project
derivations and maximizes dependency/artifact sharing across crates. It applies
[`rust-overlay`] internally (so you pass plain `pkgs`), reads your toolchain
from `rust-toolchain.toml`, and returns ready-to-use builders and checks.

Exposed via `katsuobushi.lib.rust`; the pinned `crane` / `nix-filter` /
`rust-overlay` are partial-applied and overridable per call.

## Usage

```nix
rust = katsuobushi.lib.rust {
  inherit pkgs;
  workspaceRoot = ./.;
  projectId = "my-org/my-project";        # namespaces the out-of-tree target dir
  nativeBuildInputs = with pkgs; [ pkg-config ];
  # buildInputs = …;                       # libs every derivation links against
  # sourceInclude = [ … ];                 # default keeps crates under rust/
  # cargoGitDependencies = { … };          # SRI hashes for git deps (offline builds)
};

inherit (rust) buildCrate cargoChecks rustToolchain rustEnvironmentHook;

my-crate = buildCrate {
  pname = "my-crate";                       # must match [package].name
  cargoExtraArgs = "--package my-crate";
};
```

```nix
devShells.default = pkgs.mkShell {
  nativeBuildInputs = [ rustToolchain ];
  shellHook = rustEnvironmentHook;          # steers cargo at an out-of-tree target dir
};
checks = cargoChecks;                        # clippy + rustfmt + workspace-dep hygiene
```

## What you get

| Output                               | Purpose                                                                                                                    |
| ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- |
| `buildCrate`                         | Build a workspace crate for the host (or a `target` triple). Shares a deps-only artifact across crates.                    |
| `buildWasmCrate` / `buildTrunkCrate` | Browser-wasm builds with a pinned `wasm-bindgen` / `wasm-opt` / `esbuild` toolchain (version derived from `Cargo.lock`).   |
| `buildTestArchive`                   | `cargo nextest archive` for split build/run testing.                                                                       |
| `cargoChecks`                        | `clippy` (`-D warnings`), `rustfmt`, and a check enforcing that every crate inherits deps from `[workspace.dependencies]`. |
| `rustToolchain`                      | The toolchain derivation from `rust-toolchain.toml`.                                                                       |
| `rustEnvironmentHook`                | dev-shell fragment pointing `CARGO_TARGET_DIR` out-of-tree (keeps `target/` out of the Nix store copy).                    |

## Conventions

- Crates live under `rust/` by default (override with `sourceInclude`).
- A pinned `rust-toolchain.toml` and a vendored `Cargo.lock` at the workspace
  root make builds reproducible and offline.

See [`templates/rust/flake.nix`](../../templates/rust/flake.nix) for a fully
commented starting point.

[Crane]: https://crane.dev
[`rust-overlay`]: https://github.com/oxalica/rust-overlay
