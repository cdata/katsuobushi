# Katsuobushi Markdown Helpers

Formatting and linting for a project's Markdown documents via [Prettier]. It
exists so one Prettier configuration is defined once and shared by both a
dev-shell formatter command and a flake check, so downstream projects track
upstream tweaks instead of carrying a local copy. Prettier handles GFM tables
natively — the reason this library moved off `rumdl`.

Exposed via `katsuobushi.lib.markdown`.

## Usage

```nix
markdown = katsuobushi.lib.markdown {
  inherit pkgs;
  workspaceRoot = ./.;
  name = "readmes"; # names the command group + check (default: "markdown")
  include = [ "README.md" "lib/*/README.md" ]; # default: [ "**/*.md" ]
  # exclude = [ "vendor/**" ]; # written to a Prettier ignore file
  # settings = { printWidth = 100; }; # merged over the defaults
};
```

```nix
devShells.default = pkgs.mkShell {
  # adds the `<name>` command (with `format` / `lint` subcommands) + `prettier`
  nativeBuildInputs = menu.commands ++ [ markdown.prettier ];
};

checks = markdown.checks; # fails when documents drift from the format
```

## What you get

Each invocation is **namespaced** by `name`, so several invocations compose
without collision (there is no shared/global command).

| Output                                           | Purpose                                                                                                            |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| `menuCommands`                                   | A `<name>` branch with `format` (rewrite in place) and `lint` (read-only check) subcommands, to merge into a menu. |
| `checks`                                         | `checks.<name>`, fails when included files aren't formatted. Merge into the flake's `checks`.                      |
| `prettier` / `prettierConfig` / `prettierIgnore` | The pinned tool, generated config, and generated ignore file, for ad-hoc use.                                      |

## Scoping

- `include` becomes Prettier's path arguments; Prettier expands globs itself and
  honors `.gitignore` / `.prettierignore`. Everything matched is parsed as
  Markdown (`--parser markdown`), so point `include` at Markdown.
- `exclude` is written to a generated ignore file passed via `--ignore-path`.
- The check runs from `workspaceRoot`, so every included file must be
  **tracked** — a flake check cannot reach `.gitignore`'d paths (they are not in
  the flake source). Format such paths with the menu command instead.

## Defaults

`settings` are [Prettier options][options], merged over these defaults with
`recursiveUpdate`:

- `proseWrap = "always"` — reflow prose to `printWidth` (code fences untouched).
- `printWidth = 80`.
- `tabWidth = 2`.

[Prettier]: https://prettier.io
[options]: https://prettier.io/docs/options
