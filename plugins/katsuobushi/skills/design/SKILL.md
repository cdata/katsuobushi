---
name: design
description:
  Author a Project Design Document (PDD) for a Katsuobushi project. Interview
  the author, settle the design, and write the PDD to `project/design/`. Use
  this skill when the user wants to write or draft a design doc, run `/design`,
  turn rough notes or an idea into a PDD, grill a design before it is built, or
  add the next `PDDNNN` to a `project/design/` directory. It writes one PDD in
  the six fixed sections, in Simplified Technical English, and ends with a
  context-free read-through. It does not file cards or write code. The `plan`
  and `implement` skills do that.
---

# Author a PDD (`design`)

A **PDD** (Project Design Document) records what a design is and why, before any
code. This skill runs the whole authoring ritual: the interview, the draft, the
Simplified Technical English pass, and the read-through.

`design` writes one artifact: a PDD in `project/design/`. It does not file cards
and it does not write code. A person accepts the PDD. Then the person runs the
**`plan`** skill to file its work, and the **`implement`** skill to build it.
Nothing here calls the next skill. A person walks the chain.

## When to use this

Run `design` when a person brings an idea, a rough note, or a problem. The
person wants a durable design before the work starts. If the request is "file
this work" or "build this", that is `plan` or `implement`, not `design`.

## The ritual

Do these steps in order. Do not skip the interview. Do not skip the
read-through.

1. **Load the `grilling` skill.** It interviews the author one question at a
   time.
2. **Interview the author.** Ask hard questions, one at a time. Settle every
   open decision before you draft. A design with an open question is not ready.
3. **Load `simple-english`.** Pick one word for each concept before you draft.
   Keep that word for that concept through the whole document.
4. **Draft to the six sections** the template fixes: Introduction, Goals,
   Non-goals, Body, Test Cases, References.
5. **Match the accepted PDDs** for structure and substance, not for voice. Read
   one or two in `project/design/` first. Copy their shape: user stories in the
   Body, pseudocode where it helps, and a Ground-truth list of hard facts.
6. **Write in Simplified Technical English.** Short sentences. Active voice. One
   word per concept. No filler. No semicolon.
7. **Read the draft with no context.** Read it as a person who was not in the
   interview. Each place you must remember an unwritten fact is a gap. Close
   each gap with an example, pseudocode, a diagram, or a link.
8. **Search the draft for board references.** Remove every card id, lane name,
   and card title. See "A PDD never cites the board".
9. **Write the file** to `project/design/` at the next free number.

## A PDD never cites the board

A PDD is durable. The board is not. A card moves, is renamed, is cancelled, or
is archived, so a PDD that cites one decays into a dead reference. A reader who
finds the PDD years later must not need the board to understand it.

Never write a card id (`5d7555`), a lane name, a card title, or a count of open
work into a PDD. This holds even when a card started the design.

Write the fact, not the card:

- Wrong — "Card 5d7555 proposed foreground blocking. That wording is
  superseded."
- Right — "A rewording toward foreground blocking does not work. One Bash call
  caps at 600 seconds."

The rule bans the **reference**, not the **vocabulary**. "One card is one unit
of work" is the domain language of the board model, and a PDD uses it freely.
"Card 5d7555" is a pointer into mutable state, and a PDD never uses that.

The same rule covers every other mutable pointer: a branch name, a VM instance
name, a run of a swarm, or an in-flight pull request. Cite source files, the
accepted PDDs, and published documents. Those hold still.

## Voice has one priority

STE wins over the voice of the older PDDs. The accepted PDDs are older than the
`simple-english` rule, so some carry long sentences. Copy their structure and
their depth. Do not copy their length. When the two rules disagree, STE wins.

## The number is the next free one

The filename is `PDD<NNN> <Title>.md`, for example
`PDD007 Warm Artifact Sets.md`.

- Read `project/design/` for the highest `PDDNNN`. Add one.
- `PDD000` is the project's top-level product document. `design` never writes
  `PDD000`.
- The template is `project/design/README.md`. It is not a PDD. Skip it when you
  read for the highest number.

## The template's six sections

`project/design/README.md` fixes the sections, in this order:

1. **Introduction** — a short summary of the design.
2. **Goals** — what the design must achieve. One bullet per goal. Keep the
   elaboration for the Body.
3. **Non-goals** — what the design does not cover, and anything held for a later
   PDD.
4. **Body** — the substance. Center human persons over software. Write a
   featureful improvement as a user story.
5. **Test Cases** — one prose entry per facet, each with the criterion that
   makes it acceptable. The `plan` skill lifts these into a card's
   `## Acceptance criteria`, so write each one so a card can carry it.
6. **References** — the most direct link for each cited source.

## When a referenced skill is absent

`design` names two other skills and loads them when they are present. When one
is absent, `design` falls back to a short inline form. The ritual stays whole.

- **`grilling` absent** — interview with one inline rule: one question at a
  time, each question with a recommendation, and no draft until the author
  agrees.
- **`simple-english` absent** — apply this inline digest as you write: short
  sentences, active voice, one word per concept, no filler, and no semicolon.

## What `design` does not do

- It does not file cards. That is the `plan` skill.
- It does not write code or drive sandboxes. That is the `implement` skill.
- It does not accept its own PDD. A person accepts the design before `plan`
  runs.

## References

- The **PDD template** — `project/design/README.md`. The six sections and the
  authoring rules this skill writes to.
- The **`plan`** skill — files an accepted PDD's work as board cards.
- The **`grilling`** skill (mattpocock-skills plugin) — the one-question
  interview.
- The **`simple-english`** skill — the ASD-STE100 rules this skill writes to.
