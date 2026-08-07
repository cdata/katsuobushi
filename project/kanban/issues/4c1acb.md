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

