---
id: 55cfca
title: "sandbox: agent terminal report truncated/CR-clobbered in captured (non-TTY) stream"
type: bug
blocked_by: []
design: project
labels: []
created: 2026-07-19T06:46:48Z
disposition: accepted
disposition_at: 2026-07-19T07:48:04Z
---

## Symptom

When driving a dispatched/prompted sandbox agent (`sandbox dispatch` / `sandbox prompt`), the agent's **terminal `report done` / `report blocked` summary is frequently truncated or overwritten** in the stream the orchestrator captures. The orchestrator sees an intermediate `working` line (or a partial summary) instead of the full terminal report — even though the VM ends cleanly (`ended-ok`).

Observed repeatedly (2026-07-18): nearly every peer-review verdict this session had to be recovered with `tr '\r' '\n'` on the task output file, and one dispatch's final report showed only "3 commits in. Running final clippy/fmt verification." with no terminal summary, despite `ended-ok`.

## Likely cause

The report stream is rendered with carriage returns (`\r`) for live status lines (the working spinner, the "no progress for Ns" notice), which overwrite the final `report done` line in a **non-TTY captured stream** (e.g. `run_in_background` output files). And/or the stream is torn down (VM powers off after the terminal report) before the full summary is flushed.

## Fix direction

- Emit the terminal `report done`/`blocked` **summary as a durable, newline-terminated line** that CR status updates never overwrite.
- Detect a non-TTY sink and **append lines instead of overwriting** (no `\r` status line when stdout is not a terminal).
- Ensure the full terminal report is flushed before the turn ends / the VM powers off.

## Acceptance criteria

- [ ] A backgrounded `sandbox dispatch`/`prompt` captures the COMPLETE terminal `report done`/`blocked` text (no truncation, no CR-clobbering) in its output file.
- [ ] Live status/progress lines still work on a TTY, but degrade to plain appended lines on a non-TTY sink.
- [ ] A test covers the non-TTY rendering path (terminal report survives intact).


## Recurrence 2026-07-18

Hit again while reviewing card 94bc4f: the `review-94bc4f` reviewer's `report
done` never reached the captured output file at all (5 lines — only the launch
banner), yet the dispatch exited 0 and the VM liveness was `ended`. Recovered by
re-prompting the still-running VM to restate its verdict. So the failure mode
includes the terminal report being *entirely absent* from a backgrounded stream,
not just truncated.

## Root cause (corrected 2026-07-19)

The original "CR-clobbering" hypothesis was WRONG — there is no `\r` in the
source, and intact captures have 0 CRs. The real cause: the streamed agent
reports render via `Renderer::emit` → `println!` to **stdout** (output.rs:151),
while all the reliably-captured progress lines (banner, "instance running", "no
progress", "guest ready") go to **stderr** via `eprintln!`. For the `emitExec`
menu commands (`sandbox start`/`dispatch`), the wrapper **captures katsuctl's
stdout** to exec the recipe path it prints (lib/menu §174-175) and sends all
decoration to `>&2` — so stdout is entangled with the exec mechanism and the
report `println!`s are lost/raced on teardown in a non-TTY capture, whereas
stderr is the reliable human channel.

## Fix (corrected)

Route the **streamed human reports/notes to stderr** (join the reliable channel
the other progress lines already use), keeping `--json` streaming on **stdout**
(machine-readable). Add an explicit flush for safety. Verify end-to-end on the
HOST (re-dispatch/prompt in the background and confirm the terminal report
appears) — a sandbox VM can't reproduce the host capture chain.

## Review notes

Implemented host-side (the bug only reproduces in the host's emitExec/non-TTY
capture chain a VM can't replicate); `Renderer::emit_progress` routes streamed
human reports to stderr (flushed), `--json` stays on stdout; render_report +
render_note rewired; 3 injectable-`write_progress` routing tests. Peer-reviewed
in sandbox `review-55cfca` — **ACCEPT** (would-not-block): tight + complete (all
streamed output funnels through render_report/render_note incl. the Lost line;
no stdout report path missed; 10 non-streaming `.emit()` callers untouched;
emitExec-captures-stdout rationale confirmed at lib/sandbox:1732-1740 /
menu:172-175); 338 tests green, clippy clean. **Proved itself end-to-end: this
review's own `report done` streamed cleanly into the captured file** (vs the
prior review, which captured zero report content and needed a re-prompt). One
non-blocking color-gating nit filed as a follow-up.
