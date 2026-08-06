# PDD002 Design, Plan and Implement Skills

## Introduction

The project has a repeatable way to take a design from an idea to shipped code.
This document turns that way into three **skills**: **`design`**, **`plan`**,
and **`implement`**.

Each skill is independent. A person runs one at a time. The skills do not chain
in one run. They connect through two artifacts: the **PDD** that `design` writes,
and the **board** that `plan` fills and `implement` drains.

`design` interviews the author and writes a PDD to `project/design/`. `plan`
reads an accepted PDD and files its work as **cards** on the board. `implement`
drives a swarm of **sandbox** VMs to take those cards to `ready`, where a person
accepts them.

The board model under these skills comes from
[PDD001](PDD001%20Project%20Board%20Labels%20and%20Icebox.md): labels as epics,
the icebox, and the host-only write rule. This document does not restate that
model. It uses it.

## Goals

- **Three independent skills, one workflow.** `design`, `plan`, and `implement`
  each do one job. A person runs them on their own schedule. They share no run,
  only the PDD and the board.

- **`design` runs the whole authoring ritual.** The skill loads the `grilling`
  skill, interviews the author, drafts to the PDD template, and writes in
  Simplified Technical English. It ends with a read-through for a reader who
  lacks the author's context.

- **`plan` turns an accepted PDD into board cards.** The skill reads the PDD and
  files atomic cards to To-do. Each card carries the PDD's label, its blockers,
  a type, and a slice of the PDD's test cases as acceptance criteria.

- **`implement` keeps a sandbox swarm busy on one work thread.** The skill
  negotiates headcount and roles, then schedules per-card implementor and
  reviewer VMs. It keeps the CPU saturated and takes cards to `ready`.

- **Coherence with the `project` skill.** `plan` and `implement` use the card
  model, the card types, and the lifecycle the `project` skill already defines.
  They reference those rules. They do not restate them.

- **Function without the referenced skills.** `design` names the `grilling` and
  `simple-english` skills and loads them when present. When one is absent, the
  skill falls back to a short inline form.

## Non-goals

- **The board model.** Labels, the icebox, and the host-only write rule belong
  to [PDD001](PDD001%20Project%20Board%20Labels%20and%20Icebox.md). This document
  uses them and does not change them.

- **The sandbox and orchestration mechanics.** How a VM boots, how a branch
  returns, and the report bridge belong to the `sandbox` and
  `project-orchestration` skills. `implement` drives them. It does not redefine
  them.

- **Auto-commit of board mutations.** `implement` adds no durability guard for
  board writes. That work is deferred, as PDD001 states.

- **Triage automation.** `implement` does not remove or reorder cards on its
  own. A person triages the board by hand before a session.

- **A chained pipeline.** No skill calls the next. `design` does not run `plan`,
  and `plan` does not run `implement`. A person walks the chain.

## Body

### The three skills at a glance

```
   author's                                            a person
   raw notes                                           reviews `ready`
      │                                                    ▲
      ▼                                                    │
 ┌──────────┐   a PDD in    ┌────────┐   cards on   ┌───────────┐
 │  design  │─────────────▶ │  plan  │────────────▶ │ implement │
 └──────────┘  project/     └────────┘  the board   └───────────┘
              design/         (labeled To-do cards)   drives sandboxes
   grill → draft →          one card per              per-card impl +
   STE → read-through       unit of work              reviewer VMs
```

Each arrow is an artifact, not a call. A person stands at every arrow. A person
accepts the PDD before `plan`. A person accepts a `ready` card after `implement`.

### Ground truth

Hard facts a fresh reading needs, checked against the tree at the time of
writing:

- **The PDD template is fixed.** `templates/rust/project/design/README.md` names
  six sections: Introduction, Goals, Non-goals, Body, Test Cases, References. A
  consumer project gets this file at `project/design/README.md`.

- **The board model is PDD001.** A label is an epic, read through
  `project status --label`. An iced note has no board card. The host is the only
  writer of the board.

- **The `project` skill owns the card rules.** It defines one card per unit of
  work, the `feature|bug|chore|docs` types, and the six-state lifecycle
  (`plugins/katsuobushi/skills/project/SKILL.md`). `plan` and `implement`
  reference these rules.

- **The `project-orchestration` skill owns the roles.** It defines the
  orchestrator, the implementor, the peer reviewer, and the product owner. It
  defines `sandbox dispatch` and the report bridge — the host's map from a VM's
  report to a board move
  (`plugins/katsuobushi/skills/project-orchestration/SKILL.md`). `implement`
  drives this skill.

- **The `sandbox` skill owns the VM.** It defines
  `sandbox start / prompt / status / fetch / stop` and the landing workflow
  (`plugins/katsuobushi/skills/sandbox/SKILL.md`). A reviewer reads work over the
  `report` channel. `implement` drives this skill.

- **`grilling` and `simple-english` are separate skills.** `grilling` interviews
  the user one question at a time. `simple-english` writes text to the ASD-STE100
  rules. `design` loads both.

### `design`: the authoring ritual

_As the author, I bring rough notes and run `/design`. The skill asks me hard
questions, one at a time. Together we settle the design. Then the skill writes
the PDD and reads it back to me for gaps._

`design` runs these steps in order:

1. Load the `grilling` skill.
2. Interview the author one question at a time. Settle every open decision.
3. Load `simple-english`. Pick one word for each concept before you draft.
4. Draft the PDD to the six template sections.
5. Match the accepted PDDs for structure and substance, not for voice.
6. Write in Simplified Technical English. Keep each sentence short.
7. Read the draft as a person with no context. Add examples, pseudocode,
   diagrams, and links to close each gap.
8. Write the file to `project/design/`. Use the next free `PDDNNN` number.

**Voice has one priority.** STE wins over the voice of the older PDDs. The
accepted PDDs predate the `simple-english` rule. Copy their structure and their
depth. Do not copy their long sentences.

**The number is the next free one.** `design` reads `project/design/` for the
highest `PDDNNN` and adds one. `PDD000` is the project's top-level product
document. `design` never writes `PDD000`.

**The fallback keeps `design` whole.** When the `grilling` skill is absent,
`design` interviews with one inline rule: one question at a time, each with a
recommendation, and no draft until the author agrees. When `simple-english` is
absent, `design` applies an inline digest: short sentences, active voice, one
word per concept, no filler, and no semicolon.

### `plan`: from a PDD to cards

_As the board owner, I run `/plan PDD007` after I accept the design. The skill
files the work as cards. Then I reorder them by hand._

`plan` reads the accepted PDD and splits it into cards. One card is one unit of
work, as the `project` skill defines it. `plan` files each card to To-do. Each
card carries:

- the PDD's **label** (for example `PDD007`), so the cards form an **epic**,
- a `blocked_by` edge to each card it depends on,
- a **type** from `feature|bug|chore|docs`,
- the matching **test cases** from the PDD, as `## Acceptance criteria`.

`plan` files the cards on its own. It does not wait for approval. The board owner
reorders or removes the cards after.

**A plan is intended work, not a stray idea.** `plan` files to To-do, not to the
icebox. The icebox holds ideas that wait for triage. A plan is the opposite.

### `implement`: a scheduler that keeps sandboxes hot

**A work thread is a label.** A **work thread** is the set of cards under one
label. A thread can also include a card's **blockers** that carry a different
label. Before it pulls an out-of-label blocker into scope, `implement` asks the
person.

**Each session opens with a negotiation.** The person and the skill agree on
three things:

- the **concurrency budget**: how many sandboxes run at once, and how many vCPUs
  each one gets,
- the **roles**: how many implementors and how many reviewers,
- the **threads**: which labels the session works.

The budget's ceiling is the worst case. When every sandbox builds at once, the
builds must not oversubscribe the CPU. For example, a 16-core, 32-thread machine
can sustain four sandboxes at eight vCPUs each. All four can build at once and
still fit the physical cores.

**Each card gets its own pair of VMs.** An **implementor** VM does the work. A
**reviewer** VM reads it. Both stay warm across the card's review loop. The build
cache and the context survive each bounce — each time a reviewer sends the card
back for changes. When the card reaches `ready`,
`implement` stops both VMs. The next card gets a fresh pair.

**Parallelism counts active sandboxes, not cards.** A paused VM spends no CPU. So
many per-card VMs can wait while the budget's worth of VMs run. The scheduler
cycles the active slot among the waiting VMs. The goal is a saturated CPU. The
number of cards in flight can exceed the active-sandbox budget.

**The scheduler fills a free slot in a fixed order.** It prefers older work over
new work, so few cards stay open at once:

1. Wake a VM for a card already in the review loop. A bounced implementor
   revises. A reviewer reads a pushed revision.
2. If no in-flight card needs the slot, start the next available card.

This bounds work in progress. Older cards reach `ready` before the swarm starts
more new work.

The scheduler, in pseudocode — the shape, not the implementation:

```
run while the thread has available or in-flight work:
    while active_sandboxes < budget:
        card = pick_card()               # priority below
        wake_or_start(card)              # its warm VM, or a fresh pair
    report = wait_for_a_report()         # done | blocked | verdict
    apply_to_board(report)               # the report bridge

pick_card():
    # 1) in-flight cards first — keep work in progress bounded
    if a bounced implementor waits:            return its card
    if a reviewer has a pushed revision to read: return its card
    # 2) only then, a new card
    return next_available(thread)        # by priority, blockers cleared
```

**A revision reaches the same reviewer through the mirror.** A reviewer reads
work over the `report` channel. It does not write a branch. On a bounce, the
implementor revises the branch. `implement` pushes the revised branch to the
reviewer's `sync.git`, the per-instance git mirror the VM reads. Then the same
reviewer VM reads the new commits. A fresh
reviewer is never started for a bounce.

```
 implementor VM ─commit─▶ sandbox/<card> ─push─▶ the card's sync.git
                                                       │ fetch
                                                       ▼
                                                 reviewer VM ─verdict─▶ host
      ▲                                                                  │
      └────────── bounce: host pushes the revised branch ◀──────────────┘
```

**`implement` stops at `ready`.** It drives a card
`in-progress → needs-review → ready`. It never moves a card to `accepted`. Only
a person accepts. When the thread has no available work left, `implement`
reports the state and stops. It does not idle while `ready` cards wait for the
person.

### Considerations for implementors

- **`implement` is a driver, not a new engine.** It runs `sandbox dispatch`,
  `sandbox fetch`, `sandbox prompt`, and the report bridge from the
  `project-orchestration` skill. Keep its logic in the skill text, not in a new
  tool.

- **The vCPU count is a launch parameter.** The per-sandbox vCPU count is a
  `sandbox start` flag, owned by the `sandbox` skill. `implement` passes the
  negotiated value through.

- **A bounce needs the branch, not a new VM.** A common error starts a fresh
  reviewer on a bounce. Push the revised branch to the existing reviewer's mirror
  instead.

- **A thread selector is a label filter.** `implement` finds its work with
  `project status --available --label=<thread>`. This is the PDD001 filter, not
  new board code.

- **Two gates are human.** A person accepts the PDD before `plan`. A person
  accepts a `ready` card before it is done. Do not automate either gate.

## Test Cases

Acceptance checks, by facet:

- **Skill discovery** — the three skills show as `katsuobushi:design`,
  `katsuobushi:plan`, and `katsuobushi:implement`. Each has a `SKILL.md` with a
  `name` and a `description`. This qualifies when all three load by name.

- **`design` output** — a `/design` run writes one file to `project/design/`.
  The filename is `PDD<NNN> <Title>.md`, the number is the next free one, and the
  number is never `000`. It qualifies when the file exists with the right name.

- **`plan` output** — a `/plan PDD007` run files every card to To-do with the
  `PDD007` label. A dependency between two cards shows as a `blocked_by` edge.
  Each card's `## Acceptance criteria` holds the PDD's matching test cases. It
  qualifies when `project status --label=PDD007` lists the filed cards.

- **`implement` scope** — `implement` finds its thread with
  `project status --available --label=<thread>`, and asks before it adds an
  out-of-label blocker. It qualifies when an out-of-label blocker prompts the
  person and does not enter scope on its own.

- **`implement` gate** — `implement` never runs `project status set <id>
  accepted`. It stops a card at `ready`. It qualifies when no run of the skill
  accepts a card.

By hand, in a live session:

- Run `/design` with rough notes. Check that the skill interviews you one
  question at a time, drafts to the six sections, writes short STE sentences with
  no semicolon, and ends with a read-through for a reader who lacks your context.

- Accept a small PDD, run `/plan` on it, and read the board. Check that the cards
  carry the label, the blockers match the design's order, and the acceptance
  criteria trace back to the PDD.

- Negotiate a budget of two sandboxes for one thread, then start `/implement`.
  Check that the scheduler keeps two sandboxes active. When a card enters review,
  check that a bounce reaches the same reviewer, and that a new card does not
  start while the bounced card waits.

- Fill a thread with only `ready` cards, then start `/implement`. Check that the
  skill reports the state and stops, and that it accepts nothing.

## References

- [PDD001 Project Board Labels and Icebox](PDD001%20Project%20Board%20Labels%20and%20Icebox.md)
  — the label, epic, icebox, and host-only rules these skills build on.
- [PDD template and guidelines](README.md) — the six sections `design` writes to.
- [The `project` skill](../../plugins/katsuobushi/skills/project/SKILL.md) — the
  card model, the types, and the lifecycle `plan` and `implement` reference.
- [The `project-orchestration` skill](../../plugins/katsuobushi/skills/project-orchestration/SKILL.md)
  — the roles, `sandbox dispatch`, and the report bridge `implement` drives.
- [The `sandbox` skill](../../plugins/katsuobushi/skills/sandbox/SKILL.md) — the
  VM commands and the landing workflow `implement` drives.
- The `grilling` skill (mattpocock-skills plugin) — the one-question interview
  `design` runs to settle a design.
- The `simple-english` skill — the ASD-STE100 rules `design` writes to, and the
  source of this document's voice.
