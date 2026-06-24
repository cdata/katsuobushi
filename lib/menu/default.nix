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

  # makeMenu :: { commands, title, graphic?, colorizer? } -> { header, menuText, commands }
  #
  # Accepts:
  #   commands  — An attribute set of command definitions. Each key becomes the
  #               command name, and each value is an attrset with:
  #                 description : string  — One-line summary shown in the menu.
  #                 command     : string  — Shell script body to execute.
  #                 env         : attrset — (Optional) Environment variables
  #                                         injected via runtimeEnv.
  #   title     — The project/devshell title, rendered large via figlet.
  #   graphic   — (Optional, default "") ASCII art displayed above the title.
  #   colorizer — (Optional, default lolcat) Shell command used to colorize
  #               menu output. Receives text on stdin.
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
      colorizer ? "${pkgs.lolcat}/bin/lolcat",
    }:
    let
      # Sorted list of command names (attrNames returns them alphabetically).
      names = builtins.attrNames commands;

      # makeCommand :: { name, script, description?, env? } -> { name, description, package }
      #
      # Wraps a single command definition into a shell application derivation.
      # When the resulting program is executed, it:
      #   1. Renders the command name as large ASCII text via figlet.
      #   2. Prints the description beneath the banner.
      #   3. Pipes both through lolcat for colorful output.
      #   4. Runs the actual script body.
      makeCommand =
        {
          name,
          script,
          description ? "<No description given>",
          env ? { },
        }:
        {
          inherit name description;

          package = pkgs.writeShellApplication {
            inherit name;
            runtimeEnv = env;
            text = ''
              TITLE="$(${pkgs.figlet}/bin/figlet -t '${name}')"
              SUBTITLE="${description}"

              echo "$TITLE
              $SUBTITLE
              " | ${colorizer}

              ${script}
            '';
          };
        };

      # intoPackages :: string -> derivation
      #
      # Maps a command name to its writeShellApplication derivation by looking
      # up the command definition in the `commands` attrset and passing it
      # through makeCommand.
      intoPackages =
        name:
        let
          element = builtins.getAttr name commands;

          task = makeCommand {
            inherit name;
            description = element.description;
            script = element.command;
            env = if builtins.hasAttr "env" element then element.env else { };
          };
        in
        task.package;

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
    in
    {
      # header: Full greeting — graphic, figlet title, and command table, all
      # colorized through a single lolcat invocation for a continuous rainbow.
      header = ''
        echo "${graphicSection}
        $(${pkgs.figlet}/bin/figlet -t "${title}")

        $(${menu})
        " | ${colorizer};
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
