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

  # Prettier applies `.gitignore` semantics to `--ignore-path`, which anchors a
  # pattern containing a `/` to **the directory holding the ignore file** — not
  # the working directory. A store-path ignore file therefore resolves
  # `project/kanban/BOARD.md` against `/nix/store/…`, where it can never match,
  # and `exclude` silently does nothing. So both call sites below stage the
  # generated file *at the workspace root* and point `--ignore-path` at that
  # copy, which is what makes `exclude` entries workspace-relative as documented.
  ignoreName = ".prettierignore.${name}";

  # Shared invocation: parser pinned to markdown so bare globs/dirs are treated
  # as Markdown, config from the store, root-staged ignore file, then the
  # include targets. `$mode` is `--check` or `--write`.
  #
  # `--write` has to run in the real tree, so the staged ignore file is written
  # into the repo root and removed on exit (including on Ctrl-C).
  #
  # The staged name carries the **pid**, not just `name`. Namespacing by `name`
  # alone is not enough: `<name> format` and `<name> lint` share a `name`, so two
  # overlapping runs would share one path and the first to finish would delete
  # the file out from under the second. That failure is silent and dangerous —
  # Prettier does **not** error on a missing `--ignore-path`, it exits 0 having
  # ignored nothing, so the losing `format` run would happily rewrite every
  # excluded file (a machine-managed `BOARD.md` among them). The explicit
  # existence check below is belt-and-braces against the same class of bug.
  runPrettier = mode: ''
    cd "$(git rev-parse --show-toplevel)"
    ignore="${ignoreName}.$$"
    cp -f ${prettierIgnore} "$ignore"
    trap 'rm -f "$ignore"' EXIT INT TERM
    if [ ! -f "$ignore" ]; then
      echo "${name}: could not stage the ignore file at $PWD/$ignore" >&2
      exit 1
    fi
    ${prettier}/bin/prettier ${mode} \
      --config ${prettierConfig} \
      --ignore-path "$ignore" \
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
    # The check is read-only, so instead of writing into the (immutable) source
    # store path it copies the tree into the build dir and stages the ignore
    # file there — the copy is what lets `exclude` patterns resolve
    # workspace-relative (see the `ignoreName` note above).
    "${name}" = pkgs.runCommand "lint-${name}" { } ''
      cp -r --no-preserve=mode,ownership ${workspaceRoot} ./workspace
      cp -f ${prettierIgnore} ./workspace/${ignoreName}
      cd ./workspace
      ${prettier}/bin/prettier --check \
        --config ${prettierConfig} \
        --ignore-path ${lib.escapeShellArg ignoreName} \
        --parser markdown \
        ${lib.escapeShellArgs include}
      touch $out
    '';
  };
}
