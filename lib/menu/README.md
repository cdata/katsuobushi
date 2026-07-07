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

A built-in `menu` command ("Print this menu.") is always added. It reprints the
command table on demand, with its own figlet banner + description just like any
other command — but without the graphic greeting, which is shown once when you
first drop into the dev shell (via `makeDevShellHook`), not every time you list
the commands. Supply your own `menu` in `commands` to override the built-in.

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

### Subcommands

A command is a tree. The leaves above carry a `command`; a **branch** instead
carries a `subcommands` attrset of child nodes, and compiles to a single binary
that dispatches on its first argument. This keeps a cluster of related commands
to one menu row (and one PATH binary) instead of one row each:

```nix
commands = {
  db = {
    description = "Database tasks";
    help = "Run `db <subcommand>` to migrate or seed the local database.";
    subcommands = {
      migrate = {
        description = "Apply pending migrations";
        command = "diesel migration run";
      };
      seed = {
        description = "Load seed data";
        command = ''psql < ./seed.sql "$@"'';  # sees the post-dispatch argv
      };
    };
  };
};
```

`db migrate` runs the leaf; a bare `db` (or `db -h` / `db --help`) prints the
`help` preamble and the subcommand table. Branches nest to any depth, and only
top-level keys become menu rows and binaries.

Other Katsuobushi libraries expose a `menuCommands` attrset designed to merge
straight into `makeMenu`'s `commands` — `markdown.menuCommands` (a `<name>`
branch with `format` / `lint` subcommands) and `sandbox.menuCommands` (a
`sandbox` branch with the lifecycle verbs).
