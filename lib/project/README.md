# Katsuobushi Project Backlog

A file-backed, **Obsidian-Kanban-native** project backlog, driven by the
`katsuctl project` domain and exposed here as dev-shell menu commands. This
library is a thin wrapper; all the logic (a Kanban parser/writer, a lifecycle
state machine, a dependency graph) lives in the tested Rust binary. See
`design/project.md` for the full design.

## Model

**`BOARD.md` is the source of truth for lifecycle and priority.** A card's lane
is its status; its vertical position within a lane is its priority. Card notes
(`issues/<id>.md`, a 6-hex id) own everything else — `title`, `type`,
`blocked_by`, `design`, `labels`, and the body. A card on the board is a bare
`[[<id>]]` link: Obsidian shows the short id and the Kanban plugin's _metadata
keys_ setting surfaces the note's `title` and other fields on the card face,
live from frontmatter — so nothing is copied.

| Concern           | Lives in   | As                                  |
| ----------------- | ---------- | ----------------------------------- |
| Status            | `BOARD.md` | which `## Lane` the card is under   |
| Priority          | `BOARD.md` | the card's position within its lane |
| Identity / detail | note       | `issues/<id>.md` + its frontmatter  |
| Dependencies      | note       | `blocked_by: [hex, …]`              |

## Lifecycle

```text
To-do -> In Progress -> Needs Review -> Ready -> Accepted
```

`needs-review -> in-progress` is a reviewer bounce; `ready -> todo` is an owner
return. `cancelled` is reachable from any non-accepted state. Accepted and
Cancelled cards move to the Kanban archive, with the outcome recorded on the
note as `disposition:`. `status set` enforces these transitions (`--force`
bypasses). **Only a human moves a card to `accepted`.**

## Commands

| Command                            | Does                                                                |
| ---------------------------------- | ------------------------------------------------------------------- |
| `project init`                     | Scaffold the board directory (idempotent)                           |
| `project new --title "…"`          | Mint a card note + a card in To-do                                  |
| `project status set <id> <status>` | Move a card (enforces the state machine)                            |
| `project prioritize <id> --top`    | Reorder within a lane (`--top/--bottom/--before <id>/--after <id>`) |
| `project status [--available]`     | Show the board (`--lane <lane>`, `--json`)                          |
| `project status <id>`              | Show one card's status, frontmatter, and body                       |
| `project lint [--fix]`             | Check board ↔ note consistency                                      |

Ids accept a unique prefix. `--json` gives machine output for in-sandbox agents
(they invoke `katsuctl project …` directly, bypassing the menu banner).

## Usage

```nix
project = katsuobushi.lib.project {
  inherit pkgs;
  # The host-side controller, built once. Katsuobushi exposes it as
  # `packages.<system>.katsuctl`.
  katsuctl = katsuobushi.packages.${system}.katsuctl;
  # Only needed for the optional `project-lint` flake check.
  workspaceRoot = ./.;
  # boardDir = "project/kanban";   # default
};
```

Then merge into the menu and (optionally) the flake checks:

```nix
commands = markdown.menuCommands // project.menuCommands;
checks   = rust.cargoChecks // project.checks;
```

`katsuctl` is Linux-only (it links `tokio-vsock` for the sandbox domain), so
guard `menuCommands`/`checks` with your own `isLinux` check as this repo's own
flake does.
