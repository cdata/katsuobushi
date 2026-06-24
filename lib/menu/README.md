# Katsuobushi Devshell Menu

Helpers for assembling and printing a colorful dev-shell menu. This is the
library the `katsuobushi` overlay exposes as `pkgs.katsuobushi`.

It provides three functions:

| Function           | Purpose                                                                                                                                                                       |
| ------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `makeMenu`         | Build a menu from a set of named commands. Returns `{ header, menuText, commands }` — `commands` is a list of derivations (one shell app per command) to drop into `mkShell`. |
| `makeDevShellHook` | Turn a menu into a `shellHook` that clears the screen, prints the greeting, and defines a `showMenu` function.                                                                |
| `makeColorizer`    | Build a solid-color colorizer from a hex code, for use as `makeMenu`'s `colorizer` (defaults to `lolcat`).                                                                    |

Each command becomes its own shell application (added to the dev shell's
packages), so users can invoke commands by name; running one prints a figlet
banner + description before executing.

## Usage

Apply the overlay when importing nixpkgs, then use `pkgs.katsuobushi`:

```nix
pkgs = import nixpkgs {
  inherit system;
  overlays = [ katsuobushi.overlays.default ];
};

inherit (pkgs.katsuobushi) makeMenu makeDevShellHook;

menu = makeMenu {
  title = "My Project";
  graphic = ''  /\_/\   ( o.o )  '';   # optional ASCII art
  commands = {
    build = {
      description = "Build the project";
      command = "cargo build --release";
    };
    test = {
      description = "Run the test suite";
      command = "cargo test";
      env = { RUST_LOG = "debug"; };   # optional per-command env
    };
  };
};
```

```nix
pkgs.mkShell {
  nativeBuildInputs = menu.commands;
  shellHook = makeDevShellHook menu;
}
```

Other Katsuobushi libraries expose a `menuCommands` attrset designed to merge
straight into `makeMenu`'s `commands` (e.g. `markdown.menuCommands`,
`sandbox.menuCommands`).
