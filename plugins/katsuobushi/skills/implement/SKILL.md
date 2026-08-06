---
name: implement
description:
  Drive a swarm of sandbox VMs to build one label thread of a Katsuobushi
  project board to `ready`. Use this skill when the user wants to run
  `/implement`, build an epic or `PDDNNN` label thread end-to-end with a sandbox
  swarm, negotiate a concurrency budget and schedule per-card implementor and
  reviewer VMs, or take a thread of filed cards to `ready`. It never accepts a
  card — a person does that. Load the `sandbox` and `project-orchestration`
  skills first.
---

# Drive a swarm to `ready` (`implement`)

`implement` is the last skill of three. A person accepts a design from the
**`design`** skill, files its cards with the **`plan`** skill, and runs
`implement` to build them. This skill takes filed cards from To-do to `ready`,
where a person accepts them.

> **Load the `sandbox` and `project-orchestration` skills before you drive any
> VM.** `implement` is a **driver**, not a new engine. It runs
> `sandbox dispatch`, `sandbox fetch`, `sandbox prompt`, and the report bridge
> that those skills define. It adds a scheduler on top. It does not redefine the
> VM or the roles.

## When to use this

Run `implement` when a thread of cards is filed and a person wants them built by
sandboxes. If the cards are not filed yet, that is the `plan` skill. If the
design is not settled, that is the `design` skill.

## A work thread is a label

A **work thread** is the set of cards under one label, for example `PDD007`. A
thread can also need a card's **blocker** that carries a different label. Before
`implement` pulls an out-of-label blocker into scope, it asks the person. An
out-of-label blocker never enters a session on its own.

`implement` finds its thread with the `project` skill's label filter:

```sh
project status --available --label=<thread>
```

## Each session opens with a negotiation

The person and the skill agree on three things before any VM starts:

- **the concurrency budget** — how many sandboxes run at once, and how many
  vCPUs each one gets,
- **the roles** — how many implementors and how many reviewers,
- **the threads** — which labels the session works.

CAUTION: The budget's ceiling is the worst case, when every sandbox builds at
once. The builds together must not oversubscribe the physical cores. For
example, a 16-core, 32-thread machine can sustain four sandboxes at eight vCPUs
each. All four can build at once and still fit the cores. Read the vCPU count
from the project's flake — do not assume the 4-vCPU default. The per-sandbox
vCPU count is a `sandbox start` flag from the `sandbox` skill. `implement`
passes the negotiated value through.

## Each card gets its own pair of VMs

An **implementor** VM does the work. A **reviewer** VM reads it. Both stay warm
across the card's review loop — each bounce a reviewer sends the card back for
changes. The build cache and the context survive each bounce. When the card
reaches `ready`, `implement` stops both VMs. The next card gets a fresh pair.

## Parallelism counts active sandboxes, not cards

A paused VM spends no CPU. So many per-card VMs can wait while the budget's
worth of VMs run. The scheduler cycles the active slot among the waiting VMs.
The goal is a saturated CPU. The number of cards in flight can be more than the
active-sandbox budget.

## The scheduler fills a free slot in a fixed order

The scheduler prefers older work over new work, so few cards stay open at once:

1. Wake a VM for a card already in the review loop. A bounced implementor
   revises. A reviewer reads a pushed revision.
2. If no in-flight card needs the slot, start the next available card.

This bounds the work in progress. Older cards reach `ready` before the swarm
starts more new work.

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
    if a bounced implementor waits:              return its card
    if a reviewer has a pushed revision to read: return its card
    # 2) only then, a new card
    return next_available(thread)        # by priority, blockers cleared
```

## A revision reaches the same reviewer through the mirror

A reviewer reads work over the `report` channel. It does not write a branch. On
a bounce, the implementor revises the branch. `implement` pushes the revised
branch to the reviewer's `sync.git` — the per-instance git mirror the VM reads.
Then the same reviewer VM reads the new commits. `implement` never starts a
fresh reviewer for a bounce.

```
 implementor VM ─commit─▶ sandbox/<card> ─push─▶ the card's sync.git
                                                      │ fetch
                                                      ▼
                                                reviewer VM ─verdict─▶ host
      ▲                                                                 │
      └────────── bounce: host pushes the revised branch ◀──────────────┘
```

## `implement` stops at `ready`

`implement` drives a card `in-progress → needs-review → ready`. It never moves a
card to `accepted`. It never runs `project status set <id> accepted`. Only a
person accepts. When the thread has no available work left, `implement` reports
the state and stops. It does not idle while `ready` cards wait for the person.

## Two gates are human

- A person accepts the PDD before `plan` runs.
- A person accepts a `ready` card before it is done.

Do not automate either gate.

## References

- The **`project-orchestration`** skill —
  `plugins/katsuobushi/skills/project-orchestration/SKILL.md`. The orchestrator,
  the implementor, the reviewer, the product owner, `sandbox dispatch`, and the
  report bridge this skill drives.
- The **`sandbox`** skill — `plugins/katsuobushi/skills/sandbox/SKILL.md`. The
  `sandbox start / prompt / status / fetch / stop` verbs and the landing
  workflow.
- The **`project`** skill — the `--available --label` filter that selects a
  thread, and the six-state lifecycle.
- The **`plan`** skill — files the cards this skill builds.
