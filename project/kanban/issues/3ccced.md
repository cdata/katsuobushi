---
id: 3ccced
title: A nudges-exhausted drive exit can miss the terminal report
type: bug
blocked_by: []
labels: [PDD004, sandbox, dispatch]
created: 2026-08-10T21:23:58Z
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

