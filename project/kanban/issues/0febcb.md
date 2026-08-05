---
id: 0febcb
title: nixdb seed verdict can land on the guest rootfs when the 9p share fails to mount
type: bug
blocked_by: []
labels: [sandbox]
created: 2026-08-05T06:53:01Z
---

## What happened

`katsuobushi-nixdb` (`lib/sandbox/default.nix`) is ordered `after` the share
mount but is not conditioned on it. The share is mounted `nofail`, so if the
mount fails the unit still runs and its `verdict` helper writes
`<shareMount>/nixdb-status` into the **empty mountpoint on the guest rootfs**.

The host then sees no `nixdb-status` at all and renders no `store db:` line, so
a guest that failed to get its share reads exactly like one that predates the
verdict feature — the silent-unseeded-guest case card e2e44b set out to close.

## Suggested fix

Add `unitConfig.ConditionPathIsMountPoint = shareMount` so the unit is skipped
honestly rather than writing into the void, and/or have the guest log loudly
when the share is absent.

## Acceptance criteria

- [ ] A guest whose share failed to mount does not write a verdict to its own
      rootfs.
- [ ] That failure is distinguishable, host-side or in the console, from a
      guest that simply has no snapshot.

