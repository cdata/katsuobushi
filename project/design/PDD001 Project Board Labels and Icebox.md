# PDD001 Project Board Labels and Icebox

## Introduction

The [`project` backlog](project.md) is a file-backed Obsidian-Kanban **board**.
The board has a settled core rule. **`BOARD.md`** owns a card's status and its
priority. The status is the card's lane. The priority is its position in the
lane. Each card's **note** owns the card's identity and detail. A card on the
board is a bare `[[<id>]]` link. Nothing is copied, so a title never needs a
resync. This model runs the board today. This document does not change it. It
adds three things on top of it.

The first is **epics**. Epics need no new schema. A person who runs the board
often carries two threads at once. One thread is a scoped, featureful
improvement. The other is a loose backlog of debt, chores, and one-off cards. The
person wants to read each thread alone. The board already carries a freeform
**label** on every card. Nothing reads that label today. So an epic is not a new
field and not a new state. An epic is a shared label. The epic *view* is the
board filtered to that label: `project status --label=PDD007`. The miscellaneous
backlog is not a place. It is every card the filter omits.

The second is the **icebox**. The icebox is also an absence, not a thing. A note
that exists with no card on the board is iced. To file an idea, a person writes
one new file. Two people can write two new files at the same time. Two agents on
unrelated cards can do the same. They never collide, because a conflict needs a
shared file and a new file has none. Later the board owner promotes an iced note
onto the board. The promote uses the same `status set` verb the owner already
knows. The owner can also cancel an iced note where it sits.

The third is **durability under the swarm**. Sandbox VMs land work onto the
board. The host edits the board at the same time. So the board is the most
exposed file in the repository. This document states one rule that removes a
whole class of that damage: the host is the only writer of the board. It also
changes how the host fetches a sandbox branch. After that change, landing a card
once does not break the next fetch of the same instance. One heavier question
stays out of scope: must every board mutation commit itself? That question
entangles with commit signing. It needs its own design.

## Goals

- **Epics as labels, read through a filter.** A card carries a shared label to
  mark its epic, for example `PDD007`, `warm-artifacts`, or `debt`.
  `project status` gets a `--label=<value>` option. The option narrows the board
  to the cards that carry the label. The match is exact. The option composes with
  the existing `--lane` and `--available` filters, and a card must match all of
  them. So `--label=PDD007 --available` shows the grabbable work in the PDD007
  thread. This needs no new field, no reserved prefix, and no migration.

- **The `design` field folded into labels.** The note's optional `design:` field
  says what a label already says. It is a weaker, second way. This design removes
  it. The `project new --design <ref>` option stays as a **deprecated alias**. The
  alias warns and appends the reference as a label. `project lint --fix` migrates
  any note that still has a `design:` field. It folds the field into a label and
  drops the dead key.

- **An icebox that is the absence of a board card.** A **note with no `[[<id>]]`
  card on the board** is iced. `project new --icebox` writes the note and no
  card. Plain `project new` does not change. The board owner sees the iced notes
  with `project status --icebox`. That option composes with `--label`.

- **One verb for the icebox edges.** `project status set <id> todo` on an iced
  note promotes it. The promote adds a To-do card. `project status set <id>
  icebox` shelves a board card back to the note-only state. `project status set
  <id> cancelled` on an iced note sends it straight to the archive as a
  tombstone. One verb the person already knows carries the card in and out of the
  icebox.

- **The board as single-writer state.** A sandbox guest never writes
  `project/kanban/`. The guest gets its card as prose. It returns code as a git
  branch. It returns findings as `report` lines. The host makes every board
  change. This design states this rule as an invariant, not a habit.

- **A sandbox fetch that survives a landing.** `sandbox fetch` lands a guest's
  branch into a per-instance tracking ref. The host does not rebase that ref. So
  a second fetch of an instance already landed once no longer fails
  non-fast-forward. That second fetch is the shape of every review bounce.

## Non-goals

- **Auto-commit of board mutations.** A guard can seal each `status set`, `new`,
  or `prioritize` into its own commit. That guard closes the last concurrency
  fault: uncommitted board edits lost when the host advances its working copy onto
  the landed commit. That step is the `jj new` or the branch fast-forward at the
  end of a landing. This design defers the guard. The guard must also read and
  adapt to a commit-signing failure when the signing key is absent. That
  interaction needs its own design.

- **A board health command.** A `doctor` command can flag a dirty board or a lane
  change with no matching commit. It arrives with the auto-commit work. That work
  makes each mutation its own reviewable commit, which is the signal the `doctor`
  reads.

- **Append-only per-card note fragments.** This design does not build per-card
  note fragments (`issues/<id>.notes/<ts>-<author>.md`). The host-only rule
  removes the reason for an agent to write the board. So fragments wait for a real
  need.

- **A grouped or swimlane board layout.** A grouped board file fights the
  lane-per-status shape. Grouping stays a read-time view through `--label`. The
  board on disk keeps one card list per status.

- **Richer label filters.** Label globs, OR across labels, and saved views are
  out of scope. This design ships one exact-match filter. The filter composes with
  the filters already present.

- **The design, plan, and implement skills.** One workflow authors a PDD, files
  its plan on the board, and drives the board with sandboxes. That workflow is a
  separate design.

## Body

### The board at a glance

The board is one card list per status. A label lives on the card's note. On the
card face, a person sees that label. The `--label` filter does not restructure
the list. It reads the list and shows only the matching cards:

```
  BOARD.md — one list, every thread interleaved
  ┌────────────────────────────┐        project status --label=PDD007
  │ To-do                      │        ┌────────────────────────────┐
  │   [a1f7]  labels: PDD007   │        │ To-do                      │
  │   [b2c9]  labels: —        │  ───▶  │   [a1f7]  PDD007           │
  │   [c3d0]  labels: PDD007   │ filter │   [c3d0]  PDD007           │
  │ In Progress                │        │ In Progress                │
  │   [d4e1]  labels: PDD007   │        │   [d4e1]  PDD007           │
  │   [e5f2]  labels: debt     │        └────────────────────────────┘
  └────────────────────────────┘        the misc. backlog = every
                                         card this view leaves out
```

### Ground truth

Hard facts a fresh reading needs, checked against the tree at the time of
writing:

- **A card already carries labels that nothing reads.** `NoteMeta`
  (`rust/katsuobushi-controller/src/project/note.rs`) holds `labels: Vec<String>`
  and an optional `design: Option<String>`. Only `project new --labels a,b` sets
  labels. No `query`, `select`, or `lint` path reads or filters by them. The epic
  filter is a new reader of a field that already exists. It is not a new field.

- **`design` is a single string that points at a design doc.**
  `NoteMeta::from_note` parses it. `render_new_note` writes it. The
  `metadata-keys` block in the board settings renders it as its own column on the
  card face (`rust/katsuobushi-controller/src/project/layout.rs`). To fold it into
  labels, remove it from these three places and drop one settings column.

- **The note keeps unknown keys byte-for-byte.** A line-oriented frontmatter
  editor edits the note. It is not a serde round-trip. So a legacy `design:` line
  survives untouched until a migration reads it. This lets `lint --fix` migrate at
  any time, not on every write.

- **The board writer is idempotent and snapshot-tested.** `Board::to_text`
  (`rust/katsuobushi-controller/src/project/board.rs`) renders byte-identical
  output across a Prettier-style pass. Golden snapshots guard it. The writer
  already adds, removes, and archives cards through `insert_card`, `remove_card`,
  and `move_card`. The icebox edges are new callers of these functions. They are
  not a new writer.

- **The lifecycle has six states and no icebox.** The `Status` enum
  (`rust/katsuobushi-controller/src/project/model.rs`) has six values: `Todo`,
  `InProgress`, `NeedsReview`, `Ready`, `Accepted`, and `Cancelled`.
  `transition_allowed` (`.../state.rs`) is the gate `status set` calls. `Accepted`
  and `Cancelled` are terminal and archived. No state means "off the board but not
  dead."

- **Lint already tolerates an icebox, informally.** `project lint`
  (`rust/katsuobushi-controller/src/project/lint.rs`) reports a note with no board
  card as `warn: orphan-note`. It also warns, but does not error, on an
  unrecognized lane, because "a deliberate Icebox is allowed." The icebox is
  half-blessed already. This design makes it canonical and quiets the warning.

- **The board is host-only today.** Host-side `dispatch`
  (`rust/katsuobushi-controller/src/sandbox/dispatch.rs`) is the one place the
  sandbox domain reaches into the board. It reads the board to compose a
  directive. It claims the card `todo → in-progress` before the VM boots. The
  guest gets the card as prose. It never reads or writes `project/kanban/`. The
  invariant is already the practice. The past breakage came from a directive that
  told an agent to write a note.

- **A fetch lands into a local branch that the host then rebases.** `sandbox
  fetch` runs `git fetch <sync.git> sandbox/<inst>:sandbox/<inst>`
  (`rust/katsuobushi-controller/src/sandbox/fetch.rs`). It writes straight into
  local `refs/heads/sandbox/<inst>`. The `landed` check reads the seed from
  `instance.json`. It compares the seed to `git rev-parse sandbox/<inst>`. The
  host rebases that same local branch to land the card. So the next fetch of the
  instance diverges and fails.

### Epics are labels, and a view is a filter

_As the board owner, I label every card in my feature thread `PDD007`. When the
board gets noisy, I run `project status --label=PDD007`. I see that thread alone.
The debt cards drop away, because they never carried the label._

An epic is the set of cards that share a label. There is no epic object to
create. There is no membership to register. There is nothing to clean up when the
epic is done. A person makes an epic by using a label. The person retires the
epic by dropping the label.

The `--label=<value>` option on `project status` works like this:

- **Match is exact.** `--label=PDD007` selects a card whose `labels` list
  contains exactly `PDD007`. A label is a whole token, not a substring.
- **Composition is AND.** The filter narrows the same listing that `--lane` and
  `--available` produce. `--label=PDD007 --lane=needs-review` shows PDD007 cards
  that wait for review. `--label=PDD007 --available` shows PDD007 work a person
  can grab now.
- **Repetition is AND.** `--label=PDD007 --label=security` selects a card that
  carries both labels.
- **Output is the normal flat listing.** The filter does not group the board or
  change card order. It hides the cards that do not match.

That last point is the whole grouping decision. A person gets a focused thread.
The board file grows no second axis. The board stays one list per status. The
epic is only a view over that list.

### The `design` field becomes a label

The `design:` field predates labels. It says a subset of what a label says: "this
card belongs to design X." This design removes the field. Its meaning moves to a
label.

- **New cards use labels.** `project new` carries a repeatable label option. It
  is spelled `--label`. It accepts a comma list or repetition (`--label=a,b` or
  `--label=a --label=b`). The older `--labels` spelling keeps working as an alias.
  A card in the warm-artifacts thread carries a `warm-artifacts` label. A card in
  this document's thread carries a `PDD001` label.
- **The old option stays, deprecated.** `project new --design <ref>` prints a
  deprecation warning. It appends `<ref>` to the card's labels. The canonical
  option is the repeatable label option. Both spellings keep working. So a
  consumer's scripts do not break on the day they update.
- **`lint --fix` migrates the old field.** For each note that still carries a
  `design: X` key, the fix appends `X` to `labels`. It removes the `design:` key.
  It drops the `design` column from the board settings block. A person runs one
  command. No note needs a hand edit. A second `lint` run reports no change.

### The icebox is a note with no card

_As a contributor, I work on one card and think of a separate improvement. I file
it with `project new --icebox`. I add exactly one file. I do not touch the board.
So I cannot collide with the host's work on `BOARD.md` in the same minute._

The icebox is not a lane and not a stored field. A note is iced when no `[[<id>]]`
card refers to it. This follows the board's rule that a note carries no status.
The board's shape records where a card sits. "Nowhere on the board" is a shape the
board already shows by omission.

```
                 project new --icebox            project new
                        │                             │
                        ▼                             ▼
                 ┌──────────────┐   set todo    ┌──────────┐   grab
                 │    ICEBOX    │ ────────────▶ │  To-do   │ ───────▶ In Progress ──▶ …
                 │  (a note,    │ ◀──────────── │          │
                 │   no card)   │   set icebox  └──────────┘
                 └──────────────┘
                        │
                        │ set cancelled
                        ▼
                 ┌──────────────┐
                 │  Cancelled   │  ( a - [x] tombstone in ## Archive )
                 └──────────────┘
```

The edges, all through `status set`:

- **File in.** `project new --icebox` writes the note only. Plain `project new`
  still adds a To-do card. The host's normal filing does not change.
- **Promote out.** `project status set <id> todo` on an iced note adds a To-do
  card for it. A promotion targets To-do only, unless the person adds `--force`. A
  promotion enters the pipeline at the front. A promotion into another lane is a
  deliberate exception, and `--force` already covers it.
- **Shelve back.** `project status set <id> icebox` removes the card and keeps the
  note. `icebox` is a new target token for `status set`. A shelve from To-do is
  clean. A shelve from an active lane (in-progress, needs-review, ready) needs
  `--force`. A shelve of work in flight is rare and needs a second thought.
- **Cancel from the icebox.** `project status set <id> cancelled` on an iced note
  stamps the note with the `disposition` and `disposition_at` tombstone. It adds a
  `- [x] [[<id>]]` line to `## Archive`. This is the same tombstone every
  cancelled card gets. The iced note reaches it without a stop in an active lane.

The decision `status set` makes is below, in pseudocode. This is the shape, not
the implementation:

```
status_set(id, target, force):
    note = load_note(id)                # the note must exist
    on_board = board_has_card(id)

    if target == icebox:                # shelve
        if not on_board:
            return                       # already iced, a no-op
        if lane_of(id) != todo and not force:
            reject("shelve from an active lane needs --force")
        board_remove_card(id)
        return

    if not on_board:                    # promote out of the icebox
        if target == cancelled:
            stamp_disposition(note, cancelled)   # write the tombstone
            board_add_archived(id)               # a - [x] line in ## Archive
            return
        if target != todo and not force:
            reject("a promotion lands in To-do, use --force for another lane")
        board_add_card(id, lane = target)        # normally To-do
        return

    # the card is already on the board — the existing transition gate applies
    enforce_existing_transition(id, target, force)
```

### Seeing the icebox, and what lint says about it

_As the board owner, I start a session and run `project status --icebox`. I read
the ideas that piled up while I was away. I discard the ones with no value. I
promote the rest into To-do in priority order._

A person must be able to see the icebox for the daily triage step. `project status
--icebox` lists the iced notes: id, title, and labels. It shows nothing else. It
composes with `--label`. So `project status --icebox --label=PDD001` shows the
iced ideas in the PDD001 thread. Plain `project status` stays board-only. The
active view keeps its focus, and the icebox is one flag away.

Lint treats an orphan differently now. Today an iced note is a
`warn: orphan-note`. A note with no card once meant a card a bad edit lost. Under
this design an iced note is normal. So the check drops from a warning to an
`info` inventory line, for example `info: 2 notes in icebox: 8be736, 4a1c90`. It
never fails the lint gate.

This design gives up one signal on purpose. A card a bad merge drops from the
board now looks like a card a person iced. The board already carries concurrency
risk. The durable defense against a dropped edit is the deferred auto-commit
work. There, each board change is its own reviewable commit, and a lost card
shows up in the diff. The check is not deleted. It moves to the design that can do
it well.

### The board is host-only, and a fetch cannot collide

A **sandbox VM** is an isolated agent. The host starts it to do one card's work.
It commits its code to a git branch. It sends status and findings back over a
**report** channel. It never shares the host's working tree. Two of the board's
concurrency faults live in how that work returns to the host. The cure for each is
small.

**The host is the only writer of the board.** A sandbox guest never writes
`project/kanban/`. It gets its card as prose in its directive. It returns its code
as a git branch. It returns its findings as `report` lines. The host reads those
and makes every board change. A person must never tell an agent to write a finding
into a card note. The finding returns through the report channel, and the host
writes it down.

This is the structural cure for the card-note conflict. When one writer touches
the board, two writers cannot collide on a note. There is only ever one writer.
The past breakage came from a directive that asked an agent to write its own
findings into the card note. That directive made a second writer by hand. The
invariant closes that door. Findings go through the report channel, never the
board.

**A fetch lands into its own tracking ref.** The host collects a guest's work with
`sandbox fetch`. Today the fetch writes the guest's branch into a local branch.
The host then rebases that branch to land the card. The next fetch of the same
instance diverges from the rebased branch. It fails non-fast-forward. This is
every review bounce, when a reviewer sends the card back for changes. A bounce
refetches an instance the host already landed once.

```
  today — one local branch, and the host rebases it under the fetch:

    guest ─push─▶ sync.git ─fetch─▶ sandbox/<name> ─host rebase─▶ (branch moved)
                                         ▲                              │
                                         └──── next fetch collides ◀────┘
                                                (non-fast-forward)

  this design — the fetch writes a ref the host never moves:

    guest ─push─▶ sync.git ─fetch─▶ refs/katsuobushi/<name>   (never rebased)
                                         │
                                 host lands from here ─▶ local work (moves freely)
```

The fetch changes to land into a per-instance tracking ref, for example
`refs/katsuobushi/<name>`. The host never rebases that ref. The host's landing
work moves its own local branch as before. The fetch always writes the tracking
ref. So a second fetch never collides. The `landed` check compares the fetched tip
to the launch seed. It reads the tracking ref, not the local branch. A bounce loop
that fetches an instance many times succeeds each time.

### Considerations for implementors

- **The label filter is a `select` concern.** The listing already flows through a
  `query` and `select` seam (`.../project/query.rs`, `.../project/select.rs`). That
  seam applies `--lane` and `--available`. Put the label predicate beside them.
  Then every combination composes without a special case.

- **Icebox is derived, so keep it out of the note.** Nothing about the iced state
  goes into the note. The note carries no status, by the board's rule. `status
  --icebox` derives the icebox by set difference: the notes under `issues/` minus
  the ids the board references. The board stays the one source of truth.

- **The migration must survive Prettier and Obsidian.** The note parser already
  reads folded scalars and exploded flow lists. The `lint --fix` migration writes
  labels through the same list-writer path. So a migrated note round-trips
  byte-stable. The edit that drops the `design` column must keep the rest of the
  settings block byte-for-byte. Existing boards preserve that block verbatim.

- **The tracking-ref change has two readers.** Both the fetch refspec and the
  `landed` probe name the ref. Change them together. If you change only one, the
  "no committed work landed" warning reads the wrong ref and gives a false alarm.

- **Cancel-from-icebox writes two places.** It stamps the note and adds an archive
  line. If either half can fail, order the two halves with care. A partial run must
  leave the card promotable and repeatable, not half-tombstoned.

## Test Cases

Unit tests, BDD-named in the house style:

- **The label filter** — `it_selects_only_cards_that_carry_the_label`;
  `it_matches_a_label_exactly_not_as_a_substring`;
  `it_ands_a_label_with_a_lane_filter`;
  `it_ands_two_labels`;
  `it_returns_the_whole_board_when_no_label_is_given` (the filter is opt-in and
  changes nothing when absent).

- **The deprecated design alias** —
  `it_appends_the_design_reference_as_a_label`;
  `it_warns_that_design_is_deprecated`;
  `it_keeps_the_repeatable_label_option_as_canonical`.

- **The design-field migration** —
  `it_folds_a_legacy_design_field_into_labels`;
  `it_drops_the_design_key_from_the_note`;
  `it_drops_the_design_column_from_the_board_settings`;
  `it_is_idempotent_on_a_second_run` (a re-run reports no change).

- **Filing and seeing the icebox** —
  `it_writes_a_note_and_no_card_for_new_icebox`;
  `it_leaves_the_board_byte_identical_when_filing_to_the_icebox`;
  `it_lists_an_iced_note_under_status_icebox`;
  `it_composes_status_icebox_with_a_label_filter`.

- **The icebox edges** —
  `it_promotes_an_iced_note_into_todo`;
  `it_rejects_a_promotion_to_a_non_todo_lane_without_force`;
  `it_shelves_a_todo_card_back_to_the_icebox`;
  `it_rejects_a_shelve_from_an_active_lane_without_force`;
  `it_cancels_an_iced_note_straight_into_the_archive`.

- **Lint and the icebox** —
  `it_reports_an_iced_note_as_info_not_warn`;
  `it_exits_zero_with_iced_notes_present`.

- **The fetch tracking ref** —
  `it_fetches_a_branch_into_the_instance_tracking_ref`;
  `it_fetches_the_same_instance_twice_without_non_fast_forward`;
  `it_reads_the_landed_probe_from_the_tracking_ref`.

By hand, in a scratch board and a live sandbox instance:

- File three cards. Label two of them `PDD007`. Check that
  `project status --label=PDD007` shows exactly those two, in board order. Check
  that plain `project status` still shows all three. The unlabeled card is the
  miscellaneous backlog the filter omits.
- Take a note with a legacy `design:` field. Run `project lint --fix`. Check that
  the card face in Obsidian shows the value as a label, with no `design` column.
  Check that a second `lint` run is clean.
- File an idea with `project new --icebox`. Check that `BOARD.md` did not change.
  Promote it with `project status set <id> todo`. Check that it appears at the
  front of To-do. Shelve it back. Check that it leaves the board and returns to
  the icebox listing.
- Land a sandbox instance's branch. Bounce it: refetch the same instance after the
  guest commits again. Check that the second `sandbox fetch` succeeds and brings in
  the new commit. Before this change, it failed non-fast-forward.

## References

- [Project backlog design](project.md) — the source-of-truth inversion, the
  six-state lifecycle, and the note frontmatter this document extends.
- [PDD template and guidelines](README.md) — the required sections and the
  authoring rules this document follows.
- [Note schema and parser](../../rust/katsuobushi-controller/src/project/note.rs)
  — where `labels`, `design`, and the `disposition` tombstone live.
- [Board reader and writer](../../rust/katsuobushi-controller/src/project/board.rs)
  — the lane-per-status model and the archive lane the icebox edges write to.
- [Lifecycle state machine](../../rust/katsuobushi-controller/src/project/state.rs)
  — the transition gate the icebox edges extend.
- [Lint checks](../../rust/katsuobushi-controller/src/project/lint.rs) — the
  `orphan-note` check this document reclassifies to `info`.
- [Board settings and card face](../../rust/katsuobushi-controller/src/project/layout.rs)
  — the `metadata-keys` block that renders labels and the `design` column the
  migration drops.
- [Sandbox dispatch](../../rust/katsuobushi-controller/src/sandbox/dispatch.rs)
  — the one host-side seam from the sandbox domain into the board.
- [Sandbox fetch](../../rust/katsuobushi-controller/src/sandbox/fetch.rs) — the
  fetch refspec and the `landed` probe the tracking-ref change touches.
- [Sandbox skill: landing a branch](../../plugins/katsuobushi/skills/sandbox/SKILL.md)
  — the host-side rebase and landing steps the host-only rule constrains.
