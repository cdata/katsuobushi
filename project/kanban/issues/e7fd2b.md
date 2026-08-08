---
id: e7fd2b
title: Refuse a dispatch that would freeze agent-authored commits
type: feature
blocked_by: [9344ec]
labels: [landing, sandbox]
created: 2026-08-08T06:39:06Z
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
