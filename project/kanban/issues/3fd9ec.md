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

## Review notes

Reviewed by `review-3fd9ec-7a10d6ae` against branch tip `92cbc82`.
**VERDICT: accept**, all four criteria PASS.

**The `is_work_idle` question, answered.** The guard was introduced in `874b3a8`
to preserve nudge budget for genuine silence. Its fatal flaw: `GraceExpired`
while work was active returned no `schedule_grace`, so the grace loop exited
permanently — which is precisely why `nudgeCount` stayed at 0 in the incident.
Removing it does make mid-build nudges possible, but **only for turns that have
already ended unreported**; the nudge is queued in the MCP stream and cannot
kill a running build. An agent working normally, whose turn has not ended, is
unaffected. The orchestrator's concern about nudging a legitimately busy agent
does not materialise.

**B's derived timeout cannot misfire.** `rearmed_timeout` is computed from
`MAX_NUDGES_DEFAULT * NUDGE_INTERVAL_MS_DEFAULT` rather than from actual config,
but `rearmed_at` is only set after `TurnCompleted{reported:false}`, which the
guest emits only once it has exhausted its **configured** budget. So the bound
never applies while nudges are legitimately in flight, and a project overriding
the defaults cannot turn a progressing drive into a spurious failure.

Per criterion: (1) PASS — nudges fire unconditionally on `GraceExpired`;
(2) PASS — `rearmed_timeout` fires ~6 min after `TurnCompleted{reported:false}`;
(3) PASS — `render_liveness` shows "ended unreported Xh ago" when `ended_at` is
set; (4) PASS — **each** of the three regression tests was confirmed to fail
against unfixed code (`inject_nudge=None` vs expected `Some`; a missing struct
field, i.e. compile error; and a display-string assertion).

Deleting the `WorkStateIdle` firing path broke nothing — the reviewer confirmed
no remaining dependency.

Workspace gate: **644 tests (522 + 64 + 48 + 10) across 6 crates**, 0 failures,
lint clean. Nothing left running.

Non-blocking findings, filed separately as a cleanup card:

- `is_work_idle` is now dead state — still set, never read — and its doc comment
  is stale.
- A Nix comment in `lib/sandbox/default.nix` still describes the old
  idle-transition behaviour.

## Dispatch log

- Implementor VM: `card-3fd9ec-a543cd73` (launched 2026-08-10; paused warm after
  `done`).
- Reviewer VM: `review-3fd9ec-7a10d6ae` (launched 2026-08-10).
- Branch under review: `sandbox-guest/card-3fd9ec-a543cd73`, tip `92cbc82`,
  delivered as `delivered/card-3fd9ec-a543cd73`. Seed board-clean.
- Implementor found **three** distinct bugs rather than one:
  - **A** (`server.rs`) — the `is_work_idle` guard on `GraceExpired` deferred
    nudges until work went idle. This is the root cause of the incident: with
    `carryingDurationSecs` climbing, the reviewer never looked idle, so no nudge
    ever fired. Guard removed; `nudge_pending` and the `WorkStateIdle`
    nudge-firing path deleted.
  - **B** (`prompt.rs`) — new `LineFlow::ReArmed`. After
    `TurnCompleted{reported:false}` in `--until-report` mode the drive now runs a
    `rearmed_timeout` (from `MAX_NUDGES_DEFAULT * NUDGE_INTERVAL_MS_DEFAULT`)
    and fails with a clear message once the budget is provably exhausted,
    instead of looping on heartbeats forever.
  - **C** (`status.rs`) — `render_liveness()` now prints "ended unreported Xh
    ago" when `phase=in-flight` with `ended_at` set, rather than the misleading
    climbing-duration line.
  - Regression tests added for all three paths; claimed 644 tests, lint and
    format clean.
- Highest-risk item flagged to the reviewer: removing `is_work_idle` means
  nudges fire on the grace timer even while an agent is legitimately mid-build.
  Asked it to establish why the guard existed and what a nudge does to a
  normally-working agent — as a question, since the tradeoff is real either way.
  Also asked whether B's derived timeout can fire on a turn that is still
  progressing when a project overrides the nudge or stall settings.

