# PDD003 Sandbox Heartbeats and Work State

## Introduction

A sandbox agent often ends a turn while its work continues. Today the guest
calls this `ended-unreported`. The verdict lands about 91.5 seconds after the
turn ends. A cold build in this project takes 15 to 25 minutes. The verdict is
therefore wrong almost every time, and the word blames the agent for a state the
mechanism produced.

This document replaces the guess with an observation. A **heartbeat** is a small
file in the project. It holds a shell check that exits zero while some work
runs. The guest runs every heartbeat and combines the results into one **work
state**: `finished`, `active`, or `idle`. The guest wakes an idle agent the
moment its work stops. The host shows the state, and the operator reads one word
that means one thing.

The board model comes from
[PDD001](PDD001%20Project%20Board%20Labels%20and%20Icebox.md). The orchestration
roles come from [PDD002](PDD002%20Design,%20Plan%20and%20Implement%20Skills.md).
This document changes neither. It changes what a VM tells them.

## Goals

- **The guest observes work. It does not infer it.** A verdict about an agent
  comes from evidence on the machine, not from the shape of the turn record.

- **A project declares its own heartbeats.** Any class of work qualifies,
  including work that consumes no local resource.

- **Every project is covered on the first day.** A repo that has never heard of
  heartbeats still gets the protection.

- **No universal ceiling.** Each heartbeat declares its own timeout. The library
  never guesses how long a project's work takes.

- **The agent is woken when its work stops.** Work no longer completes into an
  empty room.

- **The state names describe the machine.** An operator reads a fact, not an
  accusation.

- **The agent contract stops prescribing how to work.** The agent decides how to
  run its own work. The host requires only a terminal report.

## Non-goals

- **Transport liveness.** The transport heartbeat answers whether the host and
  the guest can still reach each other. It stays hardcoded in its own domain. It
  is never a member of the drop-in set, and this document does not change it.

- **Agent discipline as a mechanism.** A heartbeat never depends on the agent to
  declare anything. An agent that says nothing gets the same protection as one
  that narrates.

- **A sandbox for heartbeat scripts.** A heartbeat runs with the rights of the
  agent user inside a VM that is already the blast-radius boundary. This
  document adds no second boundary inside it.

- **Automatic repair.** The guest wakes an idle agent. It never kills a wedged
  process, restarts a VM, or resets a card. A person decides those.

- **Proof that the work is right.** A work state says what the machine does. The
  "trust the branch, not the VM" rule in the `project-orchestration` skill still
  decides whether work landed.

- **The board and the roles.** PDD001 and PDD002 own those.

## Body

### Ground truth

Hard facts a fresh reading needs, checked against the tree at the time of
writing:

- **The guest resolves an unreported turn in 91.5 seconds.** `maxNudges = 3`,
  `nudgeIntervalMs = 30000`, and `stopGraceMs = 1500` (`lib/sandbox/default.nix`
  lines 265 to 267). The nudge loop lives in
  `rust/katsuobushi-sandbox-guest/src/bin/server.rs` lines 441 to 477.

- **A `working` report resets nothing.** In `server.rs` lines 322 to 380, a
  non-terminal report on a turn that already ended updates `lastActivityAt` and
  no other field. It does not clear the ended flag, and it does not return any
  nudge to the budget. An agent that answers all three nudges with
  `report working "still building"` still resolves as `ended-unreported`.

- **The agent cannot block in the foreground for a cold build.** One Bash call
  caps at 600 seconds unless `BASH_MAX_TIMEOUT_MS` raises it. Nothing in
  `lib/sandbox/default.nix` sets it, and the managed settings block at line 1175
  carries only `channelsEnabled`, `skipDangerousModePermissionPrompt`, and the
  hooks. A 15-minute build has no foreground path.

- **This repo runs a 300-second stall window, not 1500.** The library default is
  `progressStallSecs ? 300` (`lib/sandbox/default.nix:124`). This project's
  sandbox call (`flake.nix` lines 166 to 190) passes no override. The host
  therefore printed a stall notice about five minutes into every cold build. The
  library comment at lines 118 to 121 names this outcome and its cost.

- **The two sides of the contract disagree.** The agent contract tells the agent
  to run long commands in the foreground (`lib/sandbox/default.nix` lines 565 to
  570). The dispatch instructions tell it to start the cold build in the
  background (`project/kanban/.dispatch-instructions.md` lines 16 to 18).

- **The guest crate carries one serialization dependency.**
  `rust/katsuobushi-sandbox-guest/Cargo.toml:25` lists `serde_json` and no YAML
  parser.

- **The agent runs under one systemd unit.** `systemd.services.katsuobushi-agent`
  (`lib/sandbox/default.nix:1654`) holds the tmux session and the harness. A
  build that goes through `nix develop` runs its derivations under
  `nix-daemon.service` instead, so the work of one cold build appears in two
  cgroups.

### The fault: three windows that never agreed

Three parts of the system each held an opinion about the same silence. None of
them looked at the machine.

```
  agent's Bash tool   |=== 600s ===|                  hard cap on one call
  guest's nudges      |= 91.5s =|                     verdict: ended-unreported
  host's stall notice |========== 300s ==========|    notice: "no reports"
  a cold build        |================================= 15-25 min =========|
```

The build outlives all three. The guest reached its verdict first, about twenty
minutes before the answer was knowable. The word it wrote down was
`ended-unreported`, which reads as an agent that failed to report. The agent had
in fact done the one thing its tools allowed.

The damage compounds. An orchestrator that reads `ended-unreported` has two bad
options and no good one. It interrupts a healthy VM in the middle of a build, or
it waits for hours on a VM that will never speak. Both were observed. Three of
six instances in one swarm stalled this way, and the two harder faults appeared
only on instances that had first idled after a yield.

### A heartbeat is a file

_As the author of a project, I add one file to my repo. From then on the
sandbox knows when my build is running, and nobody has to guess._

A heartbeat lives in `.katsuobushi/heartbeats/` in the workspace repo. It is a
YAML file:

```yaml
# .katsuobushi/heartbeats/cargo-build.yaml
label: Compiling
timeout: 45m
interval: 10s
check: |
  pgrep -f 'rustc|cargo' >/dev/null
detail: |
  echo "$(ls target/debug/deps | wc -l) units"
```

The fields:

- **`label`** (required) — the words the host shows for this heartbeat.
- **`timeout`** (required) — how long this heartbeat can beat before the guest
  calls it late. This is the ceiling, and it belongs to the heartbeat that knows
  its own work.
- **`check`** (required) — a shell body. Exit zero means the heartbeat beats.
  Any other exit code means it does not.
- **`interval`** (optional) — how often the guest runs the check. It defaults to
  the transport cadence of 10 seconds. A cheap `pgrep` polls briskly. A remote
  check polls slowly and stays polite to someone else's service.
- **`detail`** (optional) — a second shell body. Its first line of output
  narrates the current beat. A heartbeat that cannot narrate omits this field
  and shows its label alone.

The rules that close the gaps:

- A heartbeat runs as the agent user, with the workspace as its working
  directory.
- The guest bounds each run by that heartbeat's interval. A check that outlives
  its interval is killed and does not beat.
- A heartbeat's **duration** counts from the first beat of an unbroken run. One
  failed check ends the run, and the next beat starts a new one. The guest
  compares the timeout against this duration. An author therefore writes a check
  that does not flicker. A check on a process group survives a single process
  exit. A check on one process identifier does not.
- A file the guest cannot read or parse never beats. The guest reports the error
  to the host once, so a broken heartbeat is visible and is not silent.
- A `detail` body that fails is ignored. The label alone shows.

The directory is version-controlled and reaches the VM through the ordinary git
seed. A person reviews every heartbeat, and a forged one appears in the diff. An
agent can write the file, but it cannot do so quietly.

#### Re-reading the heartbeat set

The guest re-reads the heartbeat directories on a slow timer (once per minute).
The rule is **once per error state**: a broken file's error is reported once and
suppressed on every subsequent scan while the file remains broken. A file that
is corrected, or a file that is newly added to a directory, is picked up on the
next scan without a guest restart.

This is the intended reading of "once" in the design. "Once ever" would leave a
corrected file invisible and give its author no signal explaining why. "Once per
interval" would fill the log. "Once per error state" is silent while a file
stays broken, loud when it first breaks, and self-healing when it is fixed.

A filesystem watcher is not used. Re-reading on a slow timer is sufficient: the
I/O cost of scanning a small directory of YAML files once per minute is
negligible, and the one-minute lag before a corrected file takes effect is
acceptable for a development-time authoring workflow.

### The library ships heartbeats of its own

A repo with no `.katsuobushi/heartbeats/` directory is still covered. The
library ships its own heartbeats inside the VM image, as ordinary files in the
same format. A project's directory adds to that set. It never replaces it.

The shipped set covers the local work that every observed stall actually was:

- **`agent-work`** — the `katsuobushi-agent` cgroup consumed CPU since the last
  beat.
- **`nix-build`** — the `nix-daemon` cgroup consumed CPU since the last beat.

Both matter, because one cold build through `nix develop` puts its work in both
places. Both are files a person can read inside the VM, so an operator who asks
why a VM is `active` gets an answer from a file and not from Rust.

The guest also carries one heartbeat that is not a file. It beats for exactly as
long as a turn is in flight. The turn-accepted hook arms it and the stop hook
silences it. This heartbeat lives in the guest because the guest is the one
party that knows turn state. It gives the model two properties for free. An
agent that reads files and thinks is `active`, not `idle`. A turn that never
ends beats past its timeout and becomes late, which names a wedged harness. Its
timeout is 60 minutes, because a single turn longer than an hour is a fault
worth saying out loud.

### The work state

The guest combines every heartbeat into one **work state**. The rule is a
logical OR over the set, read in one precedence order:

```
  finished   the agent ran a terminal report for this turn
     >
  active     one or more heartbeats beat
             (late)  a beating heartbeat is past its own timeout
     >
  idle       nothing beats
```

`finished` outranks every observation. The agent's own terminal report is
authoritative about the turn. An agent that reports `done` while a build still
runs reads as `finished`, and the leftover process is not allowed to hide
completed work. The risk this accepts is small and already guarded. The
`project-orchestration` skill tells an orchestrator to trust the branch and not
the VM.

`late` is not a fourth state. It is a flag on `active`. A heartbeat past its
timeout is by definition still beating, and one that stops is not late but
absent. The host shows the flagged state as `Active (Late)`.

`idle` is the floor. It means nothing runs. It carries no accusation, and this
is the point. The word `ended-unreported` leaves the vocabulary. An agent that
ends a turn having done nothing is `idle`, which is a fact about the machine
that an operator can act on.

`finished` belongs to one turn. A new prompt clears it, and the state returns to
whatever the heartbeats say. The grace window after a stop
(`stopGraceMs = 1500`) is unchanged. It still absorbs a terminal report that
arrives just after the stop hook, so a clean finish is never misread as a
silence.

The four readings an operator gets:

| Work state       | What is true                            | What a person does      |
| ---------------- | --------------------------------------- | ----------------------- |
| `finished`       | The agent reported `done` or `blocked`. | Fetch the branch.       |
| `active`         | Work runs. The label says what.          | Leave it alone.         |
| `active (late)`  | Work runs past its own declared bound.   | Look. Something wedged. |
| `idle`           | Nothing runs.                            | Read the nudge count.   |

### The rescue: wake on idle

The nudge stops firing on a clock. It fires on the transition into `idle` while
a turn has ended with no terminal report.

That transition is the moment the build finishes. The agent is fetched back
exactly when it can do something useful, and the work no longer completes into
an empty room. The budget is spent only on real silence.

```
  turn ends, build runs         build ends
        │                            │
        ▼                            ▼
  ┌───────────┐               ┌───────────┐
  │  active   │──────────────▶│   idle    │──▶ nudge ──▶ agent verifies
  │ Compiling │  heartbeat    │           │             and reports done
  └───────────┘  stops        └───────────┘
     no nudge spent               nudge #1
```

The budget widens to five nudges, one minute apart. Roughly five minutes
replaces roughly 91.5 seconds, and every second of it now buys tolerance for a
slow or briefly stuck harness rather than being burned inside a build. An agent
that has ignored five injected turns over five minutes is not going to answer
the sixth.

When the budget runs out the guest stops. The VM stays `idle` and carries the
spent count, so an operator sees that the agent was asked and did not answer.

The budget belongs to the turn. It does not refill when work resumes. An agent
that starts a second build after a nudge returns to `active`, and the next
silence spends the next nudge from the same budget. A heartbeat that flaps
therefore cannot nudge an agent forever.

A late heartbeat provokes nothing. It can occur alongside a live turn, and a
nudge is a poor instrument for "go investigate". Interrupting a working agent to
tell it that a background process overran turns a slow build into a lost one.
`late` is loud in the status, and a person decides.

### What the operator sees

_As an orchestrator, I run `sandbox status` and read one line per instance. I
know which VMs to leave alone, which to fetch from, and which to look at._

`sandbox status` shows three things for each instance:

- **The work state**, with the flag folded in as `Active (Late)`, plus the label
  and the beating duration of the heartbeat that carries it. For example
  `Active — Compiling (34 units), 12m of 45m`.
- **The last incremental report**, when the agent chose to send one. These stay
  voluntary. The status shows the newest one and its age.
- **The nudge count**, as spent over budget. For example
  `idle — nudged 3/5`.

The guest reports each work-state transition to the host as it happens, so an
attached drive narrates a long build instead of going quiet. The guest also
persists the state to the record that `sandbox status` reads out of band. An
operator with no attached drive still gets the answer, which is the property the
existing `turn-state.json` design already holds.

### What the agent contract says now

The contract stops teaching the agent how to do its work. Foreground and
background are the agent's choice. The prescription leaves both places that
carry it today: the agent contract in `lib/sandbox/default.nix` and the dispatch
instructions in `project/kanban/.dispatch-instructions.md`.

What the contract keeps:

- A terminal report is required to return work to the host. This is unchanged.
- Incremental reports are offered as an option, not an obligation.

A rewording of the prescription toward foreground blocking does not work. The
agent cannot block in the foreground for a cold build, because one Bash call
caps at 600 seconds. After this design nothing needs it to.

### What goes away

- **`progressStallSecs` and its stall notice.** The work state answers what the
  notice was guessing at. The knob's right value depended on a project's
  cold-build time, which is the class of guess this design exists to remove.
  The independent check does not disappear with it. It moves into the heartbeat
  contract, because a heartbeat that always beats reaches its own declared
  timeout and becomes late.

- **`ended-unreported`.** The state it named is now `idle`, and the cases it
  confused are now separate.

- **The foreground and background prescription**, in both files that carry it.

### Costs this design accepts

- **A YAML parser enters the guest crate.** The crate carries `serde_json`
  today. Block scalars need real YAML, and hand-rolling that is harder than the
  small frontmatter parser the project already has in
  `rust/katsuobushi-controller/src/project/note.rs`.

- **Shell inside a block scalar is indented.** This is the usual source of a
  check that passes and means nothing. The heartbeats the library ships become
  the examples people copy, so they must show the indentation and the
  exit-status rule clearly.

## Test Cases

Acceptance checks, by facet:

- **A heartbeat parses and beats.** A file with `label`, `timeout`, and `check`
  runs at its interval. Exit zero beats, and any other code does not. It
  qualifies when a check that exits zero puts the guest in `active` with that
  file's label.

- **Optional fields are optional.** A file with no `interval` polls at 10
  seconds. A file with no `detail` shows its label alone. It qualifies when both
  files load and beat with no error.

- **A broken heartbeat is visible.** A file the guest cannot parse never beats,
  and the guest reports the error to the host once. It qualifies when the error
  reaches the host and the file contributes nothing to the state.

- **A slow check is bounded.** A check that runs longer than its interval is
  killed and does not beat. It qualifies when a check that sleeps past its
  interval leaves the guest `idle`.

- **The shipped set covers a bare repo.** A workspace with no
  `.katsuobushi/heartbeats/` directory still reports `active` during a cold
  build. It qualifies when a dispatched card with no project heartbeats holds
  `active` for the length of its build.

- **A project's directory adds, never replaces.** A workspace with one heartbeat
  of its own still runs the shipped set. It qualifies when both a shipped
  heartbeat and a project heartbeat appear in the state.

- **The precedence order holds.** A terminal report during a beating heartbeat
  yields `finished`. A heartbeat past its timeout beside a healthy one yields
  `active (late)`. No beat yields `idle`. It qualifies when each of the three
  combinations produces the named state.

- **A turn in flight is active.** An agent that reads files and calls no build
  reports `active` for the length of its turn. It qualifies when a turn with no
  other beating heartbeat holds `active` until the stop hook fires.

- **A wedged turn goes late.** A turn in flight for longer than 60 minutes
  reports `active (late)`. It qualifies when the built-in turn heartbeat passes
  its timeout and raises the flag.

- **The nudge fires on the transition to idle.** A turn that ends while a
  heartbeat beats spends no nudge. The first nudge arrives when the last
  heartbeat stops. It qualifies when the nudge count stays at zero for the
  length of the build and reaches one after it ends.

- **The budget bounds and records.** Five nudges go out one minute apart. The
  guest then stops and holds `idle` with the spent count. It qualifies when
  `sandbox status` shows `nudged 5/5` and no sixth nudge is injected.

- **A late heartbeat provokes no nudge.** A heartbeat past its timeout raises
  the flag and injects nothing. It qualifies when the nudge count does not move
  while the flag is up.

- **Status shows all three facts.** `sandbox status` renders the work state with
  its label and duration, the newest voluntary report with its age, and the
  nudge count over the budget. It qualifies when one instance shows all three in
  one reading.

- **Status works with no attached drive.** The guest persists each transition.
  It qualifies when `sandbox status` reports the current work state for an
  instance no drive is attached to.

- **The retired knob is gone.** No stall notice fires during a cold build, and
  `progressStallSecs` no longer appears in the library arguments or the host
  spec. It qualifies when a cold build produces no stall notice.

- **The contract no longer prescribes.** Neither the agent contract nor the
  dispatch instructions tell the agent to prefer foreground or background. Both
  still require a terminal report. It qualifies when a search of both files
  finds no foreground or background rule and finds the terminal-report rule.

By hand, in a live session:

- Dispatch a card that triggers a cold build of 15 to 25 minutes. Watch
  `sandbox status` through the build. Read that the instance holds `active` with
  a build label and a growing duration, that no nudge is spent, that the agent
  is nudged within a minute of the build ending, and that the card reaches
  `done` with no intervening stall.

- Add a heartbeat that waits on something remote, with a timeout shorter than
  the wait. Watch the state pass from `active` to `active (late)` at the
  timeout, and read that no nudge went out.

- Stop the agent harness inside a running VM, then dispatch a turn. Read that
  the instance reaches `idle`, that five nudges go out one minute apart, and
  that the status then reads `nudged 5/5` and stays there.

## References

- [PDD001 Project Board Labels and Icebox](PDD001%20Project%20Board%20Labels%20and%20Icebox.md)
  — the board model this document leaves unchanged.
- [PDD002 Design, Plan and Implement Skills](PDD002%20Design,%20Plan%20and%20Implement%20Skills.md)
  — the roles that read a VM's state.
- [The `sandbox` skill](../../plugins/katsuobushi/skills/sandbox/SKILL.md) — the
  VM commands whose status output this document changes.
- [The `project-orchestration` skill](../../plugins/katsuobushi/skills/project-orchestration/SKILL.md)
  — the roles, the dispatch report bridge, and the "trust the branch, not the
  VM" rule.
- [The sandbox library](../../lib/sandbox/default.nix) — the liveness knobs, the
  agent contract, and the managed settings this document edits.
- [The guest server](../../rust/katsuobushi-sandbox-guest/src/bin/server.rs) —
  the turn-state machine, the grace window, and the nudge loop.
- [The host watchdog](../../rust/katsuobushi-controller/src/sandbox/prompt.rs) —
  the stall notice this document retires.
- [ASD-STE100 Simplified Technical English](https://www.asd-ste100.org/) — the
  rules this document is written to.
