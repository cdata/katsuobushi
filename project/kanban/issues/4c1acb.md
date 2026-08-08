---
id: 4c1acb
title: Document heartbeat authoring and the work-state vocabulary in the sandbox skill
type: docs
blocked_by: [9689a8, 5e545d]
labels: [PDD003, sandbox]
created: 2026-08-07T21:14:10Z
---

## What to build

The `sandbox` skill is where a person learns what a VM is telling them. It must
teach two new things.

**How to author a heartbeat.** Where the directory lives
(`.katsuobushi/heartbeats/` in the workspace repo), the five fields, the
duration forms, and the rules that close the gaps:

- A heartbeat runs as the agent user, with the workspace as its working
  directory.
- A check that outlives its interval is killed and does not beat.
- Duration counts from the first beat of an unbroken run, so an author writes a
  check that does not flicker. A check on a process group survives a single
  process exit. A check on one process identifier does not.
- The directory is version-controlled and reaches the VM through the ordinary
  git seed. A person reviews every heartbeat, and a forged one appears in the
  diff. An agent can write the file, but it cannot do so quietly.

**How to read the state.** The four readings — `finished`, `active`,
`active (late)`, `idle` — and what a person does about each. `ended-unreported`
leaves the vocabulary and must leave the docs with it.

The skill's current "looks stuck" guidance points at `console.log`, which stops
at the login prompt and says nothing about agent runtime. Point it at the work
state instead.

## Acceptance criteria

- [ ] The `sandbox` skill documents the heartbeat file format, its five fields,
      and the interval, flicker and kill rules.
- [ ] The skill documents the four work-state readings and what a person does
      about each.
- [ ] The skill's "looks stuck" guidance points at the work state, not at
      `console.log`.
- [ ] `ended-unreported` and `progressStallSecs` appear in no skill text.
- [ ] The `project-orchestration` skill's references to a VM's reported state
      use the new vocabulary.


## Dispatch log

- implementor: `card-4c1acb-b562aa17`

Round 1 — implementor reported `done`. Added an "Authoring heartbeats" section
to the sandbox skill covering the five YAML fields, the two duration forms, and
the kill-on-timeout, flicker and process-group rules. Added a "Reading the work
state" table for all four readings (Active, Active (Late), Finished, Idle) with
per-reading guidance. Repointed the "looks stuck" guidance at the `WORK` column
of `sandbox status` rather than `console.log`, which stops at the login prompt.
Removed `progressStallSecs` from the `lib.sandbox` example. Updated the
`project-orchestration` skill's "Trust the branch" gotcha to use `Idle`
work-state language in place of the retired `ended-unreported` phase label.

Landed on the `c5c2c2` tip with no conflict; `rust test` green on the merged
tree.

## Review notes

### Round 1 — `review-4c1acb-08f5e022` — VERDICT: needs-changes

`markdown lint` passes. No Rust changed, so the Rust gates were correctly not
required — the reviewer said so rather than skipping silently, and read
`heartbeat.rs` as the source of truth for checking the documented contract.

**The field-by-field contract check passed.** Required (`label`, `timeout`,
`check`) vs optional (`interval`, `detail`) matches `heartbeat.rs:106-129`; the
10s interval default matches `DEFAULT_INTERVAL` (`:16`); the `s`/`m` duration
forms match `parse_duration` (`:60-82`) including the "no other units" claim;
and the shell-body/first-stdout-line description of `check` and `detail`
matches `CheckOutcome::Beat`.

**BLOCKING — the two traps that bite a first-time author are missing.** Both are
documented in the shipped heartbeat files but not in the skill, which is what an
author reads *before* writing their first one.

1. **The block-scalar trap.** The example shows correct `|` syntax but never
   says why the indentation is there, that the parser strips it before `sh(1)`
   sees it, or what breaks otherwise. The shipped files carry an explicit
   four-line comment about exactly this, and call it "the usual source of a
   check that passes and means nothing" — a misindented or folded (`>`) body
   can collapse into a no-op that always exits 0.
2. **The first-beat problem.** Both shipped files deliberately write a baseline
   and `exit 1` on their first run, with a comment saying so. The skill mentions
   none of it. An author writing any comparison-type check — CPU, a counter, an
   mtime — will get a **spurious beat on the first tick**, because any value
   compares favourably against nothing. The `pgrep` example dodges the problem
   by nature rather than teaching the pattern.

**Non-blocking, to fix in the same pass:**

3. **The work-state table reads `Active (Late)` as a fourth row**, beside
   `Active`, `Finished` and `Idle`. `WorkState` has three variants and the code
   comment is emphatic that late is a flag on `Active`. A one-line note would
   resolve the visual contradiction.
4. **A nuance was lost in the "Trust the branch" rewrite.** The core warning and
   recovery path survive verbatim. But the old `ended-unreported` label
   *encoded* that nudges had already run, so seeing it meant the agent had
   ignored them. `Idle` is a live reading that is equally true *before* any
   nudge, so "a persistent `Idle` reading after those nudges" gestures at the
   distinction without giving the operator the decision boundary the old label
   did.

**Informational:** the skill does not say what happens to a file that will not
parse — one error, then silence for that path, with other files unaffected.

**Cleared:** the "looks stuck" repoint is accurate — the `WORK` column is real,
`console.log` genuinely stops at the login prompt, and the surrounding
troubleshooting sequence (provision.log → console.log → WORK column) remains
coherent.

### Round 2 — implementor revision

Added a "Block-scalar syntax" paragraph explaining that `|` is required (not
`>`), that the two-space indent is YAML structure stripped before `sh(1)` sees
the body, and that a folded `>` body silently exits 0 every tick. Added a
"Comparison-type checks and the first-beat problem" paragraph with the
baseline-and-`exit 1` pattern and a concrete `stat` mtime example beside the
retained `pgrep` one, so an author sees both shapes. Collapsed `Active (Late)`
from its own table row into a parenthetical on the `Active` row, matching
`WorkState`'s three-variant model. Replaced the vague "persistent `Idle`"
guidance with a pointer to the `N/M` nudge counter. Added a one-sentence
parse-error isolation note.

`markdown format` and `markdown lint` clean. No Rust changed, and the
implementor said so rather than skipping the Rust gates silently.

### Round 2 — `review-4c1acb-08f5e022` — VERDICT: needs-changes

`markdown lint` passes; no `project/kanban/` changes; and the reviewer judged the
section **not** overcorrected — the additions are "tight and on-point", which was
the risk in asking for two new paragraphs.

**Resolved:**

- **Finding 1 (block-scalar trap).** Explains the rule, the mechanism *and* the
  consequence, which is what the original omission lacked.
- **Finding 3 (`Active (Late)` collapse).** The three-state table is correct and
  the per-reading late guidance survived the collapse.
- **Finding 4 (nudge counter).** The counter is real — source renders
  `Idle — nudged N/M` and a test asserts `Idle — nudged 3/5` — and the core
  branch-inspection warning is intact. Minor: the docs say `N/M` where the
  display is `Idle — nudged N/M`. Non-blocking.

**BLOCKING — the new worked example is broken, in the way the section warns
about.** The prose on the first-beat problem is correct, but the `stat` mtime
example writes its baseline to `prev=/run/myproject/hb-build.prev`. That
directory is root-owned and the agent user cannot create it, so the redirect
fails **silently**, the baseline is never written, and the heartbeat never beats.

That is precisely the class of silent failure the section exists to teach people
to avoid — a check that runs, reports nothing useful, and gives the author no
signal. An incorrect worked example here is worse than the original omission,
because a reader will copy it.
