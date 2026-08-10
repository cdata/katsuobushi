---
id: 12bead
title: A guest branch carries the board and merges it back silently
type: bug
blocked_by: []
labels: [PDD004, landing, board]
created: 2026-08-10T16:34:04Z
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

## Acceptance criteria

- [ ] A landing never stages a `project/kanban/` change that came from the guest branch
- [ ] The board's state after a landing matches the host's, with no manual restore
- [ ] If a guest branch does touch the board, the landing says so rather than merging it quietly
- [ ] The landing procedure in the `sandbox` skill matches the new behaviour

