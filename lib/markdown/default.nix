# This module contains helpers for formatting and linting a project's Markdown
# with [Prettier](https://prettier.io). It exists so that one Prettier
# configuration is shared by the dev-shell formatter command and the flake
# check, and so downstream projects track upstream tweaks to that configuration
# instead of carrying a local copy. Prettier handles GFM tables natively (the
# reason this module moved off rumdl).
#
# `include` / `exclude` are workspace-relative glob lists that scope which files
# are formatted and checked. `include` becomes Prettier's file arguments;
# `exclude` becomes a generated `--ignore-path` file.
#
# Usage (in a consuming flake):
#
#   markdown = katsuobushi.lib.markdown {
#     inherit pkgs;
#     workspaceRoot = ./.;
#     name = "readmes";                          # labels the commands + check
#     include = [ "README.md" "lib/*/README.md" ];
#     # exclude = [ "vendor/**" ];
#     # settings = { printWidth = 100; };        # merged over the defaults
#   };
#
#   # Merge `menuCommands` into makeMenu's commands table, add `prettier` to the
#   # dev shell, and merge `checks` into the flake's checks output.
{
  pkgs,
  # Path to the workspace root (e.g. `./.` in the consuming flake). The format
  # command and the check operate relative to this root.
  workspaceRoot,
  # Workspace-relative files/dirs/globs to format and check. These become
  # Prettier's path arguments (Prettier expands globs itself and honors
  # .gitignore / .prettierignore). Should target Markdown — the default is every
  # tracked `.md` file; everything is parsed as Markdown (`--parser markdown`).
  include ? [ "**/*.md" ],
  # Workspace-relative globs to skip (written to a Prettier ignore file).
  exclude ? [ ],
  # Names the menu branch (`<name>` with `format` / `lint` subcommands) and the
  # check.
  name ? "markdown",
  # Prettier options, merged over the defaults below via recursiveUpdate and
  # written to the config Prettier reads, e.g. `{ printWidth = 100; }`.
  settings ? { },
}:

let
  inherit (pkgs) lib;

  # Defaults mirror the prior rumdl behavior: reflow prose to a fixed width
  # (Prettier leaves fenced code blocks alone), 2-space indent. Prettier formats
  # GFM tables by default — no opt-in needed (unlike rumdl's MD060).
  defaultSettings = {
    proseWrap = "always";
    printWidth = 80;
    tabWidth = 2;
  };

  prettierConfig = (pkgs.formats.json { }).generate "prettier.json" (
    lib.recursiveUpdate defaultSettings settings
  );

  # Prettier reads ignore globs from a file, not the config. Generate one from
  # `exclude` (empty when nothing is excluded — an empty file ignores nothing).
  prettierIgnore = pkgs.writeText "prettier.ignore" (lib.concatStringsSep "\n" exclude + "\n");

  prettier = pkgs.prettier;

  # Shared invocation: parser pinned to markdown so bare globs/dirs are treated
  # as Markdown, config + ignore file from the store, then the include targets.
  # `$mode` is `--check` or `--write`.
  runPrettier = mode: ''
    cd "$(git rev-parse --show-toplevel)"
    ${prettier}/bin/prettier ${mode} \
      --config ${prettierConfig} \
      --ignore-path ${prettierIgnore} \
      --parser markdown \
      ${lib.escapeShellArgs include}
  '';
in
{
  # The prettier package + the generated config/ignore, for ad-hoc use or
  # inclusion in a dev shell's nativeBuildInputs.
  inherit prettier prettierConfig prettierIgnore;

  # Menu commands, ready to merge into makeMenu's `commands` table. Each
  # configuration contributes its OWN branch keyed by `<name>`, with `format`
  # (rewrite in place) and `lint` (read-only check) leaves beneath it — so
  # multiple invocations never collide and there is no shared/global command.
  # Both leaves run from the repo root so the include/exclude globs resolve
  # workspace-relative. Invoked as e.g. `${name} format` / `${name} lint`.
  menuCommands = {
    ${name} = {
      description = "Format or lint the project's ${name} documents";
      subcommands = {
        format = {
          description = "Format the project's ${name} documents";
          command = runPrettier "--write";
        };
        lint = {
          description = "Lint the project's ${name} documents";
          command = runPrettier "--check";
        };
      };
    };
  };

  # Flake check that fails when the documents drift from the enforced format.
  # Merge into the flake's `checks` output (e.g.
  # `checks = cargoChecks // markdown.checks;`). Runs from the workspace root
  # (filtered by include/exclude), so every included file must be tracked — a
  # check cannot reach .gitignore'd paths, which aren't in the flake source.
  checks = {
    "${name}" = pkgs.runCommand "lint-${name}" { } ''
      cd ${workspaceRoot}
      ${prettier}/bin/prettier --check \
        --config ${prettierConfig} \
        --ignore-path ${prettierIgnore} \
        --parser markdown \
        ${lib.escapeShellArgs include}
      touch $out
    '';
  };
}
