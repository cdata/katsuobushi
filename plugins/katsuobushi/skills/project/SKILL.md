---
name: project
description:
  Manage a file-backed, Obsidian-Kanban project backlog with the `katsuctl
  project` commands (or the devshell `project` menu command). Use this skill
  when the user wants to track work on a project board, create or file an
  issue/card/ticket, move a card through its lifecycle (to-do, in progress,
  needs review, ready, accepted), reprioritize the backlog, check what's ready
  to work on, mark work done/blocked/cancelled, or run `project init / new /
  status / status set / prioritize / lint`. Also use it whenever you see a
  `project/kanban/` directory with a `BOARD.md` and `<id>.md` cards.
---

# Project backlog (`katsuctl project`)

A lightweight, file-backed backlog rendered as an Obsidian Kanban board. Prefer
the `katsuctl project` commands over hand-editing files — they enforce the
lifecycle and keep the board consistent. Pass `--json` for machine-readable
output when you need to parse results.

## The model (read this first)

**`BOARD.md` is the source of truth for status and priority.** A card's **lane**
is its status; its **position** within a lane is its priority (top = highest).
One exception: when a card **enters the Ready lane** it is auto-slotted into
_suggested acceptance order_ — after any of its still-Ready blockers, then by
`created` (oldest first) — so Ready reads top-to-bottom as a landing chronology.
Only the entering card is placed; cards already in Ready keep their order, so a
manual `prioritize` there is preserved. Everything else about a card lives in
its note, `project/kanban/issues/<id>.md`:

```yaml
---
id: a3f7b2 # immutable 6-hex; the note's filename is issues/<id>.md
title: Device identity and data root
type: feature # feature | bug | chore | docs
blocked_by: [1a2b3c] # hex ids; a blocker clears its dependents at `ready`
design: PDD005 # optional design-doc reference
labels: [security] # optional freeform tags
created: 2026-07-17T18:22:04Z
# disposition: accepted             # appears only once the card is terminal (archived)
# disposition_at: 2026-07-19T09:00:00Z  # when it became terminal (paired with disposition)
---
## What to build
…
## Acceptance criteria
- [ ] …
## Review notes
<!-- reviewers / the owner append context here on a bounce or return -->
```

Notes do **not** carry `status` or `priority` — those are the board's job. A
card on the board is a bare `[[<id>]]` link: Obsidian shows the short id, and
surfaces the note's `title` (and `type`/`design`/`blocked_by`/`labels`) on the
card face live from frontmatter — so nothing is copied and nothing needs
resyncing when a title changes.

## Lifecycle

```text
To-do ──grab──▶ In Progress ──submit──▶ Needs Review ──review──▶ Ready ──accept──▶ Accepted
```

- **Todo → In Progress**: someone picks the card up.
- **In Progress → Needs Review**: the implementer considers it done.
- **Needs Review → Ready**: a reviewer accepts it. Or **Needs Review → In
  Progress**: the reviewer requests changes (append the feedback to the card's
  `## Review notes` first).
- **Ready → Accepted**: the product owner signs off. Or **Ready → To-do**: the
  owner returns it (append context first).
- Any non-accepted state **→ Cancelled** (won't-do tombstone).

`status set` rejects illegal jumps (e.g. `todo → accepted`); use `--force` only
for a genuine, deliberate exception. **Accepted and Cancelled are terminal** —
they move to the Kanban archive and record `disposition` on the note, plus a
`disposition_at` RFC-3339 timestamp of when the card crossed into that terminal
state. (A forced reopen out of terminal clears both.) A regression in accepted
work becomes a **new card**, not a reopen.

Freshly archived cards stay on the `project status` list for **24h** (by
`disposition_at`), then drop off to keep the view focused on live work; `--json`
still returns every archived card, so tooling is unaffected.

> **Only a human — the product owner — moves a card to `accepted`.** As an
> agent, never run `status set <id> accepted` (nor `status set --accept-all`,
> which accepts every Ready card at once) unless a human explicitly tells you to
> in this session. Take work to `ready` and stop.

## What's grabbable

A card is **Available** when it is in To-do and every id in its `blocked_by` has
reached `ready` or `accepted` (downstream builds only on reviewed work). Find
grabbable work with:

```bash
katsuctl project status --available          # or `project status --available`
katsuctl project status --available --json   # to parse
```

## Common tasks

```bash
project init                                     # scaffold project/kanban/
project new --title "Add repo list endpoint" --type feature --design PDD006
project new --title "…" --blocked-by a3f7b2,1a2b3c --labels net
echo "## What to build…" | project new --title "…" --body -   # pipe a body
project status set a3f7b2 in-progress          # ids accept a unique prefix
project status set --accept-all                # accept the whole Ready lane at once
project prioritize a3f7b2 --top                # --top/--bottom/--before/--after
project status                                    # the whole board
project status --lane needs-review               # review queue (or --available)
project status a3f7b2                           # one card: detail + body
project lint                                      # board <-> note consistency (--fix)
```

`new` prints the new id and note path; open that note to flesh out the body, or
pipe the whole body in with `--body -`.

## Editing on the board

You may drag cards in Obsidian — the board is authoritative, so a drag _is_ a
real move. But Obsidian does not know the lifecycle rules, so prefer
`status set` when the state machine matters. After any bulk hand-editing, run
`project lint` to catch orphans (a card whose note is gone, or a note with no
card). `lint --fix` prunes orphan card lines.

## Conventions

- The card `type` maps to the closing commit's Conventional-Commit prefix:
  `feature → feat`, `bug → fix`, `chore → chore`, `docs → docs`.
- When a review bounces or an owner returns a card, **write why** in the card's
  `## Review notes` before (or after) the `status set` — the tool does not
  capture it for you.
- One card per unit of work; keep `blocked_by` honest so `status --available`
  stays meaningful.
- **Re-stage the board after CLI mutations.** These commands edit the working
  tree; anything reading the git/flake source (e.g. the `project-lint` check)
  sees only tracked files, so `git add` the board dir after moving cards or
  `new`, or lint reports phantom orphans.

## Driving the board with agents

To delegate cards to sandbox VMs, review work in an isolated VM, or run a swarm
across the backlog, see the **`project-orchestration`** skill (the implementor /
reviewer / human-owner roles, `sandbox dispatch`, and the report bridge).
