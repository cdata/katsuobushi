---
id: 3fd9ec
title: An armed drive waits forever when the auto-nudge never fires
type: bug
blocked_by: []
labels: [PDD004, sandbox, dispatch]
created: 2026-08-11T00:55:34Z
---

## What to fix

Observed directly while driving the PDD004 thread, during the review of card
`3ccced`. An armed drive waited **two hours** on a turn that had already ended,
and the auto-nudge that exists to recover exactly this situation never fired.

The instance was `review-3ccced-05f985cb`, driven by
`sandbox prompt --until-report`. Its state files at the point of diagnosis:

```json
// turn-state.json
{ "turnId": 1, "phase": "in-flight",
  "acceptedAt": "2026-08-10T22:51:48Z",
  "endedAt":    "2026-08-10T22:59:27Z",
  "lastActivityAt": "2026-08-10T22:59:27Z" }

// work-state.json
{ "workState": "active", "isLate": true,
  "carryingDurationSecs": 7250,
  "nudgeCount": 0, "nudgeBudget": 5 }
```

The turn ended unreported about eight minutes in. Two hours later
`nudgeCount` was still **0** against a budget of **5**, and `phase` was still
`in-flight`. The drive had no timeout and emitted no notice; it would have
waited indefinitely.

## Why it matters

The auto-nudge is the mechanism the `sandbox` skill leans on to justify treating
a returned drive as concluded work: "a sandbox that stops silently is
automatically re-prompted a few times to report". If a turn can end unreported
*without* the nudge engaging, an armed drive can block forever, and the
orchestrator has no signal that anything is wrong.

This is adjacent to but distinct from the two cards already landed:

- `3336f8` fixed the message **ordering** so a terminal report is not missed.
- `3ccced` documents the exit paths whose late reports go **unjournaled**.

Neither covers a turn that ends unreported and is never nudged at all.

Note that `sandbox dispatch` is armed by default (`main.rs:553`), so it is
exposed to the same wait; `--until-report` on `sandbox prompt` reaches the same
armed state via `prompt.rs:516-531`.

## The reporting is misleading too

Throughout the stall, `sandbox status` showed `LIVENESS: in-flight` and a
`WORK: Active (Late) — Agent work, 2h` that kept climbing, which reads as
progress. `carryingDurationSecs` counts elapsed time, not activity. The only
honest signal was `turn-state.json`'s `lastActivityAt`, frozen at the moment the
turn ended. An orchestrator watching the status table alone would conclude the
VM was working.

## Recovery that worked

Stopping the drive and re-prompting the still-running VM — asking it to report
from its existing context without redoing the work — produced the full verdict
immediately. No review work was lost. That the agent could still answer suggests
a nudge would have worked, had one been sent.

## What to fix

- Establish why `nudgeCount` stayed 0 after `endedAt` was set, and make the
  nudge fire on that transition.
- Give an armed drive a bound: after the nudge budget is genuinely exhausted, it
  should conclude or surface a failure rather than wait forever.
- Make `sandbox status` distinguish elapsed time from activity, so a frozen
  `lastActivityAt` is visible without reading the state files by hand.

## Acceptance criteria

- [ ] A turn that ends unreported triggers the auto-nudge, and `nudgeCount` advances
- [ ] An armed drive cannot wait indefinitely once the nudge budget is exhausted
- [ ] `sandbox status` surfaces staleness of `lastActivityAt`, not just elapsed duration
- [ ] There is a regression test for a turn ending unreported while a drive is armed

