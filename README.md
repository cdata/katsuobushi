<div align="center">
    <img style="border-radius: 1em" src="./hero.webp" width="256">
    <br>
    <br>
    <p>
A collection of libraries and workflows for Nix Flake-based projects.

I tend to set up my projects with the same workflows again and again. This
repository assembles the things I always find useful.

I hope that you will enjoy it. </p>

</div>

## Inventory

| Library                              | Description                                                                        |
| ------------------------------------ | ---------------------------------------------------------------------------------- |
| [`menu`](lib/menu/README.md)         | General a colorful and useful command menu for a project's devshell                |
| [`rust`](lib/rust/README.md)         | Convenience wrapper over [Crane] to reduce boilerplate in Rust project derivations |
| [`markdown`](lib/markdown/README.md) | Formatting and lint for Markdown documentation (via [Prettier])                    |
| [`sandbox`](lib/sandbox/README.md)   | Ephemeral, project-specific, VM-sandboxed workspaces (Linux-only, via [`qemu`])    |

| Template  | Description                                          | Usage                                                |
| --------- | ---------------------------------------------------- | ---------------------------------------------------- |
| `default` | Basic flake with a fancy menu                        | `nix flake init -t github:cdata/katsuobushi`         |
| `sandbox` | General flake with a menu and pre-configured sandbox | `nix flake init -t github:cdata/katsuobushi#sandbox` |
| `rust`    | Flake for Rust projects w/ maximum umami             | `nix flake init -t github:cdata/katsuobushi#rust`    |

## Agent skills

Katsuobushi is also a [Claude Code plugin marketplace][plugins]. Installing the
`katsuobushi` plugin teaches your agent to drive the sandbox for you — say _"use
the sandbox to…"_ and it knows the launch/prompt/fetch workflow.

```text
/plugin marketplace add cdata/katsuobushi
/plugin install katsuobushi@katsuobushi
```

It currently provides the `sandbox` skill (host-side orchestration of agent-mode
sandboxes); see [`lib/sandbox/README.md`](lib/sandbox/README.md).

[Crane]: https://crane.dev
[Prettier]: https://prettier.io
[`qemu`]: https://www.qemu.org
[plugins]: https://docs.claude.com/en/docs/claude-code/plugins
