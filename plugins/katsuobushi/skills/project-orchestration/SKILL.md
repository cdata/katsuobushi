---
name: project-orchestration
description:
  Orchestrate multi-agent work against a Katsuobushi `project` board — the
  implementor / peer-reviewer / human-owner roles, and how to delegate cards to
  sandbox VMs and review completed work in sandboxes. Use this skill when the
  user wants to drive a board forward with agents — dispatch a card to a
  sandbox, run a swarm across the backlog, peer-review a card in an isolated VM,
  move work through needs-review/ready/accepted, or wire up the dispatch report
  bridge. Complements the `project` skill (board mechanics) and the `sandbox`
  skill (driving VMs).
---

# Orchestrating a project board with agents

The `project` skill covers card/board mechanics. This skill is the
**choreography**: who does what, and how to use sandboxes to delegate
implementation and review. It leans on the `sandbox` skill for the actual VM
driving.

## The four roles

A card flows through the lifecycle by passing between three distinct roles, kept
in motion by a fourth — the orchestrator. **Keep the first three separate** —
that separation is the whole point of the `needs-review` state.

| Role              | Moves                                                       | Who                                                       |
| ----------------- | ----------------------------------------------------------- | --------------------------------------------------------- |
| **Orchestrator**  | dispatches, lands & routes cards between the other three    | the **host** agent (not sandboxed) — normally you         |
| **Implementor**   | `todo → in-progress → needs-review`                         | a dispatched sandbox agent (or you, if the owner asks)    |
| **Peer reviewer** | `needs-review → ready` (accept) or `→ in-progress` (bounce) | a **different** agent — ideally an independent sandbox VM |
| **Product owner** | `ready → accepted`                                          | **a human, always**                                       |

**Never review your own work.** If you implemented a card, you are the wrong
party to move it out of `needs-review` — spawn an independent reviewer (below).
And **never** move a card to `accepted` yourself: take it to `ready` and stop.

Because a blocker only clears its dependents at `ready` (not at `needs-review`),
**review is load-bearing for throughput** — an unreviewed card stalls everything
that depends on it. Treat "there is a card in `needs-review`" as the
highest-priority signal on the board: it's often blocking more work than
whatever is `available`. Prioritize reviewing over starting new work.

## The orchestrator (you, the host agent)

The orchestrator — normally the **host** agent, the one _not_ running inside a
sandbox — is the coordinating role that keeps the other three in motion.
Typically you rarely implement cards yourself; your job is to keep the board
flowing:

- **Pump the backlog** — dispatch Available cards to implementor agents and keep
  work moving as cards clear.
- **Bound concurrency** — fan work out in parallel only as wide as you can
  actually integrate _and_ as wide as the host's cores allow (each VM is ~4
  vCPU; budget half the box by default, and ask the owner at session start — see
  "Swarming"). Too many in-flight branches is merge thrash, not throughput.
- **Wrangle merges** — land each returned branch, and when parallel work
  collides, drive the **conflict reconciliation** (delegate it to a fresh
  sandbox like any other work — see the `sandbox` skill).
- **Route work between parties** — make sure every _delivered_ card gets an
  independent **reviewer**, and every _reviewed_ card reaches the **next**
  party: a bounced card back to an implementor, a `ready` card to the human
  owner.

Nothing here is hardcoded — the orchestrator _is_ the control loop.

## Peer review in a sandbox (independent reviewer)

The cleanest way to get an implementor≠reviewer split is to run the reviewer as
its own sandbox VM — a fresh agent, its own context, that can build and test the
work itself. This is a **read-only** delegation (it changes nothing), so the
deliverable is the agent's `report`, not a branch.

```sh
sandbox start --agent --name review-<slug> --prompt "<review directive>"
```

A good review directive:

- States the reviewer is **independent** and must **not change code / commit**.
- Names what the work is and its design contract (point at `design/…` if any).
- Lists what to review: correctness on the fragile paths, test quality
  (meaningful vs. rubber-stamp), failure modes, and what should **block**.
- Requires **empirical** verification — run the build/tests/clippy, don't just
  read: "start the test build once in the background and wait; first build cold-
  compiles and takes minutes."
- Ends:
  `report done "VERDICT: accept | needs-changes + strongest findings (file:line) + test-quality assessment + would-you-block"`.

The reviewer seeds from your **current working tree** (the sandbox uses
`git stash create`), so it sees uncommitted WIP — you do **not** need to commit
to get it reviewed. When it reports:

- **accept** → move the card `needs-review → ready` (you're executing the
  reviewer's decision), file any non-blocking follow-ups as their own cards, and
  hand the `ready → accepted` step to the human.
- **needs-changes** → append the findings to the card's `## Review notes`, move
  it `→ in-progress`, address them, and re-review.

Then remove the spent reviewer VM: `sandbox stop --remove review-<slug>`.

## Implement in a sandbox by default

Per-issue implementation should happen **in a sandbox** — one VM per card
(`sandbox dispatch`, below) — **not** in the host working tree, unless the
**product owner explicitly asks** for a card to be done directly on the host.
The sandbox boundary is what makes implementor≠reviewer real and keeps risky
work isolated; make it the default, not the exception.

**When a project can't be sandboxed.** Some projects aren't feasible to build or
run inside the sandbox for technical reasons — a dependency the VM can't reach,
a device / GPU / network need the guest can't satisfy, a toolchain that won't
install offline. That is **not** a cue to silently fall back to host-side
implementation. Bring it to the **product owner** and negotiate a path forward
together: widen the sandbox (its `allowedOrigins` / `packages` / `graphics`),
adjust the project, or agree to work a given card on the host. Keep the tradeoff
an explicit, shared decision rather than one you make unilaterally.

**When the sandbox isn't available at all (non-Linux).** The sandbox is
Linux-only, so on macOS — or any host where the `sandbox` commands and
`sandbox dispatch` don't exist — you can't delegate to a VM. Don't collapse to
doing everything inline; fall back to your own **subagent** faculties (the Agent
tool) to fill the same roles. Spawn a subagent to implement a card, and —
keeping **implementor ≠ reviewer** — a _separate_ subagent to review it. You
lose the sandbox's isolation and network bounding (a subagent runs with your own
privileges in the same tree), but the orchestration shape is unchanged: claim
the card, delegate the implementation, land the result, delegate an
_independent_ review, then take it to `ready`. The reason to prefer a subagent
over inline work is the same one that motivates the sandbox — keep the reviewer
independent of the implementor.

**Concurrency here is strictly 1 — never fan subagents out in parallel.** Unlike
sandbox VMs, which are isolated instances each with their own branch, subagents
all act on the **same** working tree, so two implementing at once would clobber
each other's edits. This is the opposite of the sandbox swarm's parallel fan-out
(bounded by cores): serialize completely — one card implemented, landed, and
reviewed before the next one starts. The host-core concurrency budget under
"Swarming" applies only to sandbox VMs, not to this fallback.

## Delegating implementation with `sandbox dispatch`

`sandbox dispatch <card-id>` is the implementor-in-a-VM path. It:

1. **Guards** — refuses a card that isn't Available (To-do with blockers
   cleared) unless `--force`.
2. **Claims** — moves it `todo → in-progress`.
3. **Composes** the directive: the card's title+body, prefixed with the optional
   `<board-dir>/.dispatch-instructions.md` (put your project's build/test/VCS
   rules there — see below).
4. **Launches** an agent VM `card-<id>` seeded with that directive.

```sh
sandbox dispatch a3f7b2                 # dispatch an Available card
sandbox dispatch a3f7b2 --force         # dispatch a blocked / non-todo card anyway
```

Write a **`.dispatch-instructions.md`** in the board dir with the project's
conventions (how to build/test in the sandbox, the acceptance gate, one-command-
per-Bash, commit/push discipline, `report done`/`report blocked`). Dispatch
prepends it to every card so the agent doesn't have to rediscover them. Generic
sandbox working-rules already come from the guest + the `sandbox` skill — don't
restate them.

### The report bridge (orchestrator-driven)

When a dispatched agent reports, advance the card. This is **not** hardcoded
into `dispatch` — you (the orchestrator) drive it, per the `sandbox` skill's
collect- and-integrate flow:

- **`done`** → `sandbox fetch card-<id>`, land the branch (rebase workflow from
  the `sandbox` skill), then `project status set <id> needs-review`. Now review
  it (a sandbox reviewer, above) before it reaches `ready`. **Check that work
  actually landed:** `sandbox fetch` compares the fetched branch tip to the seed
  it launched from and warns (human: a `WARNING: no committed work landed` line;
  `--json`: `"landed": false`) when they match — i.e. the agent ended its turn
  without committing. Treat that as a non-`done`: inspect with `sandbox attach`,
  reset the card to `todo`, and re-dispatch a fresh instance rather than
  advancing an empty branch to review.
- **`blocked`** → append the agent's report to the card's `## Dispatch log`
  section, `project status set <id> todo`, resolve what it needs, and
  re-dispatch a **fresh** instance (so its clone sees current HEAD).

How you _watch_ for the report is your choice: run `dispatch` in the foreground
and act when it returns, background it and act on the completion notification,
or fan several out and poll `sandbox status`. Prefer the event-driven paths
(foreground or backgrounded) over polling — `dispatch`/`prompt` block until the
guest posts a terminal report, so a backgrounded run re-invokes you exactly when
`done`/`blocked` lands, with no timers.

Add **`--until-report`** (`sandbox dispatch <id> --until-report`, or
`sandbox prompt <name> "…" --until-report`) when you want that guarantee to
survive an agent that ends its turn without reporting: instead of returning with
a "stopped without reporting" warning, the command stays armed and keeps waiting
for a real `done`/`blocked`. It pairs with the guest's **auto-nudge** — a
sandbox that ends a turn silently is automatically re-prompted a few times to
report — so a backgrounded build that finishes long after the turn ended is
still caught live. Don't substitute arbitrary `sleep`s for a completion event;
use these instead.

## Swarming the backlog

To burn down several Available cards at once, dispatch one per card as a batch
(each gets its own `card-<id>` VM and branch), then **land serially** in the
orchestrator as each reports `done`, so the working tip advances and the next
rebases onto it. Scope dispatched cards to **disjoint files** where you can, so
most landings stay fast-forwards. Keep review in the loop: each landed card
still goes `needs-review → (sandbox review) → ready` before a human accepts. See
the `sandbox` skill's "Parallel fan-out" for the mechanics.

### Bound concurrency to the host's cores

Each sandbox is a **real VM with its own vCPUs** (the `sandbox` lib defaults to
**4 vCPU per VM**), so a wide fan-out oversubscribes the host and grinds
everything — including your own orchestrator loop — to a crawl. Size the batch
to the hardware, not to the number of Available cards:

- **Budget half the box by default.** Let sandboxes claim at most **half the
  host's logical cores**, unless the product owner says otherwise. Roughly
  `max concurrent VMs ≈ (system cores ÷ 2) ÷ vCPU-per-VM` — so an 8-core host
  with default 4-vCPU VMs runs **one** at a time; 16 cores → **two**; 32 →
  **four**. (Read the host's core count, e.g. `nproc`, rather than guessing.)
- **Ask at the start of a session.** When the work is just getting going, prompt
  the owner for the share of system resources to devote to concurrency (e.g.
  "half", "all but two cores") _before_ you fan out, and carry that budget for
  the rest of the session. Don't guess and swamp the machine.

## Gotchas learned in practice

- **Re-stage the board after CLI mutations.** `project` commands edit the
  working tree; the `project-lint` flake check (and anything reading the flake
  source) sees only **git-tracked** files. After moving cards / `new`, `git add`
  the board dir, or the check reports phantom orphans (a `BOARD.md` referencing
  an untracked new card note).
- **You don't need to commit to dispatch or review.** Sandboxes seed from your
  working tree via `git stash create`, so WIP is included. (But do re-stage for
  the _flake check_, which is separate.)
- **A dispatch that fails after the claim leaves the card `in-progress`.** If a
  launch dies mid-way, the card was already claimed; reset it
  (`project status set <id> todo`) before re-dispatching, or use `--force`.
- **Reviewer ≠ implementor is a hard rule, not a nicety.** When you both build
  and review, you rubber-stamp your own blind spots. The sandbox boundary makes
  the separation real and cheap.
- **Trust the branch, not "the VM ran."** A dispatched agent can end its turn
  **unreported** (`sandbox status` shows `ended-unreported`) having done the
  work in-VM but never committing/pushing — `sandbox fetch` then shows only the
  `git stash` seed commits (`WIP on …` / `index on …`), i.e. nothing landed. The
  guest auto-nudges an unreported stop a few times first, so `ended-unreported`
  means the agent ignored those re-prompts too — a strong signal of a real
  stall, not just a missed report. Always fetch and inspect the branch for a
  **real** commit before advancing the card. To recover, `sandbox prompt` the
  instance to commit → push → report; if it stalls again, it's stuck — remove
  it, note the attempt in the card's `## Dispatch log`, reset the card to
  `todo`, and either re-dispatch a **fresh** instance or do the work directly. A
  dispatch launching cleanly does **not** guarantee a delivered branch.
