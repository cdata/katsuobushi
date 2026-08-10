---
name: project-orchestration
description:
  Orchestrate multi-agent work against a Katsuobushi `project` board — the
  implementor / peer-reviewer / human-owner roles, and how to delegate cards to
  sandbox VMs and review completed work in sandboxes. Use this skill when the
  user wants to drive a board forward with agents — dispatch a card to a
  sandbox, run a swarm across the backlog, peer-review a card in an isolated VM,
  move work through needs-review/ready/accepted, or wire up the dispatch report
  bridge. Complements the `project` skill (board mechanics); load the `sandbox`
  skill first — this skill does not cover VM driving, launch diagnostics, or
  recovery.
---

# Orchestrating a project board with agents

> **Before you dispatch or drive any sandbox, load the `sandbox` skill.** This
> skill covers the **choreography** only — who does what, and how work routes
> between them. Launch diagnostics, liveness semantics, and recovery live in the
> `sandbox` skill, and dispatching without them has caused real incidents: an
> operator who skipped it read a benign progress notice as a hang, killed a live
> provisioning step, and orphaned the VM it was building.

The `project` skill covers card/board mechanics. This skill is the
**choreography**: who does what, and how to use sandboxes to delegate
implementation and review. Every step below that touches a VM assumes you have
loaded the `sandbox` skill as instructed above.

**IMPORTANT: If you loaded only this skill...**

...then load the /sandbox and /project skills!

## The four roles

A card flows through the lifecycle by passing between three distinct roles, kept
in motion by a fourth — the orchestrator. **Keep the first three separate** —
that separation is the whole point of the `needs-review` state.

| Role              | Moves                                                       | Who                                                                       |
| ----------------- | ----------------------------------------------------------- | ------------------------------------------------------------------------- |
| **Orchestrator**  | dispatches, lands & routes cards between the other three    | an agent not inside a per-card sandbox — currently the host; normally you |
| **Implementor**   | `todo → in-progress → needs-review`                         | a dispatched sandbox agent (or you, if the owner asks)                    |
| **Peer reviewer** | `needs-review → ready` (accept) or `→ in-progress` (bounce) | a **different** agent — ideally an independent sandbox VM                 |
| **Product owner** | `ready → accepted`                                          | **a human, always**                                                       |

**Never review your own work.** If you implemented a card, you are the wrong
party to move it out of `needs-review` — spawn an independent reviewer (below).
And **never** move a card to `accepted` yourself: take it to `ready` and stop.

Because a blocker only clears its dependents at `ready` (not at `needs-review`),
**review is load-bearing for throughput** — an unreviewed card stalls everything
that depends on it. Treat "there is a card in `needs-review`" as the
highest-priority signal on the board: it's often blocking more work than
whatever is `available`. Prioritize reviewing over starting new work.

## The orchestrator (you)

The orchestrator — the role not itself inside a per-card sandbox (currently the
host) — is the coordinating role that keeps the other three in motion. Typically
you rarely implement cards yourself; your job is to keep the board flowing:

- **Pump the backlog** — dispatch Available cards to implementor agents and keep
  work moving as cards clear.
- **Bound concurrency** — fan work out in parallel only as wide as you can
  actually integrate _and_ as wide as the host's cores allow. Read the VM's size
  out of **this project's flake** rather than assuming the 4-vCPU default;
  budget half the box, and ask the owner at session start — see "Swarming". Too
  many in-flight branches is merge thrash, not throughput.
- **Wrangle merges** — land each returned branch, and when parallel work
  collides, drive the **conflict reconciliation** (delegate it to a fresh
  sandbox like any other work — see the `sandbox` skill).
- **Route work between parties** — make sure every _delivered_ card gets an
  independent **reviewer**, and every _reviewed_ card reaches the **next**
  party: a bounced card back to an implementor, a `ready` card to the human
  owner.

Nothing here is hardcoded — the orchestrator _is_ the control loop.

### The board is orchestrator-only (an invariant)

The orchestrator is the **only** writer of `project/kanban/` — both `BOARD.md`
and the `<id>.md` card notes. A sandbox guest never writes the board. It gets
its card as prose in its directive, returns its code as a git branch, and
returns its findings through the `report` channel; the orchestrator reads those
and makes every board change. This is the structural cure for the card-note
write conflict: with one writer, two writers cannot collide.

So **never tell an agent to write a finding into a card note.** A reviewer's
verdict and an implementor's summary both come back over `report`, and the
orchestrator writes them down (a bounce's findings go into the card's
`## Review notes` by the orchestrator, below). The past board corruption came
from a directive that asked an agent to record its own findings in the note — a
second writer by hand.

## Peer review in a sandbox (independent reviewer)

The cleanest way to get an implementor≠reviewer split is to run the reviewer as
its own sandbox VM — a fresh agent, its own context, that can build and test the
work itself. This is a **read-only** delegation (it changes nothing), so the
deliverable is the agent's `report`, not a branch.

The launch, liveness, and recovery mechanics below come from the `sandbox` skill
— load it before running any of these commands.

```sh
sandbox start --agent --name review-<card-id> --prompt "<review directive>"
```

The `--name` argument **must use the card-id as the slug** (e.g.
`review-a3f7b2`). `sandbox prune` maps instance names back to cards by this
exact convention — `review-<card-id>` — so it can reap the state dir and ref
when the card is accepted or cancelled. Any name that does not follow the
pattern is **off-contract**: `prune` cannot map it to a card and will silently
skip it. Off-contract instances accumulate forever and must be removed by hand:

```sh
sandbox stop --remove <off-contract-name>
```

To spot them, run `sandbox status` and look for names that don't match
`review-<card-id>` or `card-<card-id>` for any active card on the board.

Once launched, `sandbox` prints the full suffixed instance name — for example
`review-a3f7b2-bb7f4e11` or `card-a3f7b2-bb7f4e11`. Every command that addresses
a running instance — `stop`, `prompt`, `fetch`, `deliver` — takes that suffixed
name; `--name` at launch is the only place the bare slug appears.

A good review directive:

- States the reviewer is **independent** and must **not change code / commit**.
- Names what the work is and its design contract (point at `design/…` if any).
- Lists what to examine: correctness on the fragile paths, test quality
  (meaningful vs. rubber-stamp), and failure modes. Ask questions — "trace what
  spawns this payload" — rather than asserting findings — "nothing spawns this
  payload — confirm". Name areas of uncertainty, not conclusions.
- Says plainly that the reviewer may accept, and that an accept is not a failure
  of the review.
- Requires **empirical** verification — run the build/tests/clippy, don't just
  read. Point the reviewer at the **project's menu** for how: "list the
  project's commands with `nix develop -c menu` and use its own test/lint
  commands — a sandbox starts in a plain login shell, so `menu`/`showMenu` are
  not on PATH until you go through `nix develop`. Reach for raw
  `cargo`/`nix build` only if the menu has nothing for what you need, and say so
  in your report if you do."
- Ends:
  `report done "VERDICT: accept | needs-changes + strongest findings (file:line) + test-quality assessment + would-you-block"`.

### Writing a directive that does not lead

The whole value of peer review is that a second party reaches its own
conclusion. The orchestrator is the party best placed to compromise that
independence — and the most likely to do it without meaning to. A directive that
names the verdict you expect, expresses a preference for a bounce, or centres
the orchestrator's own conclusion about the current card turns the reviewer from
a check into an echo.

**Rules:**

- **Name what to examine; do not name the verdict you expect.** "Trace what
  spawns this payload" is a question that leaves room for any answer. "Nothing
  spawns this payload — confirm" is an assertion that the reviewer is being
  asked to ratify.
- **Do not say which way you would rather it went.** "I would rather bounce this
  card than let a criterion quietly go unsatisfied twice" is orchestrator
  preference dressed as review guidance. Leave the verdict unframed.
- **Relaying a prior reviewer's finding is legitimate; relaying your own
  conclusion is not.** If a previous round of review on this card surfaced a
  finding, you may pass it on — you are quoting what another party concluded. If
  the finding is your own assessment of the current card, put it as one question
  among several rather than making it the centrepiece.
- **Say plainly that an accept is valid.** If the directive's only named outcome
  is a block, it signals what the reviewer is expected to find. A neutral
  directive leaves the accept as equally reachable.
- **When you have a real suspicion, embed it as a question among several — not
  as the frame the whole review hangs on.** One question in a list of five is a
  cue to check carefully; a single-question directive that opens with "I believe
  X" is leading by construction.

**Worked example.**

Leading directive (avoid):

> Review the branch for card 457e37. The implementation claims `Capturable` is
> satisfied but I believe nothing actually captures — trace the call chain and
> confirm. Recommend BLOCK if you conclude a criterion is unmet in substance. I
> would rather bounce this card than let a criterion quietly go unsatisfied
> twice.

What goes wrong: the orchestrator has asserted its conclusion ("I believe
nothing actually captures"), pre-set the preferred outcome ("I would rather
bounce"), and asked the reviewer to confirm rather than investigate. The
reviewer's independent judgement is crowded out before the first line of code is
read.

Neutral rewrite:

> Review the branch for card 457e37. The card's acceptance criterion is
> `Capturable: the implementation captures X`. Please:
>
> 1. Confirm the test suite passes and clippy is clean.
> 2. Trace the call chain from the public entry points: where, if anywhere, is X
>    captured?
> 3. Assess whether each acceptance criterion is met in substance.
> 4. Note any test cases you think are missing or rubber-stamp.
>
> Report `VERDICT: accept | needs-changes` with findings and file:line
> references.

What is right: the reviewer is given a list of things to examine — including the
question about capture — and left to reach any verdict. An accept is as valid as
a block.

### Delivering the branch to the reviewer

A guest can see only its own host directory, so no instance can read another's
mirror: the orchestrator is the only path between two parties. The reviewer's
clone therefore does **not** automatically contain the implementor's branch.

Before sending the reviewer its directive, fetch the implementor's branch into
the host and deliver it into the reviewer's mirror:

```sh
# After sandbox fetch card-<id-instance> brings the branch into the host:
sandbox deliver review-<card-id-instance> --branch sandbox-guest/<card-id-instance>
```

`review-<card-id-instance>` is the **full suffixed name** that `sandbox start`
printed when the reviewer launched — for example `review-fdc720-bb7f4e11`, not
the bare slug `review-fdc720` passed to `--name`. Record it on the card when you
launch; see "Keep the pair warm" below.

Inside the reviewer's clone the branch lands as `delivered/<card-id-instance>`
(the `delivered/` prefix never collides with the reviewer's own `sandbox/<inst>`
working branch). The reviewer reads it with:

```sh
git fetch origin
git log origin/delivered/<card-id-instance>
```

Include the branch name and current tip SHA in the review directive:
`"The branch is origin/delivered/<card-id-instance>; tip is <sha>."` The
reviewer uses that SHA to answer "what changed since I last read this" after a
bounce.

**Review rounds add commits; the branch is never rewritten.** The implementor
pushes follow-up work on top of its existing commits — no rebase, no force-push.
After a re-delivery, the reviewer reads only the new commits:

```sh
git log <prev-tip>..origin/delivered/<card-id-instance>
```

where `<prev-tip>` is the SHA recorded at the start of the previous round.

**Re-review after a bounce.** Once the implementor pushes new commits, the
orchestrator fetches and re-delivers before resuming the reviewer:

```sh
sandbox fetch card-<id-instance>
sandbox deliver review-<card-id-instance> --branch sandbox-guest/<card-id-instance>
```

Re-delivery is force-idempotent: `delivered/<card-id-instance>` in the
reviewer's mirror simply advances to the new tip. The reviewer runs
`git fetch origin` and reads the new range using the SHA from the prior round.

When it reports:

- **accept** → land the branch (the sandbox skill's integration procedure), then
  `project status set <id> ready`. File any non-blocking follow-ups as their own
  cards and hand the `ready → accepted` step to the human. Keep the
  `sandbox-guest/<impl-instance>` remote bookmark as the revert artifact: a
  product owner who returns the `ready` card reverts against it. Delete the
  bookmark once the card reaches `accepted`.
- **needs-changes** → append the findings to the card's `## Review notes`, move
  it `→ in-progress`, and send them to the card's **paused implementor VM** (see
  "Keep the pair warm", below) rather than fixing them yourself or dispatching a
  cold instance. Then re-review with the same reviewer.

Only once the card reaches `ready` is the reviewer spent — then remove it:
`sandbox stop --remove review-<card-id-instance>`. Until then **pause** it
(`sandbox stop review-<card-id-instance>`, no `--remove`) so the re-review
starts warm; see "When to replace the reviewer instead of pausing" for the
exception.

### Landing a branch

When the reviewer accepts, the orchestrator lands the branch. A branch is
reviewed against the **tip it was seeded from** — but integration happens onto
the **current tip**, which may have advanced. Another card can land between
review and integration, so the reviewer never saw that interaction.

The orchestrator uses its judgement to reconcile what does not apply cleanly. In
most cases a reconciliation is mechanical — realigning a patch to changed
context — and needs no second review: the reviewer agreed to the intent, and the
orchestrator aligns it to the new baseline.

**When a reconciliation changes what the reviewer agreed to** — altering the
logic, the design, or an invariant the reviewer checked — the card returns to
`needs-review`. That case is rare and is handled one card at a time.

**Write the record before the card moves to `ready`** — but only for files the
guest's branch actually changed. A conflict in a file the guest never touched
(such as `BOARD.md`, which every guest branch carries stale from dispatch) is
resolved mechanically and needs no record. When a branch merges cleanly onto the
moved tip with no hand-chosen resolution, nothing was reconciled — no record is
required. (For how to delegate a real conflict to a fresh sandbox, see the
`sandbox` skill's **Conflict reconciliation** section.)

Where a record is required, write into the card's `## Review notes`:

- What did not apply, and what the orchestrator chose in its place.
- Whether the card returned to review, and why (or that it did not and why not).

A `ready` card with a silent reconciliation in its own files is one whose final
state the reviewer never saw. The record is how the product owner knows what
they are accepting.

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
the card, delegate the implementation, delegate an _independent_ review, land
the result, then take it to `ready`. The reason to prefer a subagent over inline
work is the same one that motivates the sandbox — keep the reviewer independent
of the implementor.

**Concurrency here is strictly 1 — never fan subagents out in parallel.** Unlike
sandbox VMs, which are isolated instances each with their own branch, subagents
all act on the **same** working tree, so two implementing at once would clobber
each other's edits. This is the opposite of the sandbox swarm's parallel fan-out
(bounded by cores): serialize completely — one card implemented, reviewed, and
landed before the next one starts. The host-core concurrency budget under
"Swarming" applies only to sandbox VMs, not to this fallback.

## Delegating implementation with `sandbox dispatch`

`sandbox dispatch <card-id>` is the implementor-in-a-VM path. It launches and
drives a real VM, so the `sandbox` skill is a **prerequisite** here, not a
reference — diagnosing a dispatch that looks stuck needs its `console.log`,
progress-notice, and stop-vs-remove semantics. It:

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

**If a dispatch dies after the VM comes up, don't recompose the directive.**
`dispatch` claims the card, composes the directive, and boots the VM detached
before delivering it — so a killed dispatch can leave a card marked
`in-progress` beside an idle VM that never got its instructions. The composed
text is persisted to `directive.md` in the instance's state dir; resend it with
`sandbox prompt card-<id-instance> --redeliver` rather than rebuilding it from
the card plus the instructions file. (If the VM never came up at all, reset the
card to `todo` and re-dispatch instead.)

Write a **`.dispatch-instructions.md`** in the board dir with the project's
conventions: the acceptance gate, one-command-per-Bash, commit/push discipline,
`report done`/`report blocked`. Dispatch prepends it to every card so the agent
doesn't have to rediscover them. Generic sandbox working-rules already come from
the guest + the `sandbox` skill — don't restate them.

**Build and test commands do not belong there — they belong in the menu.** A
"how to build" section in a prose file is a copy that drifts from the commands
that actually exist, and it competes with the dev shell's own table. Put the
project's real build/test/run commands in the flake's menu, where they are
executable and discoverable (`nix develop -c menu` from anywhere, `showMenu`
inside the shell) — and where any artifact sharing the project has set up is
actually wired. Keep the instructions file for what the menu cannot express.

**A project whose menu has no build/test command has nothing to steer agents
to.** Check before you rely on this: if `nix develop -c menu` lists only
housekeeping (format, lint, board), every dispatched agent will legitimately
take the fallback branch, and the fix is to give the project a real menu rather
than to write build instructions into the prose file.

Then hold agents to it: a dispatched agent should consult the menu before
building, use the project's command when one exists, and fall back to raw
`cargo`/`nix build` only when the menu has nothing for the job — **reporting the
gap when it does**. The guest contract states this too, but a missing menu
command surfacing in a report is your signal to file it as a card rather than
let every future dispatch improvise past it.

### The report bridge (orchestrator-driven)

When a dispatched agent reports, advance the card. This is **not** hardcoded
into `dispatch` — you (the orchestrator) drive it, per the `sandbox` skill's
collect- and-integrate flow:

- **`done`** → `sandbox fetch card-<id-instance>`, check that work actually
  landed on the branch, then `project status set <id> needs-review`. **No
  landing** — landing is the step that moves the card to `ready`, not this one.
  Pause the VM (see "Keep the pair warm") and give the reviewer the branch.

  This ordering gives three properties: the owner's history holds only reviewed
  work (nothing lands before `ready`); a bounce costs no landing and no revert;
  and "has this landed" has an exact answer — the card status says so.

  **Check that work actually landed:** `sandbox fetch` compares the fetched
  branch tip to the seed it launched from and warns (human: a
  `WARNING: no committed work landed` line; `--json`: `"landed": false`) when
  they match — i.e. the agent ended its turn without committing. Treat that as a
  non-`done`: inspect with `sandbox attach`, reset the card to `todo`, and
  re-dispatch a fresh instance rather than advancing an empty branch to review.

- **`blocked`** → append the agent's report to the card's `## Dispatch log`
  section, `project status set <id> todo`, resolve what it needs, and
  re-dispatch a **fresh** instance (so its clone sees current HEAD).

How you _watch_ for the report is your choice: run `dispatch` in the foreground
and act when it returns, background it and act on the completion notification,
or fan several out and poll `sandbox status`. Prefer the event-driven paths
(foreground or backgrounded) over polling — `dispatch`/`prompt` block until the
guest posts a terminal report, so a backgrounded run re-invokes you exactly when
`done`/`blocked` lands, with no timers.

**`dispatch` stays armed by default — there is no flag to remember.** An agent
that ends its turn without reporting does _not_ end the drive: the command keeps
waiting for a real `done`/`blocked`, pairing with the guest's **auto-nudge** (a
sandbox that stops silently is automatically re-prompted a few times to report),
so a backgrounded build finishing long after the turn ended is still caught
live. That is what lets you read "the dispatch returned" as "the work
concluded". The same default applies to `sandbox start --agent --prompt …`,
which is dispatch-shaped rather than interactive.

Pass `--no-until-report` only when you deliberately want the drive to return on
the agent's first yield. (`--until-report` is still accepted and is now a
no-op.)

Interactive `sandbox prompt` is the exception — it still returns on an
unreported yield, because you are watching and can re-prompt. Its warning tells
you how to wait instead. Don't substitute arbitrary `sleep`s for a completion
event; use the armed drive.

### Keep the pair warm until the card is `ready`

A card in `needs-review` is **not** finished work — it is work mid-loop, and the
loop usually runs more than one turn. So **pause** both VMs of the pair rather
than discarding them:

```sh
sandbox stop card-<id-instance>          # pause the implementor — no --remove
sandbox stop review-<card-id-instance>   # pause the reviewer   — no --remove
```

**When to replace the reviewer instead of pausing.** Pausing is right when the
same reviewer can give a valid re-review — for example when the implementor made
targeted fixes and you want the same reader to re-check them. Replace the
reviewer when it should not be reused — for example when it has already rejected
an earlier attempt and you want an uncontaminated read of a substantially
revised implementation. In that case, stop and remove the existing instance,
then start a fresh one with the **same name**. Accept the cold rebuild for the
new reviewer — the warm caches of the removed instance are gone:

```sh
sandbox stop --remove review-<card-id-instance>   # discard the opinionated reviewer
sandbox start --agent --name review-<card-id> --prompt "<fresh review directive>"
```

Using the same name keeps the instance under the `prune`-reapable contract. A
different slug (for example `review-<card-id>-2`) would create an off-contract
instance that `prune` skips — see the naming note above.

`stop` on a named instance powers the VM off but keeps its state dir, branch,
and **scratch volume** — the cargo / rustup / nix caches and the built target
dir. A resumed instance therefore picks up where its build left off, while a
fresh dispatch cold-compiles the whole project first. That rebuild is the
dominant latency in a multi-turn review loop, and paying it on every bounce is
the mistake this section exists to prevent. `sandbox prompt` on a paused
instance **auto-starts it** (~30–60s to boot and arm), so resuming is just:

```sh
sandbox prompt card-<id-instance> "<the review findings, verbatim, + what to change>"
sandbox prompt review-<card-id-instance> "<re-review directive pointing at the new commits>"
```

Remove each VM only when its work is truly spent:

- **implementor `card-<id>`** — when the card reaches `ready` (review passed).
- **reviewer `review-<card-id>`** — same: it may be asked to re-review any
  number of times before then.
- **reviewer `review-<card-id>`, deliberately replaced** — when replacing it to
  get an uncontaminated read of a substantially revised implementation; see
  "When to replace the reviewer instead of pausing".
- **either one, early** — if the card is bounced to `todo`, cancelled, or the
  instance is stalled/unreported. A stalled VM is not warm, it's stuck; remove
  it and dispatch fresh.

Three properties of a resume to write your prompts around:

- **RAM is wiped, so the conversation is gone.** The resumed agent is a fresh
  session reading its branch — not a continuation. Make the prompt stand on its
  own: quote the review findings in full, point at the branch and the card, and
  never say "as we discussed".
- **The instance's mirror is frozen at its launch**, so it cannot see host
  commits landed since. Fixes come back as more commits on the old seed and you
  rebase them on landing, exactly as before. If the feedback genuinely requires
  the agent to build against work landed after its launch, that's the case for a
  **fresh** instance — take the cold build knowingly.
- **Record both full instance names on the card** (in `## Dispatch log`) as soon
  as you launch them. `--name` returns a suffixed name, and a later turn — or a
  later orchestrator — can only resume the pair if the names are on the card
  rather than in a context that has since compacted.

Paused VMs cost **disk, not cores or RAM** — they don't count against the
concurrency budget below. They do hold their store/scratch volumes, though, so
sweep the pairs of cards that reached `ready` instead of letting them
accumulate; `sandbox status` lists what is running vs. stopped.

## Swarming the backlog

To burn down several Available cards at once, dispatch one per card as a batch
(each gets its own `card-<id>` VM and branch). Several cards can be in flight
and reviewed concurrently, but **land serially**: when a reviewer accepts a
card, land its branch so the working tip advances and the next landing rebases
onto it. The serialisation point is **acceptance**, not the `done` report —
multiple cards can reach `needs-review` before any of them land. Scope
dispatched cards to **disjoint files** where you can, so most landings stay
fast-forwards. See the `sandbox` skill's "Parallel fan-out" for the mechanics.

### Bound concurrency to the host's resources

Each sandbox is a **real VM with its own vCPUs and its own RAM**, so a wide
fan-out oversubscribes the host and grinds everything — including your own
orchestrator loop — to a crawl. Size the batch to the hardware, not to the
number of Available cards:

- **Read the VM's size from the project's flake — don't assume the default.**
  The consumer sets `vcpu` and `mem` on its `lib.sandbox` call, so a project may
  be running 8- or 16-vCPU VMs, and one of those is worth two or four of the
  defaults. Grep the project's `flake.nix` (or wherever it calls `lib.sandbox` /
  `sandboxLib`) for `vcpu` and `mem` before you compute anything. Only when the
  call sets neither does the lib's default apply: **4 vCPU, 8192 MiB**. Nothing
  asserts a ceiling, so a graphics-enabled or build-heavy project has usually
  raised one or both — read, don't assume.
- **Budget half the box by default, on _both_ axes.** Let sandboxes claim at
  most **half the host's logical cores** and **half its RAM**, unless the
  product owner says otherwise. The batch size is whichever bound is tighter:

  ```
  max concurrent VMs ≈ min( (cores ÷ 2) ÷ vcpu , (RAM MiB ÷ 2) ÷ mem )
  ```

  Read the host's actual numbers (`nproc`, `free -m`) rather than guessing. With
  default 4-vCPU / 8 GiB VMs, a 16-core / 32 GiB host runs **two**; the same
  16-core host with a project configured at `vcpu = 8` runs **one**.

- **Ask at the start of a session.** When the work is just getting going, prompt
  the owner for the share of system resources to devote to concurrency (e.g.
  "half", "all but two cores") _before_ you fan out, and carry that budget for
  the rest of the session. Tell them the per-VM size you read from the flake and
  the VM count it implies — a bare "half the box" is not actionable to someone
  who doesn't have `vcpu` memorized. Don't guess and swamp the machine.

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
- **An orchestrator can compromise a reviewer's independence without meaning
  to.** A directive that names the expected verdict, says "I would rather
  bounce", or leads with the orchestrator's own conclusion about the current
  card turns the reviewer from a check into an echo. A reviewer that is
  independent in name only produces an accept/block record that reads cleaner
  than it is — and the orchestrator is the party best placed to prevent this.
  See "Writing a directive that does not lead".
- **Don't `--remove` a VM whose card is still in review.** The habit comes from
  the `sandbox` skill's landing procedure, which removes an instance once its
  work is accepted — but at `needs-review` it isn't. Removing it throws away the
  warm build caches, so every round of reviewer feedback pays a full cold
  compile before it can change a line. Pause instead; see "Keep the pair warm".
  Exception: deliberate reviewer replacement — see "When to replace the reviewer
  instead of pausing".
- **Trust the branch, not "the VM ran."** A dispatched agent can end its turn
  without committing or pushing — `sandbox status` shows `Idle` in the `WORK`
  column (no heartbeat beating, no terminal report filed), and `sandbox fetch`
  then shows only the `git stash` seed commits (`WIP on …` / `index on …`), i.e.
  nothing landed. The sandbox auto-nudges an idle agent; `sandbox status` shows
  the count as `Idle — nudged N/M`. Once the counter reads `Idle — nudged M/M`
  and the reading is still `Idle`, that is the decision boundary: the agent
  ignored every nudge and is stalled. Always fetch and inspect the branch for a
  **real** commit before advancing the card. To recover, `sandbox prompt` the
  instance to commit → push → report; if it stalls again, it's stuck — remove
  it, note the attempt in the card's `## Dispatch log`, reset the card to
  `todo`, and either re-dispatch a **fresh** instance or do the work directly. A
  dispatch launching cleanly does **not** guarantee a delivered branch.
