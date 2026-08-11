---
id: 3ccced
title: A nudges-exhausted drive exit can miss the terminal report
type: bug
blocked_by: []
labels: [PDD004, sandbox, dispatch]
created: 2026-08-10T21:23:58Z
disposition: accepted
disposition_at: 2026-08-11T04:13:00Z
---

## What to fix

Raised by the independent reviewer of card `3336f8` as a non-blocking finding,
so it was filed rather than bounced.

Card `3336f8` fixed the case where the guest pushed `TurnCompleted` before
`Report`, which let an armed host drive exit before it had seen or journaled the
terminal text. That ordering is now correct.

A related exit path remains. When the guest's **auto-nudges are exhausted**, the
drive exits naturally, and that exit carries the same miss risk as a drive whose
process was killed: a terminal report arriving afterwards is not journaled.

The difference is in the documentation, not the behaviour. The `sandbox` skill
documents the miss only for the **killed-process** case ("Losing the **output**
is fully covered; losing the **process** is not"). It does not say that a
nudges-exhausted natural exit lands in the same place. An orchestrator reading
the skill would not expect it.

The reviewer notes the risk is mitigated by `--until-report`, and that the
condition is pre-existing rather than introduced by `3336f8`.

## What to fix

- Either journal a terminal report that arrives after a nudges-exhausted exit,
  or say plainly in the `sandbox` skill that this exit path has the same gap as
  a killed drive, and name where the conclusion can be read instead.
- Make sure the skill's "Limit" paragraph covers **both** exit paths, not only
  the killed-process one.

## Acceptance criteria

- [ ] The nudges-exhausted exit path is either journaled or documented as unjournaled
- [ ] The `sandbox` skill's journaling limit covers both the killed-process and nudges-exhausted paths
- [ ] An orchestrator can tell, from the skill alone, when a terminal report may not reach `reports.ndjson`

## Review notes

Reviewed by `review-3ccced-05f985cb` against branch tip `a025b2c`.
**VERDICT: accept.**

The reviewer checked the prose against the code rather than judging wording, and
reported the exact sources it read:

- `server.rs:544-558` — the `GraceExpired` / nudges-exhausted branch sends
  `TurnCompleted{reported:false}` and clears the turn.
- `prompt.rs:516-531` — `handle_phase1_line` exits the drive on
  `TurnCompleted{reported:false}` when `until_report=false`, and stays armed
  when it is true.
- `server.rs:379`, `1195-1285` — `last_report_text` is set on **every** `Report`
  event regardless of turn state, and persisted to `work-state.json` as
  `lastReportText`.
- `status.rs:826`, `544` — `lastReportText` surfaces as the `note:` line in the
  detail view.

Per criterion: AC 1 met (documentation route, which the criterion permits);
AC 2 met (both exit paths now under the Limit bullet); AC 3 **largely** met.

Findings, none blocking:

- The nudges-exhausted bullet does not say **which command** is affected.
  `dispatch` is armed by default and its drive does not exit on
  nudges-exhausted; the exit applies to `sandbox prompt` without
  `--until-report`. The reviewer judged the claim non-false because the
  preceding table (lines 240-249) covers the distinction and the wording is
  conditional ("when the drive exits naturally because…").
- The `--until-report` mitigation is accurate for `sandbox prompt` but is a
  **no-op for `dispatch`**, which `main.rs:553` asserts is already armed.
- "regardless of nudge count" is slightly imprecise: staying armed keeps the
  drive alive after nudges stop; it does not increase the nudge count.

No contradictions with `project-orchestration`. Nothing left running.

### Incident during this review — the reviewer stalled unreported

Recorded because it is evidence about the machinery this card describes, and
because the recovery is the reason a verdict exists at all.

The reviewer ended its turn **without reporting** ~8 minutes in
(`turn-state.json`: `endedAt` and `lastActivityAt` both `22:59:27`, `phase`
still `in-flight`). It then sat for roughly two hours. `work-state.json` showed
`workState: active`, `isLate: true`, `carryingDurationSecs: 7250` — and
**`nudgeCount: 0` against `nudgeBudget: 5`**. No auto-nudge fired even once.

The drive was `sandbox prompt --until-report`, which per `prompt.rs:516-531`
stays armed on `TurnCompleted{reported:false}` — so it waited indefinitely with
no timeout and no notice.

Recovery: the orchestrator stopped the drive and re-prompted the still-running
VM asking it to report from its existing context without redoing the analysis.
It returned the full verdict above immediately, so no review work was lost.

Note for future orchestrators: the `LIVENESS` / `WORK` columns in
`sandbox status` kept climbing during the stall and read as healthy progress.
`turn-state.json`'s `lastActivityAt` was the only honest signal.

## Dispatch log

- Implementor VM: `card-3ccced-42f48a10` (launched 2026-08-10; paused warm after
  `done`).
- Reviewer VM: `review-3ccced-05f985cb` (launched 2026-08-10).
- Branch under review: `sandbox-guest/card-3ccced-42f48a10`, tip `a025b2c`,
  delivered as `delivered/card-3ccced-42f48a10`. Seed board-clean.
- Implementor took the **documentation** branch of AC 1 rather than journaling
  the path. Its `done` summary: expanded the journaling "Limit" paragraph to
  cover both exit paths, naming the nudges-exhausted natural exit as unjournaled
  alongside the killed-process case, pointing at `work-state.json` /
  `sandbox status` as the fallback for both, and noting `--until-report`
  mitigates the nudges-exhausted case. Markdown format and lint clean.
- Review directive asks the reviewer to check the **prose against the code**
  rather than judging wording: a docs fix describing runtime behaviour is only
  correct if the behaviour matches. Also asked it to test the `--until-report`
  claim, since that flag is documented elsewhere as now a no-op on some paths.
