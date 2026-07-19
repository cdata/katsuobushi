# Katsuobushi project-backlog library.
#
# Wraps the `katsuctl project` domain (the Obsidian-Kanban-native backlog) as a
# dev-shell `menuCommand`. The board's logic lives in the tested Rust binary;
# this is a thin shell over it. Rather than re-enumerate katsuctl's subcommands
# in Nix (which would duplicate its clap), `project` is a single **pass-through**
# leaf: `project <anything>` forwards verbatim to `katsuctl project --board-dir
# <boardDir> <anything>`, so katsuctl's clap is the single source of truth for
# subcommands and help. (Same spirit as `lib/sandbox`'s handOff.)
#
# BOARD.md is authoritative for lifecycle (lane) and priority (position); card
# notes own identity/detail/dependencies. See design/project.md.
#
# Usage (in a consuming flake):
#
#   project = katsuobushi.lib.project {
#     inherit pkgs;
#     # The host-side controller, built once. Katsuobushi exposes it as
#     # `packages.<system>.katsuctl`; a consumer passes that here.
#     katsuctl = katsuobushi.packages.${system}.katsuctl;
#     # workspaceRoot is only needed for the optional `lint` flake check.
#     workspaceRoot = ./.;
#     # boardDir = "project/kanban";   # default
#   };
#
#   # Merge into the menu, and (optionally) the flake checks:
#   commands = … // project.menuCommands;
#   checks   = … // project.checks;
{
  pkgs,
  # The built `katsuctl` binary (host-side controller). Required — this library
  # does not rebuild it; share the one build (`packages.katsuctl`).
  katsuctl,
  # The board directory, relative to the repo root where the command runs.
  boardDir ? "project/kanban",
  # Repo root, for the optional `lint` check. When null, no check is emitted.
  workspaceRoot ? null,
}:
let
  inherit (pkgs) lib;

  katsuctlBin = "${katsuctl}/bin/katsuctl";
  boardArg = "--board-dir ${lib.escapeShellArg boardDir}";

  # clap qualifies its usage/error/help text with the real binary path — e.g.
  # `Usage: katsuctl project status …` and `'katsuctl project' requires a
  # subcommand`. Rewrite `katsuctl project` back to `project` so what a user
  # sees matches what they typed.
  usageSed = "${pkgs.gnused}/bin/sed -E 's|katsuctl project|project|g'";

  menuCommands = {
    project = {
      description = "Manage the Obsidian-Kanban project backlog";
      # A single pass-through: forward the whole post-dispatch argv to katsuctl.
      # `--board-dir` is a global arg so it may precede any subcommand (incl.
      # `status set`). katsuctl runs in the foreground (not `exec`) so the filter
      # shell survives to post-process output; `|| ret=$?` + pipefail preserve
      # its exit code. Agents wanting raw output pass `--json`, which flows
      # through untouched (see the stdout/fd3 note below).
      #
      # The prefix rewrite is unsafe on data stdout (a card titled "fix katsuctl
      # project lint" must not be mangled), so we normally filter only stderr —
      # where clap prints Usage/errors — and let real stdout (a `--json` payload,
      # the board table) flow through fd3 untouched. The one exception is
      # `-h`/`--help`: clap prints help to *stdout* and emits no card data, so
      # when help is requested it is safe (and necessary) to rewrite stdout too.
      command = ''
        ret=0
        # Detect help by inspecting each arg *exactly* — so a value like
        # `--title "fix -h flag"` (one arg, not the `-h` flag) never misroutes.
        help=0
        for a in "$@"; do
          case "$a" in -h | --help) help=1 ;; esac
        done
        if [ "$help" -eq 1 ]; then
          ${katsuctlBin} project ${boardArg} "$@" 2>&1 | ${usageSed} || ret=$?
        else
          { ${katsuctlBin} project ${boardArg} "$@" 2>&1 1>&3 | ${usageSed} >&2; } 3>&1 || ret=$?
        fi
        exit "$ret"
      '';
    };
  };

  # Optional flake check: fail CI when the board and its notes drift. Runs
  # read-only against the tracked board in the flake source. katsuctl is
  # Linux-only, so a consumer merges this under its own isLinux guard.
  checks = lib.optionalAttrs (workspaceRoot != null) {
    project-lint = pkgs.runCommand "project-lint" { } ''
      cd ${workspaceRoot}
      ${katsuctl}/bin/katsuctl project --board-dir ${lib.escapeShellArg boardDir} lint
      touch $out
    '';
  };
in
{
  inherit menuCommands checks;
}
