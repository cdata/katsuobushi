---
id: e7fd2b
title: Refuse a dispatch that would freeze agent-authored commits
type: feature
blocked_by: [9344ec]
labels: [landing, sandbox]
created: 2026-08-08T06:39:06Z
disposition: accepted
disposition_at: 2026-08-10T02:00:10Z
---

## What to build

Card `9344ec` documents that landed commits must be re-authored to the repository
owner, and that this must happen **before the next sandbox dispatch** — after
that, the new guest commit makes them ancestors of an immutable remote bookmark
and jj refuses to rewrite them. The requirement is correct and now correctly
documented. Nothing enforces it.

The peer reviewer on that card proposed the enforcement, and the shape is good
because it fires at exactly the moment the window closes and needs no cooperation
from the orchestrator: before `sandbox dispatch` seeds a guest from the host
working tree, check whether the range about to be frozen contains commits authored
by an agent identity, and refuse if it does — naming them.

Roughly: `git log --author=agent@katsuobushi.local --oneline <dispatch-base>..HEAD`,
refuse on any match. Settle the details as part of the work:

- What the dispatch base should be. `HEAD` alone is not enough; the check needs to
  bound the range to commits that this dispatch would actually freeze.
- Whether to match on a fixed agent email or on "any author that is not the
  configured user", which is more robust but riskier around legitimate
  co-authored or imported history.
- Whether it refuses outright or warns. Refusing is the point — a warning at
  dispatch time is a notice nobody reads — but it must be overridable, because an
  operator may have a good reason and should not be stuck.

Fail closed on the check itself: if the probe cannot run, do **not** silently
allow the dispatch to proceed as though the range were clean. Say the check could
not run.

Also fix a small documentation gap the same review found: the Bounce section of
the sandbox skill gives duplication commands but never restates that the
attribution and description requirements apply to bounce-duplicated commits too.
The "every duplicated commit" wording covers it, but not where a bounce reader is
working.

## Acceptance criteria

- [ ] A dispatch that would freeze one or more agent-authored commits refuses, and
      names them.
- [ ] A dispatch with a clean range proceeds unchanged.
- [ ] The refusal tells the operator how to fix it (`jj metaedit --update-author`
      / `git commit --amend --reset-author`) and how to override deliberately.
- [ ] A probe that cannot run reports that it could not run; it never reports a
      clean range it did not verify.
- [ ] The Bounce section restates the attribution and description requirements for
      bounce-duplicated commits.
- [ ] `nix develop -c rust lint` is clean.


## Dispatch log

- implementor: `card-e7fd2b-3a047397`

Round 1 — implementor reported `done`. Added `guard_against_agent_commits` to
`sandbox/dispatch.rs`: before each dispatch (unless `--force`) it enumerates
existing `refs/remotes/sandbox-guest/` bookmarks, then runs `git log HEAD` with
those as `^` exclusions and `--author=agent@katsuobushi.local`. Any match refuses
and names the commits. The probe **fails closed** — if either git invocation
cannot run or exits non-zero, the dispatch errors rather than proceeding on an
unchecked range. Nine tests. Also restated the attribution requirements in the
sandbox skill's Bounce section.

617 tests, lint clean.

### It fired correctly in production on its first run

The host's tree contained agent-authored commits at landing time, so the guard
was exercised for real rather than only in tests:

```
Error: refusing dispatch: 3 commit(s) authored by agent@katsuobushi.local
would be frozen by this dispatch:

  9bf7545 feat(sandbox): refuse dispatch when agent-authored commits would freeze
  b818732 docs(orchestration): fix three contradictions in fresh-reviewer guidance
  9f89191 docs(orchestration): document fresh-reviewer protocol and off-contract cleanup
```

It named exactly the right three, explained the consequence, gave both fix
commands and the override.

**And it caught a defect the documented procedure had missed.** Two of those
three had already been through `jj metaedit --update-author`, which reported
success and did nothing (filed as `6d77aa`). So the guard's first real act was to
catch commits that the prose-level fix had silently failed to correct — which is
precisely the argument the `9344ec` reviewer made for checking the **outcome**
rather than trusting the step.

## Review notes

### Round 1 — `review-e7fd2b-1b74dd9c` — VERDICT: accept

Gates: 617 tests, `rust lint` clean, `markdown lint` clean.

**Ordering is correct — a refusal leaves no dirty board.** This was the host's
main worry. The agent-commit guard runs **before** `prepare()`, and `prepare()`
does its own state guard, compose, then claim in that order. So a refusal from
either guard returns before any board mutation; no path leaves a card marked
`in-progress` with no VM. The module comment documents the ordering and an inline
comment restates it.

**Fail-closed verified, not assumed.** Both probes (`for-each-ref`, `git log`)
× both failure kinds (spawn error, non-zero exit) = four paths, all returning
`Err` and surfacing "could not run". All four are tested by name.

**The fixed author literal is right, and the reviewer rebutted the alternative.**
The host suggested "any author that is not the configured user" might be more
robust; the reviewer argued it would generate mass false positives from imported
history, co-authored commits and legitimate third-party contributors in any
shared repo, whereas the guest identity is a system constant, not user
configurable. It is documented in both places a reader would look — a four-line
doc comment on `AGENT_EMAIL` (`:38-41`) and the Attribution section of
`SKILL.md`. Accepted as the better choice.

**First-ever dispatch is benign.** With no `sandbox-guest` refs, `git log` runs
unbounded from `HEAD` to root. A false positive needs agent-authored commits in
host history with **no** bookmark covering them — only reachable by an
out-of-procedure path (cherry-picked but never fetched) or a hand-made commit
using the agent identity. So no realistic path to habitual `--force`.

**Ten tests, not the nine claimed**, and the two argument-inspection tests verify
what the implementation actually sent to the fake rather than what the test
constructed.

**Known gaps, recorded not filed:**

- No test asserts that on a first-ever dispatch `git log` receives **zero** `^ref`
  arguments — the clean-path test covers the outcome but not the range boundary.
- `--force` leaves no structured audit trail. The frozen agent-authored commit is
  its own evidence, but there is no record that the guard was bypassed. Fine
  while force is rare; a concern if it becomes common enough to be noticed only
  in post-mortems.
