<div align="center">
    <img style="border-radius: 1em" src="./hero.webp" width="256">
    <br>
    <br>
    <p>
A collection of libraries and workflows for Nix Flake-based projects.

I tend to set up my projects with the same workflows again
and again. This repository assembles the things I always find useful.

I hope that you will enjoy it.
    </p>
</div>

## Inventory

| Library | Description |
| ------- | ------- |
| `menu` | General a colorful and useful command menu for a project's devshell |
| `rust` | Convenience wrapper over [Crane] to reduce boilerplate in Rust project derivations |
| `markdown` | Formatting and lint for Markdown documentation (via [`rumdl`]) |
| `sandbox` | Ephemeral, project-specific, VM-sandboxed workspaces (Linux-only, via [`qemu`]) |

| Template  | Description                                          | Usage                                                |
| --------- | ---------------------------------------------------- | ---------------------------------------------------- |
| `default` | Basic flake with a fancy menu                        | `nix flake init -t github:cdata/katsuobushi`         |
| `sandbox` | General flake with a menu and pre-configured sandbox | `nix flake init -t github:cdata/katsuobushi#sandbox` |
| `rust`    | Flake for Rust projects w/ maximum umami             | `nix flake init -t github:cdata/katsuobushi#rust`    |


[Crane]: https://crane.dev
[`rumdl`]: https://rumdl.dev
[`qemu`]: https://www.qemu.org