# Migrating Katsuobushi

Katsuobushi is versioned with Git tags (SemVer); pin a release as
`github:cdata/katsuobushi/v0.1.0`. While in `0.x`, any release may break.

Each version heading below covers the changes **from the version immediately
beneath it up to that version**. The top heading is the current release. `0.1.0`
is the first tagged release, so it covers everything up to the first tag — i.e.
the changes anyone tracking untagged `main` should know about.

## Unreleased

## 0.5.0

**`progressStallSecs` is removed — delete it from your `lib.sandbox` call.**
Leaving it in place is a hard Nix evaluation error
(`called with unexpected argument`), not a silent no-op.

The argument drove a host-side "no reports" notice that no longer fires. Watch
the **WORK** column of `sandbox status` instead: `Active` means work runs;
`Active (Late)` means work runs past its own declared bound; `Idle` means
nothing runs; `Finished` means the agent reported a terminal result.

**Work now lands at `ready`, not at `done` — if you drive a board with agents,
your report bridge changes.**

On `done`, fetch the branch, confirm real commits landed, and move the card to
`needs-review`. Do not land. Landing is the step that moves a card to `ready`,
after a peer review accepts. A bounced card therefore costs no landing, and a
card that bounced four times still contributes exactly one commit.

**Landing is now a squash commit. Stop re-authoring.**

Replace the duplicate-then-repair procedure with:

```sh
git merge --squash sandbox-guest/<name>
git commit -m "<type>(<scope>): <card title>"
```

The message comes from the card, not from a commit on the branch. The commit is
the owner's by construction, so `--reset-author` and
`jj metaedit --update-author` are no longer needed — and the rule that
re-attribution had to happen before the next dispatch is gone with them.

Do **not** land with `git cherry-pick <branch-tip>`: on a bounced branch that
applies the review fix alone and silently drops the original work.

In a **colocated jj repo**, git `HEAD` tracks `@-` and jj's working-copy content
is staged in git's index, so `git merge --squash` followed by `git commit` will
sweep that content into the landing commit. Run `jj new` first to get an empty
`@`, then use the git recipe.

**Giving a reviewer a branch is now a command.**

```sh
sandbox fetch card-<id-instance>
sandbox deliver review-<id-instance> --branch sandbox-guest/card-<id-instance>
```

It arrives in the reviewer's mirror as `refs/heads/delivered/<basename>`, which
the guest reads as `origin/delivered/<basename>`. Replace any raw `git push` at
another instance's storage path with this.

**`sandbox deliver` and `sandbox prune` are now reachable from the `sandbox`
command.** Both existed in `katsuctl` but were missing from the wrapper's
subcommand list. A new `checks.sandbox-verb-coverage` flake check fails the
build if the two lists diverge again.

**`sandbox fetch` now writes to `refs/remotes/sandbox-guest/<inst>` — read that
ref instead of `sandbox/<inst>` if you scripted against the old location.**

The fetch command moved from `+sandbox/<inst>:refs/heads/sandbox/<inst>` (the
0.4.1 scheme) to `+sandbox/<inst>:refs/remotes/sandbox-guest/<inst>`. On a
colocated jj repo, `jj log -r '<inst>@sandbox-guest'` resolves the ref as
before.

**Old `refs/heads/sandbox/*` refs are left behind — they are harmless and safe
to delete.**

The first fetch after upgrading creates the new ref but does not remove the old
one. The `sandbox fetch` guard now uses strict descent (not containment) when
checking for local branches that descend from the guest tip, so a stale
`refs/heads/sandbox/<inst>` pointing at pure guest history no longer blocks
subsequent fetches. Delete it at any time:

```sh
git branch -D sandbox/<inst>
```

No spec or instance-state change (`specVersion 4` / `instanceVersion 2`
unchanged).

## 0.4.1

**`sandbox fetch` moves back to `refs/heads/sandbox/<inst>` — this reverses the
0.4.0 note below.** 0.4.0 landed the guest branch in `refs/katsuobushi/<inst>`,
a ref namespace Jujutsu does not import, so a colocated jj repo could not reach
the fetched work from any revset. 0.4.1 fetches
`+sandbox/<inst>:refs/heads/sandbox/<inst>` instead. The leading `+` (force)
keeps a refetch idempotent, and `refs/heads/` is a namespace jj imports. If you
scripted against `refs/katsuobushi/<inst>` after 0.4.0, read
`refs/heads/sandbox/<inst>` (or its short form `sandbox/<inst>`) now.

**If you cannot upgrade off 0.4.0 yet**, bridge each fetch into a namespace jj
imports:

```sh
git branch -f sandbox/<instance> refs/katsuobushi/<instance>
jj git import
```

No spec or instance-state change (`specVersion 4` / `instanceVersion 2`
unchanged).

## 0.4.0

**Retire the `design:` field: run `project lint --fix` once.** The `design:`
note frontmatter field is removed, and the card face drops its `design` column.
If any card note still carries `design: <ref>`, run `project lint --fix`. It
folds the reference into the card's `labels`, drops the `design:` key, and drops
the `design` metadata-key from the board settings block. The migration is
idempotent and byte-stable — a second run reports no change. A plain
`project lint` reports each legacy field as an `info` line until you run the
fix. No spec or instance-state change (`specVersion 4` / `instanceVersion 2`
unchanged).

**`project new --design` is deprecated.** It still works, but it now prints a
deprecation warning and records the reference as a label instead of a `design:`
field. Move scripts to `--label` (for example `--label PDD007`). The old
`--labels` spelling stays as an alias for the new `--label`.

**`sandbox fetch` lands into `refs/katsuobushi/<inst>`.** The host used to fetch
the guest branch into a local `sandbox/<inst>` branch and rebase it; it now
fetches into a per-instance tracking ref, `refs/katsuobushi/<inst>`, that it
never rebases. If you scripted against the local `sandbox/<inst>` branch that
`sandbox fetch` wrote, read the tracking ref now. The guest push target
(`sandbox/<inst>` in the instance's `sync.git` mirror) and the `sandbox status`
branch probe are unchanged, so a repeated fetch of an already-landed instance no
longer fails non-fast-forward.

**`project lint` no longer warns on an orphan note.** A note with no card on the
board is the icebox now, which is a normal state. `lint` reports it as an `info`
inventory line and exits `0`. If you gated anything on the `orphan-note`
warning, that signal moved to `info: <n> note(s) in icebox`.

**Everything else is additive.** The `--label` filter, `project labels`, the
icebox verbs (`new --icebox`, `status --icebox`, `status set <id> icebox`), and
the three new skills (`design`, `plan`, `implement`) add surface without
changing existing behavior. No action required for these.

## 0.3.8

**Expect one deps-bundle rebuild.** `lib.rust`'s deps-only derivations now emit
an alignment `manifest.json`, which changes their derivation hash. The bundled
`target.tar.zst` is unchanged in content and crane ignores the new file, so
nothing behaves differently — but the first build after upgrading recompiles the
dependency closure once. No action required.

**A new evaluation warning may fire.** If your project has a
`.cargo/config.toml` and your `sourceInclude` does not carry `.cargo` into the
Nix source, `lib.rust` now warns. It is telling you something real: those flags
apply to your dev shell but not to anything Nix builds, so cargo silently
discards Nix-built artifacts and rebuilds. Add `".cargo"` to `sourceInclude` (it
is in the default set) or drop the config file. Projects using the default
`sourceInclude`, or with no cargo config, see nothing.

**New export: `checkArtifactAlignment`.** Additive; nothing existing changes.
Wire it into your menu if you want an answer to "why is this rebuilding?":

```nix
rust = katsuobushi.lib.rust { inherit pkgs; /* … */ };
# rust.checkArtifactAlignment → katsuobushi-check-artifact-alignment
```

Exit codes are **0** aligned, **1** misaligned with the field named, **2**
unknown (no manifest / unparseable cargo config / broken check). A `2` is not a
mismatch — do not treat it as one.

**If you drive agents into sandboxes, check your menu.** The guest contract now
instructs dispatched agents to run `nix develop -c menu` and prefer the
project's own build/test commands, reporting a gap when the menu has none. A
project whose menu lists only housekeeping (format, lint) will see agents
legitimately fall back to raw `cargo`/`nix build` — the fix is to give the
project real menu commands rather than to write build instructions into
`.dispatch-instructions.md`.

## 0.3.7

**Docs only — no action required.** No tooling, spec, or instance-state change
(`specVersion 4` / `instanceVersion 2` unchanged); the `project-orchestration`
skill is the only file that moved.

One consequence worth knowing if you drive a board with agents: an orchestrator
following the updated skill now **pauses** a card's implementor and reviewer VMs
(`sandbox stop <name>`) at `needs-review` and removes them only once the card
reaches `ready`, instead of removing them as soon as the branch lands. That is
deliberate — a resumed instance skips the cold rebuild that otherwise
front-loads every round of review feedback — but it means paused instances hold
their `storeVolumeSize` + `scratchVolumeSize` volumes on disk for the length of
the review loop rather than releasing them at `needs-review`. `sandbox status`
lists what is stopped; `sandbox stop --remove <name>` reclaims it.

## 0.3.6

**`sandbox dispatch` and prompted `sandbox start --agent` now wait for a
terminal report by default.** Previously they returned as soon as the agent
yielded, even without a `report done`/`blocked`. If you relied on that early
return — a script that treats the command exiting as "the turn is over, move on"
— add **`--no-until-report`** to restore it. Everything else needs no change:
`--until-report` still parses on both commands (it is now a no-op), so existing
invocations that passed it behave identically, and this is the behavior they
were asking for anyway.

Practical effect if you do nothing: a dispatch that used to return early on a
silent stop now keeps waiting, and ends only on a terminal report, transport
death, or delivery exhaustion. That is usually what you want — but a wrapper
with its own timeout around `dispatch` may now hit that timeout where it
previously returned. Interactive `sandbox prompt` is unchanged.

**The sandbox progress notice changed wording and now fires up to twice.**
Nothing to do unless you scrape it: the first notice reads
`no reports for Ns — normal during long builds …` (it no longer claims the agent
may be stuck), and a second, stronger one fires only if the silence reaches 3×
the window. Neither breaks the turn, and the default window is still 300s. If
that notice fires on every launch of your project — it will, if a cold build is
minutes long — set the new `progressStallSecs` argument past your build time:

```nix
sandbox = katsuobushi.lib.sandbox { inherit pkgs; …; progressStallSecs = 1500; };
```

**`sandbox status` can now report a third state: `provisioning`.** Anything
parsing the `--json` `state` field must handle it alongside `running` and
`stopped` — an instance that is still being provisioned previously reported
`stopped`, so a consumer treating "not running" as "dead" will now see a live
launch it used to misread (which is the point). The detail view and `--json`
also gain an optional `phase` (the provisioning step in flight), `storeDb` (the
guest's host-Nix-DB seed verdict) and `lastReport` (the latest terminal report
object, from the new `reports.ndjson`); all three are omitted when unknown, so
existing parsers are unaffected. Each instance's state dir gains a
`provision.log` and, transiently, a `phase` file.

**The guest Nix-DB seeding service now always runs.** It was gated on
`ConditionPathExists` for the snapshot, so a missing snapshot skipped the unit
silently; it now runs unconditionally and records its verdict to `nixdb-status`
in the share, exiting early (and logging loudly) when no snapshot was staged.
Seeding behavior itself is unchanged — only the reporting is new.

**Check your board for cards a `markdown format` run already blanked.** If a
project wired both `lib.markdown` and `lib.project`, Prettier reflowed any
frontmatter value longer than the print width onto continuation lines, and the
note reader took the result as absent — silently emptying long card titles and
long `blocked_by` lists. The reader now folds those values back, so **existing
reflowed notes heal on their own with no migration**. What does not heal is a
card someone _re-saved_ while its title read as blank. Run `project lint`: the
new `empty-title` error names any card whose title no longer parses, and it is
an error, so a `project-lint` flake check that was green can now fail. Restore
those titles from history (`git log -p <boardDir>/issues/<id>.md`) and re-run.

**`lib.markdown`'s `exclude` now works — check yours.** Prettier resolves
`--ignore-path` patterns relative to the directory holding the ignore file, and
that file was generated into the Nix store, so every workspace-relative
`exclude` entry silently matched nothing. Both the `<name> format`/`lint`
commands and the flake check now stage the ignore file at the workspace root, so
`exclude` behaves as its documentation always claimed. **If you passed `exclude`
and it appeared to have no effect, those files are now genuinely skipped** — a
gate that was quietly formatting/checking them will change behavior. Consumers
that never passed `exclude` are unaffected.

The staged file is named `.prettierignore.<name>` at the repo root and is
removed when the command exits (including on Ctrl-C). It is namespaced by the
configuration's `name`, so it never collides with a consumer's own
`.prettierignore` or with a second `lib.markdown` invocation.

**Keep `BOARD.md` out of the gate if you use `lib.project`.** Opening the board
in Obsidian rewrites it into the Kanban plugin's serialization, which Prettier
rejects and which the plugin cannot be configured away from — so reading the
board could fail CI. Wire the new export:

```nix
markdown = katsuobushi.lib.markdown {
  inherit pkgs;
  workspaceRoot = ./.;
  exclude = project.markdownExclude; # ⇒ [ "<boardDir>/BOARD.md" ]
};
```

If your `include` already avoided the board (e.g. an explicit file list), this
is belt-and-braces; if it globbed `project/kanban/**` or defaulted to `**/*.md`,
it is the fix. Card notes stay gated either way.

## 0.3.5

**`lib.rust` gains per-helper build profiles — additive, no action required.**
Every build helper now takes an optional `profile` (default `"release"`), so
existing calls behave exactly as before — the release build derivations are
byte-identical. Pass `profile = "dev"` to build a crate and its dependency
bundle unoptimized (Cargo's unoptimized profile is spelled `"dev"`; `"debug"` is
a reserved name). The shared workspace-deps bundle is now keyed by
`(target, profile)`, so distinct profiles get distinct bundles rather than
colliding.

**Default `wasm-bindgen-cli` is now `0.2.126` (was `0.2.108`).** If your
`Cargo.lock` resolves `wasm-bindgen` to `0.2.108` and you relied on the bundled
hashes, either bump your workspace to `0.2.126` or add your own entry —
`wasmBindgenHashes."0.2.108" = { hash = …; cargoHash = …; }`. Consumers that
already pass their own `wasmBindgenHashes`, or whose lock resolves `0.2.126`,
need no change.

**`project status` hides archived cards after 1h (was 24h).** The human
(non-JSON) board list now drops accepted and cancelled cards an hour after their
`disposition_at`, rather than a day. If you rely on `project status` to review
recently completed work, scan it more often or use `--json`, which still returns
every archived card regardless of age.

## 0.3.4

**Board reformat on first write — no action required, but expect a one-time
diff.** No spec or instance-state change (`specVersion 4` / `instanceVersion 2`
unchanged). The `project` board writer now emits prettier-stable markdown: the
archive separator is `---` (was `***`), empty lanes lose a redundant blank line,
and the settings block gains blank lines around its code fence. The first CLI
mutation (or `project init`) rewrites an existing `BOARD.md` into this form
once; thereafter a CLI rewrite and `markdown format` agree byte-for-byte, ending
the format-drift churn. Archive parsing is now anchored on the `## Archive`
heading, so a board whose separator a formatter rewrote — and any duplicate
`## Archive` sections a prior version appended — heal automatically on the next
write.

**New `project lint` findings.** Duplicate lane headings are now a hard error
(`duplicate-lane`). A card in an unrecognized lane (`unrecognized-lane`) or a
checked `- [x]` card left in an active lane (`checked-in-lane`) are warnings, so
they surface without failing the `lint` gate — a board that deliberately keeps
an extra lane (an "Icebox") still lints clean of errors.

**Sandbox in-guest builds: size `scratchVolumeSize` for build trees.** The
nix-daemon's build directory now lives on the disk-backed scratch volume
(`/scratch/nix-build`) instead of the RAM-backed root tmpfs, so a derivation
whose build tree exceeds ~`mem/2` no longer fails with `ENOSPC` while every
provisioned volume reads nearly empty. If you run large in-guest `nix build`s
(e.g. a Bevy-scale `cargo test --no-run`), raise `scratchVolumeSize` to cover
the transient tree on top of the cargo/rustup/XDG caches — the images are sparse
and discard-trimmed, so a generous cap costs only real usage. On a Nix older
than 2.22 (before `build-dir`), the daemon's `TMPDIR` is pinned to the same
volume as the fallback.

## 0.3.3

**Mostly additive — no action required for existing consumers.** No spec or
instance-state change (`specVersion 4` / `instanceVersion 2` unchanged); the new
knobs are guest-only agent env vars and the new flag is host-only CLI, so a
consumer who overrides neither behaves as before.

**Behavioral change to note (agent mode):** a sandbox agent that ends a turn
without a terminal `report done`/`blocked` is no longer resolved immediately.
The guest now **auto-nudges** it — re-prompting "report your real state" up to
`maxNudges` times (default 3), `nudgeIntervalMs` apart (default 30s) — before
resolving the turn as `ended-unreported`. So a silent stop takes up to
~`maxNudges × nudgeIntervalMs` longer to reach that terminal state than before,
and the agent may receive extra channel turns. `maxNudges`/`nudgeIntervalMs` are
internal `lib.sandbox` liveness tunables (alongside `stopGraceMs`);
`maxNudges = 0` disables nudging entirely (the prior single-grace behavior).

**New, opt-in:** `sandbox prompt`/`dispatch`/`start` accept `--until-report`,
which keeps the host stream armed across an unreported turn-end (waiting for a
real terminal report) instead of returning with the "stopped without reporting"
warning. Off by default — existing invocations are unchanged.

## 0.3.2

**Docs only — no action required.** No spec or instance-state change
(`specVersion 4` / `instanceVersion 2` unchanged). Agent-facing guidance for the
project board now points at the `project` menu command instead of the underlying
`katsuctl project` binary; `project` was always the intended interface (it
forwards `--json` through untouched), so behavior is unchanged. Boards
scaffolded before this release are unaffected — their `project/kanban/README.md`
still mentions `katsuctl` cosmetically; edit that one line if you want the
updated wording (`project init` won't overwrite it).

## 0.3.1

**Mostly additive.** No spec or instance-state change (`specVersion 4` /
`instanceVersion 2` unchanged). `katsuctl` now builds on non-Linux (macOS) with
the `project` board commands; the `sandbox` domain is Linux-only and absent
there — nothing changes if you were already on Linux.

**One behavioral change to note:** the live human reports streamed by
`sandbox prompt` / `sandbox dispatch` now go to **stderr** instead of stdout, so
they are reliably captured in a backgrounded / non-TTY context. If you scraped a
driven turn's _human_ report text from **stdout**, read **stderr** instead.
Machine consumers are unaffected — `--json` streaming stays on stdout.

## 0.3.0

**Additive — no action required for existing consumers.** No spec or
instance-state change (`specVersion 4` / `instanceVersion 2` unchanged); the
`sandbox`, `menu`, `rust`, and `markdown` libraries are unchanged, and
`sandbox dispatch` is a new subcommand rather than a change to an existing one.

New this release: a **project board** (`lib.project` + the `project` menu
command) and orchestration on top of it. To adopt it, wire `lib.project` into
your flake and run `project init` to scaffold `project/kanban/`; the `project`
and `project-orchestration` skills document the workflow. Nothing changes if you
do not opt in.

## 0.2.9

**Action required: rebuild your dev shell** (`nix develop`) to pick up the fixed
controller. No config, spec, or instance-state change (`specVersion 4` /
`instanceVersion 2` unchanged).

A bugfix release: `sandbox prompt` can once again resume a paused, **named**
instance to deliver a turn. The 0.2.6 command-tree rename removed the
`sandbox:start` menu binary, but the auto-resume path kept invoking that name,
so prompting a powered-off named instance failed. The fix is entirely host-side,
so a dev-shell rebuild is all that is needed — running instances are unaffected.
If anything you own scraped a `sandbox:*` name out of a hint or error message,
note those lines now print the subcommand form (`sandbox status`, not
`sandbox:status`).

## 0.2.8

**Action required: rebuild your dev shell.** No config, spec, or instance-state
change (`specVersion 4` / `instanceVersion 2` unchanged) — just rebuild
(`nix develop`) to pick up the new command.

Every dev shell now gains a built-in `menu` command that reprints the command
table (handy after the screen scrolls). It is added automatically by `makeMenu`;
if you already define a command named `menu`, yours still wins. Nothing else
changes — existing commands, their banners, and the drop-in greeting are
unchanged.

## 0.2.7

**Action required: rebuild your dev shell.** No config, spec, or instance-state
change (`specVersion 4` / `instanceVersion 2` unchanged) — just rebuild
(`nix develop`) to pick up the menu fixes.

One behavioral change worth knowing: **menu decoration now goes to stderr.** The
dev-shell greeting and every menu command's figlet banner previously printed to
stdout; they now print to stderr. This is what lets a captured or piped menu
command keep clean output — e.g. `nix develop -c 'sandbox status --json' | jq`
now sees only the JSON, where before the greeting could land in the pipe. If you
have anything that scraped the greeting or a banner from a command's **stdout**,
read it from stderr instead. The greeting still displays on an interactive
terminal (it is not gated on interactivity — it always shows, just on stderr).

## 0.2.6

**Action required: rebuild your dev shell, and rename any calls to the menu
commands.**

Dev-shell menu commands are now subcommand trees: a namespace is a single
command with subcommands rather than one command per verb. This is purely a
menu/command-wiring change — there is **no spec or instance-state bump**
(`specVersion 4` / `instanceVersion 2` unchanged) and no change to what any
command does — but the command _names_ change, which breaks a script, CI step,
or muscle memory that calls the old colon-namespaced names.

Rebuild your dev shell (`nix develop`) to pick up the renamed commands, then
update call sites:

| Before                             | After                              |
| ---------------------------------- | ---------------------------------- |
| `sandbox:start`                    | `sandbox start`                    |
| `sandbox:prompt <inst> "…"`        | `sandbox prompt <inst> "…"`        |
| `sandbox:status [inst]`            | `sandbox status [inst]`            |
| `sandbox:attach <inst>`            | `sandbox attach <inst>`            |
| `sandbox:fetch <inst>`             | `sandbox fetch <inst>`             |
| `sandbox:stop [--remove] <inst>`   | `sandbox stop [--remove] <inst>`   |
| `sandbox:screenshot <inst> [path]` | `sandbox screenshot <inst> [path]` |
| `format:<name>`                    | `<name> format`                    |
| `lint:<name>`                      | `<name> lint`                      |

A bare `sandbox` (or `sandbox -h`) now prints the subcommand list, and
`nix run .#sandbox` is unchanged. Sandbox usage/error text also now names the
command you typed (`sandbox attach …`) rather than the underlying
`katsuctl sandbox --config <CONFIG> attach …`.

If you build your own menu with `katsuobushi.makeMenu`, nothing forces a change:
the flat `{ description; command; }` command shape still works. Grouping is
opt-in — give an entry a `subcommands` attrset instead of a `command` to make it
a branch.

## 0.2.5

**Action required: rebuild your dev shell and restart agent instances.**

A hardening release; no spec or instance-state bump (`specVersion 4` /
`instanceVersion 2` unchanged) and **no config changes** for a correctly
configured project. Rebuild your dev shell to pick up the new controller, and
**restart any running agent instances** — the turn-delivery fixes live in the
guest image, so a VM booted under `0.2.4` keeps the old behavior until its next
start. Persistent (`--name`d) instances keep working across the upgrade; they
get the new guest on their next `sandbox:start`.

Three behavioral changes are worth knowing:

- **Eval-time validation is tighter, and can newly fail your flake.** A
  `homeFiles` entry with an unknown `mode` (e.g. a typo like `"immutible"`) now
  throws at evaluation instead of silently never appearing in the guest, and
  `homeFiles`/`extraRepos` destinations now reject every `..` traversal form. If
  your eval starts failing here, the entry was silently misconfigured before —
  the file it names was not landing in the guest.
- **`sandbox:stop` can now refuse.** If the VM's monitor keeps answering after
  `quit`, stop exits nonzero with nothing removed instead of deleting the disk
  images out from under a live qemu. Retry, or inspect the qemu process before
  discarding state.
- **`sandbox:prompt` fails loudly on a corrupt `liveness.json`.** Previously a
  corrupt record silently rewound the turn-id counter (which could drop the next
  prompt); now the prompt errors and names the file. Remove
  `<state>/<instance>/liveness.json` to start the counter over if you hit it.

Cosmetic: ephemeral instance names are now UTC-stamped (previously host-local
time), and `sandbox:status` stops showing "stream active" once heartbeats go
stale rather than trusting a leftover flag.

## 0.2.4

**Action required: rebuild your dev shell.**

The `sandbox:*` commands no longer depend on `katsuctl` being on your PATH —
they invoke it by absolute store path — so the bug where a consumer dev shell
reported `katsuctl: command not found` is fixed. **No config changes are
required**, and if you only ever used the menu commands you needed no workaround
before either.

The one thing everyone must do is rebuild: the instance spec bumps to
`specVersion 4` (it now carries the controller's own path so the agent-mode boot
recipe can self-reference it), and a stale v3 spec is rejected loudly. Run
`nix develop` (or otherwise rebuild your dev shell) so the spec re-renders.

Per-instance `instance.json` state is unchanged (still `instanceVersion 2`), so
persistent (`--name`d) instances created under `0.2.3` keep working across the
upgrade.

## 0.2.3

No action required.

## 0.2.2

**Action required: rebuild your dev shell.**

The sandbox gains an opt-in `graphics` capability (a headless compositor plus a
paravirtual GPU). It is **off by default**, so **existing consumers need no
change** — a sandbox without a `graphics` block behaves exactly as before.

The one thing everyone must do is rebuild: the instance spec bumps to
`specVersion 3`, and a stale v2 spec is now rejected loudly. Run `nix develop`
(or otherwise rebuild your dev shell) so the spec re-renders; no config changes
are required.

Per-instance state also bumps: `instance.json` is now `instanceVersion 2` (it
records the resolved graphics rung shown in `sandbox:status`). A v1 instance
state from `0.2.1` is rejected on read, so recreate any persistent (`--name`d)
instance after upgrading — ephemeral instances are unaffected.

If you _do_ enable graphics, two things are worth knowing — both covered in
[`lib/sandbox/README.md`](lib/sandbox/README.md#graphics-opt-in):

- It widens the host-facing attack surface (a GPU rung parses the guest's GPU
  command stream inside the host QEMU process). Pin `gpu = ["software"]` to keep
  the full original boundary at a performance cost.
- Set a higher resource floor yourself (`vcpu ≥ 4`, `mem ≥ 8192`) — the library
  does not auto-bump them — and ensure your uid can open a host render node (the
  `graphics` row in `sandbox:status` checks this and names the fix).

## 0.2.1

**Action required: rebuild your dev shell.**

Agent-mode sandboxes gain turn/transport liveness: heartbeats, a durable
`turn-state.json` on the share, a host watchdog with ack-and-resend and a
ready-gate, and a `sandbox:status` liveness line. It is additive, so there is
**no action for devshell users** — except that the instance spec bumps to
`specVersion 2`, and a stale v1 spec is now rejected loudly. Run `nix develop`
(or otherwise rebuild your dev shell) so the spec re-renders; no config changes
are required.

The seven liveness knobs (`heartbeatSecs`, `heartbeatMiss`, `progressStallSecs`,
`deliveryDeadlineSecs`, `deliveryRetries`, `readyGateSecs`, `stopGraceMs`) ship
with sensible defaults and need no consumer action.

## 0.2.0

**Host sandbox control is now `katsuctl` — `sandbox:*` behavior is unchanged.**

The host side of the sandbox (`sandbox:start` / `sandbox:prompt` /
`sandbox:status` / `sandbox:fetch` / `sandbox:stop` / `sandbox:attach`) is
reimplemented as a tested Rust binary, `katsuctl`, behind the **same** devshell
command names. **No action for devshell users** — the command names and behavior
are unchanged, verified end-to-end on a real boot. The win is internal: the host
logic now lives in compiled, tested Rust instead of an untested shell pile.

**The three in-tree Rust crates are renamed.**

Only relevant if your flake references these crates or their build outputs
directly:

- `katsuctl` → **`katsuobushi-controller`** — still produces the `katsuctl`
  binary, and `nix build .#katsuctl` is unchanged.
- `katsuobushi-protocol` → **`katsuobushi-sandbox-protocol`**.
- `katsuobushi-sandbox-control` → **`katsuobushi-sandbox-guest`** — its guest
  controller server binary (and the agent-mode MCP/channel server name) renames
  with it; the flake output is now `.#katsuobushi-sandbox-guest`.

If you build a specific crate via `nix build .#<crate>`, update the attribute to
the new name (except `.#katsuctl`, which is unchanged).

**`sandbox:status` no longer lists the SSH and CID columns.**

The list view (`sandbox:status` with no argument) drops the `SSH` (ssh port) and
`CID` (vsock CID) columns — they are plumbing you do not type by hand. Both
remain in the **per-instance detail view** (`sandbox:status <name>`), alongside
the ready-to-run ssh and `sandbox:prompt` commands, and in the `--json` output.
Tooling that parsed those two columns from the list table should read the detail
view or `--json` instead.

## 0.1.10

**`lib.sandbox`: writable scratch is now disk-backed — `storeOverlaySize` is
removed.**

The guest's writable scratch — the writable `/nix/store` overlay, the workspace
clone and its build artifacts, the `cargo`/`rustup`/XDG caches, and the guest
Nix database — now lives on per-instance **sparse disk images** instead of a
tmpfs. This lifts the old cap (a fraction of `mem`) that let a single Rust
`target/` exhaust guest RAM: capacity now scales with host disk, and peak RAM
tracks the working set. The guest root `/` stays a tmpfs.

**Action required only if you set `storeOverlaySize`.** That single tmpfs-size
string is gone, replaced by three image sizes (in MiB, sparse):
`storeVolumeSize` (default `16384`), `scratchVolumeSize` (default `32768`), and
`dbVolumeSize` (default `4096`). Rename and re-express in MiB — e.g.
`storeOverlaySize = "8G"` → `storeVolumeSize = 8192`. If you never set it, no
action is needed; the defaults are generous and the images are sparse, so host
disk usage tracks real content rather than these caps.

Two behavioral notes, no action:

- A **named** instance keeps its images across a stop/restart, so warm build
  caches survive a pause. As a consequence, its guest Nix database is seeded
  from the host **once** (on first launch) and then accumulates the agent's own
  in-VM registrations; a resumed instance therefore does **not** pick up host
  paths built _after_ its first launch. Discard it with `sandbox:stop --remove`
  to re-seed from a fresh host snapshot. Ephemeral instances seed every launch
  as before.
- Prompting a **paused** instance now auto-starts it (see below), so its
  disk-backed caches are warm when the work resumes.

**`sandbox:prompt` auto-starts a paused instance — no action required.**

Prompting a named instance that was stopped (but kept) now restarts it — booting
and arming the channel (~30–60s) before delivering the turn — instead of hanging
against the powered-off VM. The live conversation does not survive a pause (the
VM's RAM is gone); only the pushed branch does, so the resumed agent begins a
fresh session on top of its branch. Phrase such a prompt to stand on its own.

## 0.1.9

No action required.

## 0.1.8

**`lib.sandbox`: the guest now imports the host Nix DB by default — no action
required in normal use**

`importHostStoreDb` defaults to `true`, so a launched sandbox now snapshots the
host's Nix database and the guest reuses every path the host has already built
(e.g. a `nix develop` toolchain) instead of re-downloading it. This is
transparent: it only changes what the guest's `nix` treats as valid, adds no
read exposure (the whole host store was already mounted read-only), and falls
back to the previous system-only behavior if the snapshot is missing or a
host/guest Nix schema mismatch is detected — so a sandbox always boots.

Two things worth knowing:

- Each launch writes a ~150 MB `nix-db.sqlite` into the per-instance host state
  dir and the guest copies it in at boot. For a persistent (`--name`d) instance
  this lives alongside its other state until teardown.
- To restore the old behavior (substitute everything from the allowlisted
  caches), pass `importHostStoreDb = false`.

## 0.1.7

No action required.

## 0.1.6

No action required.

## 0.1.5

No action required.

## 0.1.4

**`lib.sandbox`: named instances are suffixed with random entropy — action only
if you script instance names**

A provided `--name foo` now boots an instance named `foo-<8 hex>` (e.g.
`foo-a3f9c2d1`) rather than `foo`. This makes every launch a fresh,
collision-free instance instead of a silent resume of an older same-named branch
— an easy footgun before. Two consequences:

- **Drive and resume by the full suffixed name.** Every other command
  (`sandbox:prompt` / `status` / `fetch` / `stop`) and a later resume key off
  the full name, not the bare `--name`. The full name is printed at launch and
  by `sandbox:stop`. If you have a script that assumes the instance name equals
  the `--name` you passed, capture and reuse the printed name instead.
- **Re-passing the bare `--name foo` mints a NEW instance**, it no longer
  resumes the old branch. To resume, relaunch with the full suffixed name. A
  name that already carries the 8-hex suffix is not re-suffixed, so passing a
  printed name back is safe and idempotent.

## 0.1.3

No action required.

## 0.1.2

No action required.

## 0.1.1

A small release: no library argument or output signatures changed, so a normal
upgrade needs no edits. The one behavioral change worth knowing is below; the
rest is additive or a bug fix (see [`CHANGELOG.md`](CHANGELOG.md)).

**`lib.sandbox`: `sandbox:status` now exits non-zero on a failed preflight —
action only if you script its exit code**

A bare `sandbox:status` now runs an environment preflight before listing
instances (it prints an `environment:` block verifying each declared secret at
its host source and checking for `/dev/vhost-vsock`) and **exits with the count
of missing prerequisites** instead of always exiting `0`. The instance listing
is unchanged.

This is a feature — the exit status is now a usable launch gate — but if you
have a script or CI step that runs a bare `sandbox:status` and treats a non-zero
exit as failure, it will now fail when a prerequisite is missing rather than
silently succeeding. Pass an explicit instance name (`sandbox:status <inst>`) to
get just that instance's details without the preflight gate.

**`lib.sandbox`: guest push to the 9p mirror now works — no action needed**

The per-instance bare mirror is now shared into the guest with
`security_model=mapped-xattr` (was `none`), so the unprivileged in-guest agent
owns the files it creates and `git push` back to the host succeeds. Previously
the push failed and no work crossed the sandbox boundary. This is a pure fix; no
consumer change is required.

## 0.1.0

The first tagged release. The notes below matter to anyone who was tracking
untagged `main`; on a fresh install there is nothing to migrate.

**Action required: transitive dependency inheritance (`lib.rust`)**

Katsuobushi now owns its infrastructure dependencies (`crane`, `nix-filter`,
`rust-overlay`, and `microvm` for the sandbox lib) and passes them through to
consumers transitively. Two consequences for `lib.rust` callers:

1. **Drop the infra inputs and arguments.** `lib.rust` no longer requires
   `crane`, `nix-filter` (`filter`), or `rust-overlay` — it inherits them from
   Katsuobushi. Your consumer flake collapses from six inputs to two (plus
   `flake-utils`):

   ```nix
   # Before
   inputs = {
     nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
     flake-utils.url = "github:numtide/flake-utils";
     katsuobushi.url = "github:cdata/katsuobushi";
     crane.url = "github:ipetkov/crane";
     nix-filter.url = "github:numtide/nix-filter";
     rust-overlay = { url = "github:oxalica/rust-overlay"; inputs.nixpkgs.follows = "nixpkgs"; };
   };
   # ...and the call threaded them:
   rustHelpers = katsuobushi.lib.rust { inherit pkgs crane; filter = nix-filter.lib; /* ... */ };

   # After
   inputs = {
     nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
     flake-utils.url = "github:numtide/flake-utils";
     katsuobushi.url = "github:cdata/katsuobushi";
     katsuobushi.inputs.nixpkgs.follows = "nixpkgs";   # unify on your nixpkgs
   };
   # ...and the call no longer names them:
   rustHelpers = katsuobushi.lib.rust { inherit pkgs; /* ... */ };
   ```

   Each infra dep remains an _optional_ argument, so you can still override one
   per-call (`katsuobushi.lib.rust { …; crane = myCrane; }`) or flake-wide
   (`inputs.katsuobushi.inputs.crane.follows = "crane";`).

2. **Drop `(import rust-overlay)` from your overlays.** `lib.rust` now applies
   rust-overlay internally, so `pkgs` is plain nixpkgs + the katsuobushi
   overlay:

   ```nix
   # Before
   pkgs = import nixpkgs { inherit system; overlays = [ (import rust-overlay) katsuobushi.overlays.default ]; };
   # After
   pkgs = import nixpkgs { inherit system; overlays = [ katsuobushi.overlays.default ]; };
   ```

**Trade-off:** menu-only consumers now pull `crane`/`microvm` into their
transitive `flake.lock` (they are not _built_ unless used, and `nixpkgs.follows`
prevents nixpkgs duplication). This is the accepted price of a dramatically
smaller consumer flake.

**New library: `lib.sandbox`**

`katsuobushi.lib.sandbox` assembles a `microvm.nix` guest that boots into a
working dev environment in which an agent harness (Claude Code by default) can
run with its blast radius bounded by a real VM. It ships born with the
transitive-dependency pattern (no legacy signature). It returns `apps.sandbox`
(`nix run .#sandbox`), `menuCommands` (`sandbox:start`, `sandbox:prompt`,
`sandbox:status`, `sandbox:fetch`, `sandbox:stop`), `checks.sandbox` (builds the
guest image), and `nixosConfiguration`. Scaffold a worked example with
`nix flake init -t github:cdata/katsuobushi#sandbox`; see
[`lib/sandbox/README.md`](lib/sandbox/README.md).

**Action required: `lib.markdown` uses Prettier, scoped by include/exclude**

`lib.markdown` switched its engine from `rumdl` to [Prettier], which handles GFM
tables natively (rumdl misidentified them). The argument and output surface
changed with it.

**Arguments.** `docsDir` is gone; scope is now two workspace-relative glob
lists, `include` and `exclude`, plus a `name` that labels this invocation's
outputs.

| Old arg                       | New arg(s)                                                  |
| ----------------------------- | ----------------------------------------------------------- |
| `docsDir`                     | `include` (globs, default `[ "**/*.md" ]`) + `name` (label) |
| —                             | `exclude` (globs → a Prettier ignore file, default `[ ]`)   |
| `settings` (rumdl rule table) | `settings` (Prettier options — different keys)              |

```nix
# Before (rumdl)
markdown = katsuobushi.lib.markdown {
  inherit pkgs;
  workspaceRoot = ./.;
  docsDir = "docs";
  settings = { MD013.line-length = 100; };
};

# After (Prettier)
markdown = katsuobushi.lib.markdown {
  inherit pkgs;
  workspaceRoot = ./.;
  name = "docs"; # labels `format:docs` / `lint:docs` and the `docs` check
  include = [ "docs" ]; # path(s)/glob(s); Prettier expands globs, honors .gitignore
  # exclude = [ "docs/vendor/**" ];
  settings = { printWidth = 100; }; # Prettier options, not rumdl rules
};
```

**`settings` is now [Prettier options][options]**, merged over the defaults
(`proseWrap = "always"`, `printWidth = 80`, `tabWidth = 2`). Translate the rumdl
rules you relied on — e.g. `MD013.line-length = 100` → `printWidth = 100`. Rules
with no Prettier equivalent simply disappear: Prettier does not flag inline HTML
or a missing top-level heading, so the `MD033` / `MD041` opt-outs some configs
needed for an HTML hero banner are **no longer necessary** — drop them.

**Default scope changed.** The old `docsDir` default was `"design"`; the new
`include` default is every tracked `.md` file (`[ "**/*.md" ]`). If you relied
on the old default to lint only `design/`, set `include = [ "design" ]`
explicitly.

**Outputs.** `rumdl` / `rumdlConfig` became `prettier` / `prettierConfig` /
`prettierIgnore` (update dev-shell `nativeBuildInputs` from `markdown.rumdl` to
`markdown.prettier`). Each invocation contributes its OWN namespaced pair of
menu commands — `format:<name>` (rewrite in place) and `lint:<name>` (read-only
check) — and its own check `checks.<name>`; there is no shared/global command.

**Behavioral notes.** Both commands run from the repository root; `include`
becomes Prettier's path arguments (everything matched is parsed as Markdown via
`--parser markdown`, so point `include` at Markdown). The check runs from the
workspace root, so every included file must be **tracked** — a flake check
cannot reach `.gitignore`'d paths, which are not part of the flake source;
format those with the menu command instead.

**Action required: `lib.rust`: renamed input arguments**

The two input-list arguments were renamed to match nixpkgs vocabulary (the old
`buildInputs` confusingly fed `nativeBuildInputs`, and `libraries` fed
`buildInputs`).

| Old arg       | New arg             | Feeds                                        |
| ------------- | ------------------- | -------------------------------------------- |
| `buildInputs` | `nativeBuildInputs` | derivation `nativeBuildInputs` (build tools) |
| `libraries`   | `buildInputs`       | derivation `buildInputs` (link libraries)    |

```nix
# Before
rustHelpers = katsuobushi.lib.rust {
  inherit pkgs crane;
  # ...
  buildInputs = with pkgs; [ pkg-config ];   # build tools
  libraries   = with pkgs; [ webkitgtk ];    # link libs
};

# After
rustHelpers = katsuobushi.lib.rust {
  inherit pkgs crane;
  # ...
  nativeBuildInputs = with pkgs; [ pkg-config ];   # build tools
  buildInputs       = with pkgs; [ webkitgtk ];    # link libs
};
```

Both now default to `[ ]` (previously `buildInputs` was required), so tool-only
projects can omit them entirely.

**`lib.rust`: wasm-bindgen version is derived — action required for non-default
wasm builds**

The `wasm-bindgen-cli` version is no longer hard-pinned in the lib; it is read
from your `Cargo.lock`. The lib ships hashes for **0.2.108** as the default.

- If you build wasm **and** your lock file pins a different `wasm-bindgen`, eval
  now fails fast with a copy-pasteable fix (previously you would have silently
  received a mismatched 0.2.108 CLI — a latent runtime bug):

  ```nix
  rustHelpers = katsuobushi.lib.rust {
    # ...
    wasmBindgenHashes."0.2.99" = {
      hash      = pkgs.lib.fakeHash;   # build once, copy the real hash from the error
      cargoHash = pkgs.lib.fakeHash;
    };
  };
  ```

- If you are on 0.2.108, or you do not build wasm: no change needed. The
  `Cargo.lock` read is lazy, so native-only projects and the bare template never
  trigger it.

[Prettier]: https://prettier.io
[options]: https://prettier.io/docs/options
