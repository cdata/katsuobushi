---
id: acb53d
title: "sandbox fetch: land into a per-instance tracking ref"
type: feature
blocked_by: []
labels: [PDD001]
created: 2026-08-06T18:06:50Z
disposition: accepted
disposition_at: 2026-08-06T20:04:54Z
---

## What to build

Make `sandbox fetch` land a guest's branch into a per-instance tracking ref the
host never rebases, so a second fetch of an already-landed instance no longer
fails non-fast-forward. That second fetch is the shape of every review bounce.

- The fetch lands into `refs/katsuobushi/<inst>` instead of the local branch
  `refs/heads/sandbox/<inst>`. The host's landing work moves its own local
  branch as before; the fetch always writes the tracking ref, so a repeated
  fetch never collides.
- The `landed` probe compares the fetched tip to the launch seed by reading the
  **tracking ref**, not the local branch.

### Seam (verified against the tree)

- Fetch refspec (`fetch.rs:36`): the remote (mirror) side stays `sandbox/<inst>`
  — that is what the guest pushes into `sync.git`. Only the local destination
  changes: `format!("sandbox/{inst}:refs/katsuobushi/{inst}")`. Update the
  docstring at `fetch.rs:28-30`.
- `landed` probe (`fetch.rs:66-76`): `git rev-parse` must target
  `refs/katsuobushi/{inst}` (was `sandbox/{inst}`). Both readers — refspec and
  probe — change together; if only one changes, the "no committed work landed"
  warning reads the wrong ref and false-alarms.
- Output/label strings (`fetch.rs:41,52,53,55`) and the tests
  (`fetch.rs:155,167,173,198,238,253,268,289`) assert the old ref — update them.
- **Verify, do not assume:** `status.rs:698` probes
  `refs/heads/sandbox/<name>` in the instance's **bare mirror** (`sync.git`),
  which is the guest's push target and is unchanged by this card. Confirm the
  status existence-probe and display (`status.rs:533`) still read the mirror
  ref, and leave them alone unless a test proves otherwise. The guest push
  target (`start.rs`, `lib/sandbox/default.nix:1626`) is likewise unchanged.

## Acceptance criteria

- [ ] `sandbox fetch` lands the branch into `refs/katsuobushi/<inst>`.
- [ ] Fetching the same instance twice does not fail non-fast-forward.
- [ ] The `landed` probe reads the tip from the tracking ref.
- [ ] `sandbox status` still reports branch existence correctly (mirror probe
      unaffected).
- [ ] Unit tests, BDD-named: `it_fetches_a_branch_into_the_instance_tracking_ref`,
      `it_fetches_the_same_instance_twice_without_non_fast_forward`,
      `it_reads_the_landed_probe_from_the_tracking_ref`.
