---
id: 1eccce
title: Nothing reads the alignment manifest's schema field
type: chore
blocked_by: []
design: warm-artifacts
labels: [rust]
created: 2026-08-05T21:29:10Z
---

## What to build

The deps-bundle manifest carries `schema` (now 2), but `checkArtifactAlignment`
never reads it — `field()` defaults every missing key to `""`. Older bundles
therefore degrade silently, which is fine today but means a future schema that
*removes* or repurposes a field would mis-verdict rather than refuse.

Make the checker assert the schema it understands and fail loudly (exit 2,
"rebuild the bundle") on one it does not.

## Acceptance criteria

- [ ] Checker refuses an unknown/newer schema rather than guessing
- [ ] A schema-1 bundle still gets a clear, actionable message
- [ ] The accepted range is stated in one place next to the emitter

