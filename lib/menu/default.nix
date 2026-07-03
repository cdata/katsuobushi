# Helpers for assembling and printing dev shell menus.
#
# This library provides three functions:
#
#   makeMenu         — Builds a menu data structure from a set of named commands.
#   makeDevShellHook — Consumes a menu and produces a shellHook string suitable
#                      for use in mkShell.
#   makeColorizer    — Creates a colorizer command that applies a single solid
#                      color, given a hex code.
#
# Together they give Nix devshells a friendly, colorful TUI greeting: an
# optional ASCII graphic, a figlet title, and a table of available commands —
# all piped through a configurable colorizer (lolcat by default) for styled
# output.
#
# Each command in the menu also becomes its own shell application (derivation)
# added to the devshell's packages, so users can invoke commands by name. When
# run, each command displays its own figlet banner and description before
# executing its script body.
#
# Usage (in a consuming flake's devShell):
#
#   # Apply the overlay when importing nixpkgs:
#   pkgs = import nixpkgs {
#     inherit system;
#     overlays = [ katsuobushi.overlays.default ];
#   };
#
#   # Then use pkgs.katsuobushi:
#   let
#     inherit (pkgs.katsuobushi) makeMenu makeDevShellHook;
#
#     menu = makeMenu {
#       title = "My Project";
#       graphic = ''
#         Some optional ASCII art here
#       '';
#       commands = {
#         build = {
#           description = "Build the project";
#           command = "cargo build --release";
#         };
#         test = {
#           description = "Run the test suite";
#           command = "cargo test";
#           env = { RUST_LOG = "debug"; };
#         };
#       };
#     };
#   in
#   pkgs.mkShell {
#     packages = menu.commands;
#     shellHook = makeDevShellHook menu;
#   };

{ pkgs }:
let

  # escapeForDoubleQuotes :: string -> string
  #
  # Escapes characters that have special meaning inside a bash double-quoted
  # string (backslash, double-quote, backtick, dollar sign). This allows
  # arbitrary text — such as ASCII art — to be safely embedded in an
  # `echo "..."` expression without triggering unintended shell interpretation.
  escapeForDoubleQuotes =
    str:
    builtins.replaceStrings
      [ "\\" "\"" "`" "$" ]
      [ "\\\\" "\\\"" "\\`" "\\$" ]
      str;

  # makeMenu :: { commands, title, graphic?, graphicFile?, colorizer?, colorizeGraphic? } -> { header, menuText, commands }
  #
  # Accepts:
  #   commands  — An attribute set of command *nodes* forming a tree. Each
  #               top-level key becomes one binary + one menu row. A node is
  #               either a leaf or a branch:
  #                 description : string  — One-line summary (menu row / listing).
  #                 help        : string  — (Optional) Multi-line usage preamble,
  #                                         shown when a branch is run bare or
  #                                         with -h/--help.
  #                 command     : string  — (Leaf) Shell body; sees the argv left
  #                                         after dispatch as "$@".
  #                 env         : attrset — (Leaf, optional) Environment variables
  #                                         exported before the command runs.
  #                 subcommands : attrset — (Branch) Child nodes, keyed by the
  #                                         token that selects them; nests to any
  #                                         depth. Mutually exclusive with command.
  #   title     — The project/devshell title, rendered large via figlet.
  #   graphic   — (Optional, default "") ASCII art displayed above the title,
  #               inlined as a string. Best for plain text; if the art contains
  #               raw ANSI escape (ESC, U+001B) bytes, use graphicFile instead —
  #               inlining control characters makes them part of the shellHook,
  #               which `nix develop` rejects when it serializes the environment
  #               to JSON.
  #   graphicFile — (Optional, default null) A path whose contents are catted at
  #               runtime to produce the banner. Takes precedence over graphic.
  #               Because the bytes live in a store file and only the store path
  #               appears in the shellHook, this safely handles pre-colorized
  #               ANSI art (e.g. terminal pixel art). Pair with
  #               colorizeGraphic = false so the embedded colors are preserved.
  #   colorizer — (Optional, default lolcat) Shell command used to colorize
  #               menu output. Receives text on stdin.
  #   colorizeGraphic — (Optional, default true) When false, the ASCII art
  #               banner is printed raw and only the title and command table are
  #               run through the colorizer. Has no effect when no graphic is set.
  #
  # Returns an attrset with:
  #   header   — Shell snippet that prints the full greeting (graphic + title +
  #              command table), colorized with the colorizer.
  #   menuText — Shell snippet that prints only the command table, colorized
  #              with the colorizer. Useful for re-displaying the menu on demand.
  #   commands — A list of derivations (one per command). Each derivation is a
  #              writeShellApplication that shows a figlet banner and description
  #              before running the command's script.
  makeMenu =
    {
      commands,
      title,
      graphic ? "",
      graphicFile ? null,
      colorizer ? "${pkgs.lolcat}/bin/lolcat",
      colorizeGraphic ? true,
    }:
    let
      # Sorted list of command names (attrNames returns them alphabetically).
      names = builtins.attrNames commands;

      # Command nodes form a tree. A node is either a LEAF (has `command`) or a
      # BRANCH (has `subcommands`, an attrset of child nodes); the two are
      # mutually exclusive. Every node carries a one-line `description` (its menu
      # row and its entry in a parent's listing) and an optional multi-line
      # `help`. Each top-level node compiles to exactly ONE shell application
      # whose body is the whole subtree inlined:
      #   * a branch becomes a `case` that dispatches on the next argv token,
      #     `shift`s it, and recurses into the matched child; bare invocation (or
      #     `-h`/`--help`) prints usage and exits 0; an unknown token errors;
      #   * a leaf prints its figlet banner + description, then runs `command`
      #     with the post-dispatch argv still available as "$@".
      # Only top-level nodes become binaries and menu rows — subcommands are
      # reached by walking argv inside their group's single binary.

      # nodeKind :: string -> node -> "leaf" | "branch"  (throws on an ill-formed
      # node so a typo fails loudly at eval time instead of vanishing).
      nodeKind =
        pathStr: node:
        if (node ? command) && (node ? subcommands) then
          throw "katsuobushi menu: '${pathStr}' has both `command` and `subcommands`; a node must be exactly one."
        else if node ? command then
          "leaf"
        else if node ? subcommands then
          "branch"
        else
          throw "katsuobushi menu: '${pathStr}' has neither `command` nor `subcommands`.";

      # NAME=value exports for a leaf's optional `env`, safely quoted and emitted
      # inside the leaf's own case arm so sibling subcommands never inherit each
      # other's environment.
      envExports =
        env:
        pkgs.lib.concatStrings (
          pkgs.lib.mapAttrsToList (k: v: "export ${k}=${pkgs.lib.escapeShellArg v}\n") env
        );

      # renderLeaf :: string -> node -> string
      #
      # Banner (figlet title + description via the colorizer) then the script,
      # which sees the argv left after dispatch as "$@".
      renderLeaf = name: node: ''
        TITLE="$(${pkgs.figlet}/bin/figlet -t '${name}')"
        SUBTITLE="${node.description}"

        echo "$TITLE
        $SUBTITLE
        " | ${colorizer}

        ${envExports (node.env or { })}${node.command}
      '';

      # renderUsage :: string -> node -> attrset -> string
      #
      # The optional `help` preamble, a Usage line, and the aligned subcommand
      # table (';'-delimited, aligned by `column`). Arbitrary node text (which
      # may contain `$`, backticks, or quotes) is emitted through double-quoted
      # echo/printf with escapeForDoubleQuotes neutralizing shell
      # metacharacters — so the printed text is literal and the generated script
      # still passes writeShellApplication's shellcheck (single-quoted bodies
      # would trip SC2016 on any `$`/backtick in a description).
      renderUsage =
        fullPath: node: children:
        let
          helpPreamble =
            if (node.help or null) != null then
              ''printf '%s\n\n' "${escapeForDoubleQuotes node.help}"'' + "\n"
            else
              "";
          listing = escapeForDoubleQuotes (
            pkgs.lib.concatStringsSep "\n" (
              pkgs.lib.mapAttrsToList (n: c: "${n};${c.description}") children
            )
          );
        in
        ''
          ${helpPreamble}echo "Usage: ${fullPath} <subcommand> [args]"
          echo ""
          echo "Subcommands:"
          echo "${listing}" | ${pkgs.util-linux}/bin/column -t -s ';'
        '';

      # renderBranch :: [string] -> string -> node -> string
      #
      # `path` is the chain of ancestor names (for the Usage line); the branch's
      # own name is appended. Each child becomes a case arm that shifts the
      # matched token and recurses.
      renderBranch =
        path: name: node:
        let
          fullPath = pkgs.lib.concatStringsSep " " (path ++ [ name ]);
          children = node.subcommands;
          arms = pkgs.lib.concatStrings (
            pkgs.lib.mapAttrsToList (childName: child: ''
              '${childName}')
                shift
                ${renderNode (path ++ [ name ]) childName child}
                ;;
            '') children
          );
          usage = renderUsage fullPath node children;
        in
        ''
          case "''${1:-}" in
          ${arms}  "" | -h | --help)
              ${usage}
              exit 0
              ;;
            *)
              {
                echo "Unknown subcommand: ''${1}"
                echo ""
                ${usage}
              } >&2
              exit 2
              ;;
          esac
        '';

      # renderNode :: [string] -> string -> node -> string  (leaf or branch).
      renderNode =
        path: name: node:
        let
          pathStr = pkgs.lib.concatStringsSep " " (path ++ [ name ]);
        in
        if nodeKind pathStr node == "leaf" then renderLeaf name node else renderBranch path name node;

      # intoPackages :: string -> derivation
      #
      # Compiles a top-level node into its single shell application; the whole
      # subtree below it is inlined into that one script.
      intoPackages =
        name:
        pkgs.writeShellApplication {
          inherit name;
          text = renderNode [ ] name (builtins.getAttr name commands);
        };

      # escapeForSingleQuotes :: string -> string
      #
      # Escapes single quotes for safe inclusion inside a bash single-quoted
      # string, using the standard '\'' idiom (close, escaped-quote, reopen).
      # Without this, a command name or description containing an apostrophe
      # (e.g. "the instance's branch") would prematurely close the quote in the
      # menu's `echo '...'` and produce a shell syntax error.
      escapeForSingleQuotes = builtins.replaceStrings [ "'" ] [ "'\\''" ];

      # intoLines :: string -> string -> string
      #
      # Fold accumulator that builds a chain of echo statements separated by
      # "&&". Each echo prints "name;description" — the semicolon is later used
      # as a column delimiter so the menu aligns neatly.
      intoLines =
        acc: name:
        let
          description = (builtins.getAttr name commands).description;
        in
        acc + " && echo '${escapeForSingleQuotes name};${escapeForSingleQuotes description}'";

      # The list of derivations — one shell application per command.
      scripts = map intoPackages names;

      # A shell expression that echoes all "name;description" pairs.
      menuLines = builtins.foldl' intoLines "echo ''" names;

      # Shell snippet that pipes the menu lines through `column` for aligned
      # tabular output, using ";" as the field separator.
      menu = ''
        echo "$(${menuLines})" | column -t -s ';'
      '';

      # Prefix the graphic with a trailing newline if present, otherwise empty.
      # The graphic is escaped for safe inclusion inside echo "...".
      graphicSection = if graphic != "" then escapeForDoubleQuotes graphic + "\n" else "";

      # Whether any banner is configured at all.
      hasGraphic = graphicFile != null || graphic != "";

      # Shell snippet that writes the banner (uncolorized) to stdout. A
      # graphicFile is catted from its store path at runtime, so its raw bytes
      # (including ANSI escapes) never enter the shellHook string; an inlined
      # graphic is echoed from the escaped, JSON-safe string. Each form ends
      # with a blank line separating the banner from the title.
      emitGraphic =
        if graphicFile != null then
          ''cat ${graphicFile}; echo ""''
        else if graphic != "" then
          ''echo "${graphicSection}"''
        else
          "";

      # Shell snippet that echoes the figlet title and command table.
      emitBody = ''
        echo "$(${pkgs.figlet}/bin/figlet -t "${title}")

        $(${menu})
        "'';
    in
    {
      # header: Full greeting — graphic, figlet title, and command table.
      # Normally the banner, title, and table stream through a single colorizer
      # invocation for a continuous rainbow. When colorizeGraphic is false and a
      # banner is set, the banner is written raw (preserving any colors it
      # already carries) and only the title and command table are colorized.
      header =
        if hasGraphic && !colorizeGraphic then
          ''
            ${emitGraphic}
            ${emitBody} | ${colorizer};
          ''
        else if hasGraphic then
          ''
            { ${emitGraphic}; ${emitBody}; } | ${colorizer};
          ''
        else
          ''
            ${emitBody} | ${colorizer};
          '';

      # menuText: Just the command table, colorized. Handy for showing the menu
      # again without reprinting the entire greeting.
      menuText = ''
        echo "$(${menu})" | ${colorizer}
      '';

      # commands: List of derivations to include in mkShell's `packages`.
      commands = scripts;
    };

  # makeColorizer :: string -> string
  #
  # Accepts a hex color code (with or without leading "#") and returns a path
  # string to a shell script that colorizes stdin text in the nearest matching
  # terminal color. The returned string is suitable for use as the `colorizer`
  # argument to makeMenu.
  #
  # Example:
  #   makeColorizer "#ff5733"
  #   makeColorizer "a3be8c"
  makeColorizer =
    hex:
    let
      cleanHex =
        if builtins.substring 0 1 hex == "#" then builtins.substring 1 6 hex else hex;
      rHex = builtins.substring 0 2 cleanHex;
      gHex = builtins.substring 2 2 cleanHex;
      bHex = builtins.substring 4 2 cleanHex;

      pkg = pkgs.writeShellApplication {
        name = "colorize";
        text = ''
          printf '\033[38;2;%d;%d;%dm' "0x${rHex}" "0x${gHex}" "0x${bHex}"
          cat
          printf '\033[0m'
        '';
      };
    in
    "${pkg}/bin/colorize";

  # makeDevShellHook :: { header, menuText, ... } -> string
  #
  # Consumes the output of makeMenu and produces a shellHook string. When the
  # devshell starts, it:
  #   1. Clears the terminal.
  #   2. Prints the full header (graphic + title + menu).
  #   3. Defines and exports a `showMenu` function so the user can re-display
  #      the command table at any time by typing `showMenu` in the shell.
  makeDevShellHook =
    { header, menuText, ... }:
    ''
      clear
      ${header}

      function showMenu() {
        ${menuText}
      }

      export -f showMenu
    '';
in
{
  inherit makeMenu makeDevShellHook makeColorizer;
}
