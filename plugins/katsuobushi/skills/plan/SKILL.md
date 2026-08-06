---
name: plan
description:
  Turn an accepted Project Design Document (PDD) into board cards for a
  Katsuobushi project. Use this skill when the user wants to run `/plan`, break
  a design into work, file a PDD's work as issues/cards/tickets, or fill the
  board from an accepted design. It splits the PDD into atomic To-do cards, each
  carrying the PDD's label, its blockers, a type, and the PDD's matching test
  cases as acceptance criteria. It files the cards on its own. It does not write
  code or drive sandboxes — that is the `implement` skill. Load the `project`
  skill first.
---

# From a PDD to board cards (`plan`)

`plan` reads an accepted PDD and files its work as **cards** on the board. It is
the middle skill of three. A person accepts a design from the **`design`**
skill, runs `plan` to file the work, and runs the **`implement`** skill to build
it. This skill files the cards. It does not build them.

Load the **`project`** skill first. `plan` runs its `project new` verbs to file
each card. The `project` skill owns the card model, the
`feature | bug | chore | docs` types, and the six-state lifecycle. `plan`
references those rules. It does not restate them.

## When to use this

Run `plan` when a person has an **accepted** PDD and wants its work on the
board. If the design is not settled yet, that is the `design` skill. If the
cards are filed and the person wants them built, that is `implement`.

## What `plan` files

`plan` splits the PDD into **atomic** cards. One card is one unit of work, small
enough to build and review on its own. Read the PDD's Body and Test Cases to
find the units. Each Test Cases facet is usually one card, or part of one.

`plan` files each card to **To-do**. Each card carries:

- **the PDD's label** — for example `PDD007`. The shared label makes the cards
  an **epic** (see the `project` skill's `--label` filter).
- **a `blocked_by` edge** — one edge to each card this card depends on. Use the
  PDD's own order. A card that needs another card's output is blocked by it.
- **a type** — one of `feature | bug | chore | docs`, from the nature of the
  work.
- **acceptance criteria** — the PDD's matching Test Cases, written into the
  card's `## Acceptance criteria`. A card carries the checks that qualify it.

## `plan` files on its own

`plan` files the cards without a pause for approval. The board owner reorders or
removes the cards after. A plan is a first draft of the work, not a contract.

**A plan is intended work, not a stray idea.** `plan` files to To-do, not to the
icebox. The icebox holds ideas that wait for triage. A plan is the opposite: it
is work a person has already chosen to do.

## How `plan` files a card

`plan` uses the `project new` verbs (see the `project` skill):

```sh
project new --title "…" --type feature --label PDD007 \
  --blocked-by <id>,<id> --body -   # pipe the acceptance criteria on stdin
```

- `--label` sets the epic label. `--blocked-by` takes the ids this card depends
  on. Pipe the card body — the `## What to build` prose and the
  `## Acceptance criteria` list — on stdin with `--body -`.
- **File a blocker before its dependent.** `--blocked-by` needs the blocker's
  id, so file the blocker first and read its id from the output.
- **Re-stage the board after filing.** `project new` writes new files. Git sees
  them only when they are tracked. So `git add` the board directory after each
  filing run. If you do not, `project lint` reports phantom orphans.

## What `plan` does not do

- It does not settle the design. A person accepts the PDD before `plan` runs.
- It does not write code or drive sandboxes. That is the `implement` skill.
- It does not accept or reorder cards for the person. The owner triages after.

## References

- The **`project`** skill — `plugins/katsuobushi/skills/project/SKILL.md`. The
  card model, the types, the lifecycle, and the `project new` verbs this skill
  uses.
- The **`design`** skill — writes the accepted PDD this skill reads.
- The **`implement`** skill — takes the filed cards to `ready`.
- The **PDD template** — `project/design/README.md`. The Test Cases section this
  skill lifts into a card's acceptance criteria.
