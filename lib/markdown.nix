# This module contains helpers for formatting and linting a project's
# Markdown design documents with [rumdl](https://github.com/rvben/rumdl).
# It exists so that the rumdl configuration — notably MD060, which aligns
# GFM table columns — is defined once and shared by the dev-shell
# formatter command and the flake check, and so downstream projects track
# upstream tweaks to that configuration instead of carrying a local copy.
#
# Usage (in a consuming flake):
#
#   markdownHelpers = katsuobushi.lib.markdown {
#     inherit pkgs;
#     workspaceRoot = ./.;
#     # docsDir = "design";                  # default
#     # settings = { MD013.line-length = 100; };
#   };
#
#   # Merge `menuCommands` into makeMenu's commands table, add `rumdl` to
#   # the dev shell, and merge `checks` into the flake's checks output.
{
  pkgs,
  # Path to the workspace root (e.g. `./.` in the consuming flake). The
  # check lints `<workspaceRoot>/<docsDir>`.
  workspaceRoot,
  # Directory of Markdown documents, relative to the workspace root. Also
  # names the menu command (`format:<docsDir>`) and the check.
  docsDir ? "design",
  # rumdl settings merged over the defaults below via recursiveUpdate,
  # e.g. `{ MD013.line-length = 100; }`.
  settings ? { },
}:

let
  # Defaults: reflow prose to the line limit (leaving code blocks alone)
  # and enable MD060, which formats GFM tables. MD060 is off by default
  # upstream, and it is most of the reason this module exists.
  defaultSettings = {
    global = {
      "respect-gitignore" = true;
      cache = false;
    };
    MD013 = {
      reflow = true;
      code-blocks = false;
    };
    MD060 = {
      enabled = true;
    };
  };

  # Materialised into the Nix store so commands reference it via
  # `--config` without checking a file into the repo.
  rumdlConfig = (pkgs.formats.toml { }).generate "rumdl.toml" (
    pkgs.lib.recursiveUpdate defaultSettings settings
  );

  rumdl = pkgs.rumdl;
in
{
  # The rumdl package, for inclusion in a dev shell's nativeBuildInputs so
  # the tool is also available for ad-hoc use.
  inherit rumdl rumdlConfig;

  # Menu commands, ready to merge into makeMenu's `commands` table.
  menuCommands = {
    "format:${docsDir}" = {
      description = "Format Markdown files in the ${docsDir}/ folder";
      command = ''
        root=$(git rev-parse --show-toplevel)
        ${rumdl}/bin/rumdl fmt --config ${rumdlConfig} "$root/${docsDir}"
      '';
    };
  };

  # Flake checks that fail when the documents drift from the enforced
  # format. Merge into the flake's `checks` output (e.g.
  # `checks = cargoChecks // markdownHelpers.checks;`).
  checks = {
    "${docsDir}" = pkgs.runCommand "lint-${docsDir}" { } ''
      set -e
      ${rumdl}/bin/rumdl check --config ${rumdlConfig} ${workspaceRoot + "/${docsDir}"}
      touch $out
    '';
  };
}
