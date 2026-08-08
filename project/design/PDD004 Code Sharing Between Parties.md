# PDD004 Code Sharing Between Parties

## Introduction

Work on this project passes between parties. One party writes a change. A second
party reviews it. A third integrates it. Today only the last of these uses the
version control system as a collaboration tool. The other two exchange work by
copy, by hand, and by an orchestrator that reaches into another party's storage.

This design fixes how a branch moves between parties, when it enters the owner's
history, and whose name it carries when it does. It keeps the isolation that
makes a sandbox worth using. It removes the class of failure that comes from
rewriting a commit after it is published.

The design changes four things. A branch enters the owner's history only after a
peer review passes. A landed commit is created, never rewritten. A resumed
instance sees the work that landed while it slept. The orchestrator becomes a
named role instead of a machine.

## Goals

- Nothing enters the owner's history until a peer review passes.
- A landed commit carries the owner's name, and the branch it came from keeps
  the name of the party that wrote it.
- Landing creates a commit. It never rewrites one.
- A resumed instance works against the current tip, not the tip it launched
  from.
- Moving a branch between parties is one command, not a raw `git push` at a
  storage path.
- One card contributes one commit to the owner's history.

## Non-goals

- **A shared repository.** Each instance keeps its own mirror. The guest sees
  exactly one host directory, so instances cannot reach each other. Branch
  movement stays deliberate.
- **A second integrating party.** The orchestrator integrates. This design names
  the role so that the role can move later. It does not build a second
  integrator.
- **A provenance record in the commit.** The retained branch and the card
  already answer where a commit came from. This design adds no note and no
  trailer.
- **Merge-based integration.** A landed commit is a copy. It has no ancestry
  link to the branch it came from, so a merge would duplicate the work.
- **Changing the sandbox boundary.** Network rules, the share mount, and the
  secret handling are unchanged.

## Body

### The three parties

This document names three roles. One person or one agent can hold more than one
of them, and the roles do not name machines.

- **The writer** makes a change on a branch. A dispatched sandbox holds this
  role today.
- **The reviewer** reads that branch and accepts it or returns it. A second
  sandbox holds this role, and it is never the writer.
- **The orchestrator** moves branches between the other two, integrates an
  accepted branch, and writes the board. It runs on the host today.

The owner is the person whose repository this is. The owner accepts a `ready`
card. The owner is not one of the three roles.

### Ground truth

Hard facts a fresh reading needs, checked against the tree at the time of
writing:

- **A guest sees exactly one host directory.** The whole per-instance state dir
  is exposed as a single 9p share (`lib/sandbox/default.nix` lines 221 to 224).
  It holds that instance's `sync.git`. No instance can read another instance's
  mirror. The orchestrator is the only path between two parties.

- **The guest commits as an agent.** The provision script sets
  `user.email "agent@katsuobushi.local"` and `user.name "Katsuobushi agent"`
  (`lib/sandbox/default.nix` lines 1609 and 1610). Every commit a guest makes
  carries that name.

- **The guest clones its own mirror and works on one branch.** The provision
  script runs `git clone ${shareMount}/sync.git` and then checks out
  `sandbox/$instance` (`lib/sandbox/default.nix` lines 1613 and 1616).

- **A guest launches from a snapshot of the orchestrator's tree.** The seed is
  `git stash create`, and `HEAD` when the tree is clean
  (`rust/katsuobushi-controller/src/sandbox/start.rs` lines 457 and 458). The
  mirror holds that snapshot and does not change while the instance runs.

- **Work lands before review today.** The report bridge says: on `done`, fetch,
  land the branch, then set the card to `needs-review`
  (`plugins/katsuobushi/skills/project-orchestration/SKILL.md` lines 256 and
  257). A card that bounces three times lands four times.

- **Ancestry cannot answer whether work landed.** Landing copies the change, so
  the landed commit and the guest commit share no ancestor. `sandbox fetch`
  compares the branch tip against the launch seed instead
  (`rust/katsuobushi-controller/src/sandbox/fetch.rs` lines 74 and 174).

- **One card maps to one commit prefix.** The card `type` maps to the closing
  commit's Conventional-Commit prefix
  (`plugins/katsuobushi/skills/project/SKILL.md` line 134). The convention is
  already singular.

- **`jj duplicate` keeps the author of the source.** Measured in a scratch
  colocated repo. `JJ_USER` and `JJ_EMAIL` do not change the result.

- **A squash-merge needs no author repair.** Measured in a scratch repo.
  `git merge --squash` followed by `git commit` produces a commit authored by
  the owner, with every file from the branch, and no author flag. A
  `git cherry-pick` of the branch tip stages one commit only.

- **A rewrite after publication can fail without saying so.**
  `jj metaedit --update-author` prints `Modified 1 commits` and a new commit ID,
  changes nothing, and writes no operation, when the target is immutable. The
  same command with `--ignore-immutable` prints `Rebased N descendant commits`
  and takes effect.

### The fault: three separate problems, one cause

The orchestrator copies a branch into its own history and then repairs the
result. Every repair is a rewrite, and every rewrite is where the work goes
wrong.

The first problem is unreviewed code. The orchestrator lands on `done`, so a
change sits in the owner's history while a reviewer decides whether it is
correct. A bounced card lands again for each round.

The second problem is attribution. A copy keeps the guest's name, so the
orchestrator rewrites the author afterwards. That rewrite must happen before the
next dispatch, because the next dispatch makes the commit an ancestor of a
remote bookmark and the tool then refuses. In one session this repair silently
did nothing on 37 commits.

The third problem is reach. A reviewer cannot fetch a branch from the party that
wrote it, so the orchestrator pushed branches into another instance's storage by
hand. That worked. It is not a command, so nobody can test it or fix it.

### A branch is the unit under review

The party that writes a change owns a branch. That branch is the review artifact
and the revert artifact. It carries the writer's name, and it keeps that name.

The reviewer reads that branch. It does not read the owner's integrated tip. The
orchestrator moves the branch into the reviewer's mirror, and the reviewer
fetches it there.

A review round adds commits to the branch. It never rewrites the branch. The
reviewer can then answer "what changed since I last read this" with a range.

### The orchestrator is a role

The orchestrator moves work between parties and integrates it. It is the single
writer of the board and of the owner's history.

That role runs on the host today. It does not have to. A later design can put
the orchestrator in its own sandbox without changing anything here. For that
reason this document says "orchestrator" and never "host" for the role, and it
restates the board rule as **the board is orchestrator-only state**.

### Work enters the owner's history at `ready`

The orchestrator lands a branch when the peer review accepts it, and the card
moves to `ready`. Before that moment the owner's history holds no part of it.

This gives three properties. The owner's history holds only reviewed work. A
bounce costs no landing. The question "has this landed" has an exact answer,
because the card says so.

The owner still accepts a `ready` card. That gate does not move. A returned card
needs a revert, and the retained branch is what the revert uses.

### Landing creates a commit

The orchestrator applies the whole branch as one commit:

```
git merge --squash sandbox/<instance>    # stage every commit on the branch
git commit -m "<type>(<scope>): <card title>"
```

The commit is the owner's by construction. A new commit has no author to
inherit, so no flag sets the author and nothing repairs it afterwards. The
result is signed by the owner's existing configuration.

The message comes from the card, not from a commit on the branch. A branch that
bounced twice holds messages like "address review findings", which mean nothing
in the owner's history.

CAUTION: Do not land a branch with `git cherry-pick <branch-tip>`. That applies
the tip commit alone. On a bounced card it lands the review fix and drops the
original work.

Nothing is rewritten, so three failures cannot occur. The immutability of a
published commit does not apply. A silent no-op cannot happen. There is no
deadline to re-author before the next dispatch.

One card becomes one commit. The Conventional-Commit prefix comes from the card
`type`. The review rounds stay on the branch, where a reader can find them until
the card is accepted.

### A resumed instance sees the current tip

When the orchestrator resumes a paused instance, it refreshes the base in that
instance's mirror to the current tip. The guest then rebases its own branch onto
that base and continues.

The guest rewrites only its own branch, in its own mirror. This is ordinary work
on a topic branch. It is not the fault that PDD-era orphaning came from, because
the orchestrator never rewrites what the mirror holds.

### Moving a branch is a command

`sandbox fetch` moves a branch from an instance to the orchestrator. This design
adds the opposite direction as a command of the same shape. The orchestrator
uses it to give a reviewer the branch under review, and to refresh a resumed
instance's base.

A raw `git push` at a storage path is not this command. The command belongs in
the same place as the other verbs, so an operator can find it, and a test can
cover it.

### Drift between review and integration

A branch is reviewed against the tip it was based on. It is integrated onto the
tip as it is then. Another card can land in between.

The orchestrator integrates onto the current tip and reconciles what does not
apply. It uses its judgement. In most cases a reconciliation needs no second
review. When a reconciliation changes what the reviewer agreed to, the card
returns to review. That case is rare and is handled one card at a time.

### What goes away

- The rebase-based landing workflow, and the orphaning it caused.
- The rule that re-attribution must happen before the next dispatch.
- Repeated landings for one card.
- Raw `git push` against another instance's storage.

The guard that refuses a dispatch when agent-authored commits would freeze stays.
It no longer has a common case to catch, and it still catches work landed by
another route. Its purpose changes from routine to backstop, and the code says
so.

### Costs this design accepts

- **A reviewer reads a branch, not the integrated result.** The reviewer does
  not see interactions with work that landed during the review. The
  reconciliation step carries that risk instead.
- **Ancestry still cannot answer whether work landed.** Landing copies, so
  `sandbox fetch` keeps the comparison against the launch seed.
- **The orchestrator is a single point.** Every branch passes through it. That
  is the price of an isolation model where no instance can reach another.
- **A returned `ready` card needs a revert.** Peer review has passed by then, so
  this is rare.

## Test Cases

Acceptance checks, by facet:

- **Nothing lands before review.** A card that reaches `needs-review` puts no
  commit in the owner's history. It qualifies when the owner's history is
  unchanged between `in-progress` and `ready`.

- **A bounce costs no landing.** A card that bounces twice and then passes adds
  one commit. It qualifies when the owner's history grows by exactly one commit
  for that card.

- **A landed commit is the owner's.** The commit carries the owner's name and a
  valid signature. It qualifies when the author is the owner and
  `git verify-commit` reports a good signature.

- **The branch keeps the writer's name.** The guest commits on the branch still
  carry the agent identity after the card lands. It qualifies when the branch
  tip's author is unchanged by the landing.

- **Landing rewrites nothing.** The landing runs with no metadata edit and no
  immutability override. It qualifies when the operation log holds one create
  and no rewrite.

- **One card, one commit.** A branch with three commits lands as one. It
  qualifies when the landed commit's prefix matches the card `type` and no
  review-round commit reaches the owner's history.

- **A bounced branch lands whole.** A card that bounced adds every file its
  branch changed, not only the files of its last commit. It qualifies when a
  two-commit branch lands both changes in one commit.

- **A resumed instance sees the current tip.** An instance paused before another
  card landed sees that work after it resumes. It qualifies when the guest can
  read the landed change in its own clone.

- **Moving a branch is a command.** The orchestrator gives a reviewer the branch
  under review without a raw `git push`. It qualifies when one documented
  command puts the branch in the reviewer's mirror.

- **A reviewer reads the branch.** The reviewer's workspace holds the branch
  under review. It qualifies when the reviewer can name the branch tip and read
  its commits.

- **Reconciliation is visible.** A branch that does not apply cleanly is
  reconciled by the orchestrator, and the card records what changed. It
  qualifies when the card names the reconciliation and says whether the card
  returned to review.

- **The board rule reads correctly.** The skills say the board is
  orchestrator-only state, not host-only state. It qualifies when no skill
  couples the role to the host machine.

## References

- [ASD-STE100 Simplified Technical English](https://asd-ste100.org) — the
  controlled language this document is written in.
- [Jujutsu: set of immutable commits](https://docs.jj-vcs.dev/latest/config/#set-of-immutable-commits)
  — why a published commit refuses a rewrite.
- [git-cherry-pick](https://git-scm.com/docs/git-cherry-pick) — the `-n` form
  this design lands with.
- [git-commit](https://git-scm.com/docs/git-commit) — `--reset-author` and `-C`.
- [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) — the
  prefix the card `type` maps to.
- PDD003 Sandbox Heartbeats and Work State — the work state this design leaves
  unchanged.
