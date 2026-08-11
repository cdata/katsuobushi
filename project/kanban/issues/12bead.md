---
id: 12bead
title: A guest branch carries the board and merges it back silently
type: bug
blocked_by: []
labels: [PDD004, landing, board]
created: 2026-08-10T16:34:04Z
disposition: accepted
disposition_at: 2026-08-11T04:13:00Z
---

## What to fix

`project/kanban/` is declared orchestrator-only state — "the orchestrator is
the **only** writer", and a guest "never writes the board". But the board still
travels inside every guest branch, because `sandbox dispatch` claims the card
(`todo -> in-progress`, a `BOARD.md` edit) and then seeds the guest from the
working tree via `git stash create`. Every branch therefore carries a
dispatch-time snapshot of the board.

At landing time that snapshot merges **back**, and it does so without a
conflict.

Observed on both landings in one session in `cdata/arcanabasis`:

```
$ git merge --squash sandbox-guest/card-580353-ff8a021f
Auto-merging project/kanban/BOARD.md
Automatic merge went well; stopped before committing as requested

$ git diff --cached -- project/kanban/BOARD.md
@@ -40,6 +40,8 @@ kanban-plugin: basic
 ## In Progress
+- [ ] [[580353]]
+
 ## Needs Review
```

The card was in `needs-review` at that moment and about to become `ready`. The
staged change would have re-added it to **In Progress**, in the very commit
that lands its work. The same thing happened on the next card.

Nothing warns. There is no conflict to resolve, no prompt, no message — the
auto-merge succeeds and the wrong board state is silently staged. It is caught
only by an orchestrator who inspects `git diff --cached` on every landing and
knows to look at the board specifically.

The skill anticipates the *conflict* case — "a conflict in a file the guest
never touched (such as `BOARD.md`, which every guest branch carries stale from
dispatch) is resolved mechanically and needs no record" — but the dangerous
case is the one that does **not** conflict.

## Why it matters

This is the one failure mode the single-writer invariant exists to prevent, and
the landing procedure reintroduces it. A silently corrupted board is worse than
a noisy one: the card can end up in two lanes, or back in a lane it has left,
inside a commit whose message says it shipped.

## What to fix

Options, roughly in order of preference:

- Exclude `project/kanban/` from the dispatch seed, so a guest branch never
  carries the board at all. The guest is documented as never needing to write
  it.
- Or exclude the board from the landing merge, since the host's copy is
  authoritative by definition.
- Or make the landing fail loudly if the squash stages any board path.

## Host observation from the `3336f8` landing (2026-08-10)

Recorded by the orchestrator as evidence, not as a conclusion — the report above
was observed in `cdata/arcanabasis`, and this thread's first landing in *this*
repo behaved differently. Verify before assuming either shape.

At dispatch time the host working tree did have board changes (a modified
`BOARD.md` plus four untracked card notes), and `sandbox dispatch` then claimed
the card, modifying `BOARD.md` again. Despite that:

- The guest branch's parent was `ab1f06a` — the host's `main` tip — **directly**.
  There were no `WIP on …` / `index on …` stash-seed commits between them.
- The branch contained exactly one commit, and it touched no `project/kanban/`
  path.
- `git merge --squash` staged three files, none under `project/kanban/`.

So on this landing no board snapshot travelled in the branch and none merged
back.

### Correction — it *does* reproduce here (observed on this card's own dispatch)

The paragraph above is falsified by the very next dispatch. When `12bead` itself
was dispatched, the seed behaved as the original report describes:

- The branch's ancestry contained two stash-seed commits —
  `bbf7857 WIP on (no branch): 4f96a6c` and `1d3ff8a index on (no branch): …` —
  carrying the dispatch-time host working tree.
- Diffed against the merge base, the branch therefore showed `BOARD.md` plus the
  card notes `3336f8.md`, `3ccced.md` and `12bead.md`.
- The agent's own commit `e3bc0ba` touched **no** `project/kanban/` path. The
  board travelled entirely in the seed, exactly as reported.

So the failure mode is real in this repo; the `3336f8` landing simply did not
trigger it. What differs between the two dispatches is the state of the host
working tree and git index at dispatch time, and establishing *which* of those
decides it is the useful question — not whether the bug exists.

## Acceptance criteria

- [ ] A landing never stages a `project/kanban/` change that came from the guest branch
- [ ] The board's state after a landing matches the host's, with no manual restore
- [ ] If a guest branch does touch the board, the landing says so rather than merging it quietly
- [ ] The landing procedure in the `sandbox` skill matches the new behaviour

## Review notes

Round 1, reviewed by `review-12bead-fa400d41` against branch tip `e3bc0ba`.
**VERDICT: needs-changes** (reviewer would block).

Strongest finding — the `SKILL.md` landing guard, at both occurrences (lines
~397 and ~436), uses:

```sh
git reset HEAD -- project/kanban/
```

That only **unstages** the board files from the index; it does not restore the
working tree. After the guard fires, the host working tree still holds the
guest's merged board content, which violates AC 2 ("The board's state after a
landing matches the host's, with no manual restore"). The reviewer's proposed
fix is one line in each recipe:

```sh
git checkout HEAD -- project/kanban/
```

Everything else checked out:

- `strip_board_from_stash`, its `resolve_seed` integration, and the dispatch
  wiring are correct.
- The seed handling covers **both** the `WIP on …` and `index on …` commits.
- Test quality: 7 new tests, all judged meaningful and all would fail against
  the unfixed code. Roughly 60% test / 40% logic.
- No regressions. Nothing left running in the reviewer VM.

Resolved the open question about the test count: the measured workspace total is
**634** (513 + 64 + 47 + 10), all passing, consistent with the figure on
`3336f8`. The 520 cited in the implementor's `done` was a narrower scope, not a
suite that failed to run.

### Round 2 — implementor response

The implementor agreed with the finding and replaced `git reset` with
`git checkout HEAD -- project/kanban/` in both recipes (plain-git ~399,
jj-colocated ~440). It also took up the adjacent question the bounce raised:
`git checkout HEAD -- <path>` does not remove a file the guest **added** that
HEAD lacks, so it added a `git rm --cached` + `rm -f` loop for entries still
staged after the checkout, covering modifications, deletions and additions.

New commit `67200cf` (`SKILL.md` only, +10/-2). Verified on the host: `e3bc0ba`
remains an ancestor, so the round-1 work was **not** dropped despite the
implementor reporting that it reached the tip via a cherry-pick onto the remote
tip — that cherry-pick landed as a fast-forward. Neither agent commit touches
`project/kanban/`.

Still-open factual discrepancy carried into round 2: the implementor reports the
main crate at **520** tests (total 641); the reviewer measured it at **513**
(total 634). Round 2 changed no Rust code, so the figure should not have moved.
One of the two counts is wrong, and it is the acceptance gate's evidence.

### Round 2 review — accept

Re-reviewed by `review-12bead-fa400d41` against `e3bc0ba..67200cf`.
**VERDICT: accept.**

- The round-1 finding is resolved: `git checkout HEAD -- project/kanban/`
  restores both the index and the working tree, satisfying AC 2.
- The `3ccced` hazard raised by the orchestrator is **safe**, verified by
  tracing the recipe step by step: the three-way merge preserves a host-only
  card note in the index at HEAD-identical content, so it never appears in
  either staged diff and the deletion loop never reaches it.
- Both recipes (plain-git and jj-colocated) are identical and correct.
- `>/dev/null 2>&1 || true` is *necessary*, because `git checkout` on a
  directory exits non-zero when any path is absent from HEAD (the guest-added
  case) and the `|| true` is what lets the cleanup loop run. The reviewer notes
  a mild AC 3 risk — it would also swallow a checkout failure arising from some
  other cause — but did not consider it a blocker.
- No regressions from round 1.

Test-count discrepancy resolved: the correct workspace total is **634**
(513 + 64 + 47 + 10), re-confirmed by re-running `nix develop -c rust test`. The
implementor's 641 (claiming 520 for the main crate) is wrong by 7 — a misread,
not a suite that failed to run. Both of its `done` reports carried the bad
figure.

### Post-landing validation (the next dispatch)

The first dispatch after this card landed (`9f0012`, instance
`card-9f0012-b8aed9ee`) confirms the fix works in production:

- The branch's seed is now a **single** commit titled `dispatch seed`, replacing
  the previous pair of `WIP on (no branch): …` / `index on (no branch): …` stash
  commits.
- Diffed against the merge base, the branch contains **zero** `project/kanban/`
  paths — where `12bead`'s own dispatch had carried `BOARD.md` plus three card
  notes.

Also worth recording: this card's own landing reproduced the bug one last time.
`git merge --squash` staged `BOARD.md` and `12bead.md` from the guest seed. It
surfaced as a *conflict* only because the orchestrator had edited the same card
note in the same window; absent that, it would have merged silently, exactly as
the card describes. The newly-added
`git checkout HEAD -- project/kanban/` guard cleaned it, and the landing commit
`7d7a7b8` contains only the four non-board files.

## Dispatch log

- Implementor VM: `card-12bead-a539c695` (launched 2026-08-10; paused warm after
  `done`).
- Reviewer VM: `review-12bead-fa400d41` (launched 2026-08-10).
- Branch under review: `sandbox-guest/card-12bead-a539c695`, author's commit
  `e3bc0ba`, delivered to the reviewer as `delivered/card-12bead-a539c695`. The
  branch also carries two dispatch stash-seed commits (`bbf7857`, `1d3ff8a`)
  that are not the author's work.
- Implementor's `done` summary: two-layer fix — `strip_board_from_stash` in
  `start.rs` rewrites the stash commit to replace board changes with HEAD state
  before the mirror push, activated by a new `board_exclude` parameter on the
  dispatch path; plus a `SKILL.md` landing guard that resets `project/kanban/`
  after `git merge --squash` when board files were staged. Claimed 520 tests
  passing, lint clean.
- Open question passed to the reviewer: the 520-test figure is lower than the
  634 cited on `3336f8`, which landed immediately before this card was seeded.
  Asked the reviewer to measure the real workspace count and account for the
  difference.
