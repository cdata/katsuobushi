//! `project status` — the read view. Bare (`status`) lists the board; with an
//! id (`status <id>`) it details one card. Joins board lane membership with note
//! frontmatter; `--json` lets in-sandbox agents self-serve the board.

use anyhow::{Context, Result};
use serde::Serialize;

use crate::output::{render_table, Renderer, TableCell};

use super::board::Board;
use super::clock::{parse_rfc3339, Clock};
use super::fs::Fs;
use super::layout::{self, NoteEntry, Paths};
use super::model::{CardId, Status};
use super::select::resolve_status;

/// Archived cards drop off the human list once their `disposition_at` is older
/// than this window (1h). `--json` is unaffected — tooling still sees them all.
const ARCHIVE_WINDOW_SECS: i64 = 60 * 60;

/// A card's board+note projection for the list and detail views.
#[derive(Serialize, Clone)]
struct CardView {
    id: String,
    /// The resolved status token (lane, or archived disposition). `None` if the
    /// card is on the board but in no recognized lane.
    status: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    title: String,
    blocked_by: Vec<String>,
    /// Grabbable: To-do with every blocker at ready/accepted.
    available: bool,
    labels: Vec<String>,
}

/// `project status [id] [--lane] [--available] [--label ...]`. With an id,
/// detail one card; without, list the board (optionally filtered).
#[allow(clippy::too_many_arguments)]
pub fn show(
    fs: &dyn Fs,
    paths: &Paths,
    renderer: &Renderer,
    clock: &dyn Clock,
    id_input: Option<String>,
    status_filter: Option<Status>,
    available_only: bool,
    label_filter: &[String],
    icebox_only: bool,
) -> Result<()> {
    let board_text = fs
        .read(&paths.board_md())
        .with_context(|| format!("read {}", paths.board_md().display()))?;
    let board = Board::parse(&board_text);
    let notes = layout::load_notes(fs, paths)?;

    match id_input {
        Some(input) => show_one(&board, &notes, renderer, &input),
        None if icebox_only => list_icebox(&board, &notes, renderer, label_filter),
        None => list_all(
            &board,
            &notes,
            renderer,
            clock,
            status_filter,
            available_only,
            label_filter,
        ),
    }
}

/// List the icebox: notes that no card on the board references. The icebox is
/// derived by set difference (issues on disk minus the ids the board names), so
/// nothing about the iced state lives in the note. Composes with `--label`.
fn list_icebox(
    board: &Board,
    notes: &[NoteEntry],
    renderer: &Renderer,
    label_filter: &[String],
) -> Result<()> {
    let views = iced_views(board, notes, label_filter);
    renderer.emit(&views, |_| icebox_table(&views))
}

/// The icebox listing: id, title, and labels only (design/PDD001) — an iced note
/// has no lane, so the board's status/type columns do not apply.
fn icebox_table(views: &[CardView]) -> String {
    if views.is_empty() {
        return "(icebox empty)".to_string();
    }
    let rows: Vec<Vec<TableCell>> = views
        .iter()
        .map(|v| {
            vec![
                TableCell::plain(v.id.clone()),
                TableCell::plain(v.title.clone()),
                TableCell::plain(v.labels.join(", ")),
            ]
        })
        .collect();
    render_table(&["ID", "TITLE", "LABELS"], &rows, false)
}

/// The iced notes as card views, in note order: every note whose id no card on
/// the board references, narrowed by the label filter.
fn iced_views(board: &Board, notes: &[NoteEntry], label_filter: &[String]) -> Vec<CardView> {
    let on_board: std::collections::HashSet<CardId> = super::board_ids(board).into_iter().collect();
    let mut views: Vec<CardView> = notes
        .iter()
        .filter_map(|e| e.id())
        .filter(|id| !on_board.contains(id))
        .map(|id| build_view(board, notes, &id, None))
        .collect();
    if !label_filter.is_empty() {
        views.retain(|v| label_matches(v, label_filter));
    }
    views
}

/// List the whole board (active lanes in priority order, then the archive),
/// optionally filtered by status or grabbability.
fn list_all(
    board: &Board,
    notes: &[NoteEntry],
    renderer: &Renderer,
    clock: &dyn Clock,
    status_filter: Option<Status>,
    available_only: bool,
    label_filter: &[String],
) -> Result<()> {
    let now = clock.now_unix();
    let mut views = Vec::new();
    // Ids of archived cards hidden from the human list by the 1h window. The
    // JSON path serializes every view regardless; only the table filters.
    let mut stale_archived = std::collections::HashSet::new();
    for status in Status::ACTIVE {
        for card in board.cards_in(status) {
            if let Some(id) = card.id() {
                views.push(build_view(board, notes, &id, Some(status)));
            }
        }
    }
    for card in board.archived() {
        if let Some(id) = card.id() {
            let disp = resolve_status(board, notes, &id);
            if is_stale_archive(notes, &id, now) {
                stale_archived.insert(id.to_string());
            }
            views.push(build_view(board, notes, &id, disp));
        }
    }

    if let Some(want) = status_filter {
        views.retain(|v| v.status.as_deref() == Some(want.token()));
    }
    if available_only {
        views.retain(|v| v.available);
    }
    if !label_filter.is_empty() {
        views.retain(|v| label_matches(v, label_filter));
    }

    renderer.emit(&views, |_| {
        let shown: Vec<CardView> = views
            .iter()
            .filter(|v| !stale_archived.contains(&v.id))
            .cloned()
            .collect();
        table(&shown)
    })
}

/// Whether an archived card should drop off the human list: its `disposition_at`
/// is older than the 1h window, or is missing/unparseable (treated as old). All
/// archived cards — accepted and cancelled alike — are subject to this.
fn is_stale_archive(notes: &[NoteEntry], id: &CardId, now: i64) -> bool {
    let at = notes
        .iter()
        .find(|e| e.id().as_ref() == Some(id))
        .and_then(|e| e.meta.as_ref().ok())
        .and_then(|m| m.disposition_at.as_deref())
        .and_then(parse_rfc3339);
    match at {
        Some(t) => now - t > ARCHIVE_WINDOW_SECS,
        None => true,
    }
}

/// Detail one card: its resolved status, frontmatter, and body.
fn show_one(board: &Board, notes: &[NoteEntry], renderer: &Renderer, id_input: &str) -> Result<()> {
    let id = layout::resolve_id(id_input, &super::board_ids(board))?;
    let status = resolve_status(board, notes, &id);
    let view = build_view(board, notes, &id, status);
    let body = notes
        .iter()
        .find(|e| e.id().as_ref() == Some(&id))
        .map(|e| e.note.body().to_string())
        .unwrap_or_default();

    #[derive(Serialize)]
    struct ShowOutput {
        #[serde(flatten)]
        card: CardView,
        body: String,
    }
    let out = ShowOutput { card: view, body };
    renderer.emit(&out, |r| {
        let c = &out.card;
        let mut s = format!("{}  {}  {}", c.id, r.green(&status_display(c)), c.kind);
        s.push_str(&format!("\n  title: {}", c.title));
        if !c.labels.is_empty() {
            s.push_str(&format!("\n  labels: {}", c.labels.join(", ")));
        }
        if !c.blocked_by.is_empty() {
            s.push_str(&format!("\n  blocked_by: {}", c.blocked_by.join(", ")));
        }
        if !out.body.trim().is_empty() {
            s.push_str(&format!("\n\n{}", out.body.trim_end()));
        }
        s
    })
}

/// Whether a card carries every requested label. The match is exact and
/// whole-token; an empty request matches everything. Repetition is AND — the
/// board is narrowed to the intersection of the requested labels.
fn label_matches(view: &CardView, wanted: &[String]) -> bool {
    wanted
        .iter()
        .all(|want| view.labels.iter().any(|have| have == want))
}

/// Build the projection for one card id.
fn build_view(board: &Board, notes: &[NoteEntry], id: &CardId, status: Option<Status>) -> CardView {
    let meta = notes
        .iter()
        .find(|e| e.id().as_ref() == Some(id))
        .and_then(|e| e.meta.as_ref().ok());
    let blocked_by: Vec<CardId> = meta.map(|m| m.blocked_by.clone()).unwrap_or_default();
    let available = super::select::is_available(board, notes, id);
    CardView {
        id: id.to_string(),
        status: status.map(|s| s.token().to_string()),
        kind: meta.map(|m| m.kind.token().to_string()).unwrap_or_default(),
        title: meta
            .map(|m| m.title.clone())
            .unwrap_or_else(|| "(no note)".into()),
        blocked_by: blocked_by.iter().map(|c| c.to_string()).collect(),
        available,
        labels: meta.map(|m| m.labels.clone()).unwrap_or_default(),
    }
}

/// The status as shown to humans: a To-do card with unmet blockers reads
/// `todo (blocked)`, so a card sitting out of `--available` is never mistaken
/// for a bug. Every other status renders as its bare token. (The `--json`
/// `status`/`available` fields stay separate for tooling.)
fn status_display(v: &CardView) -> String {
    match v.status.as_deref() {
        Some("todo") if !v.available => "todo (blocked)".to_string(),
        Some(s) => s.to_string(),
        None => "?".to_string(),
    }
}

fn table(views: &[CardView]) -> String {
    if views.is_empty() {
        return "(no cards)".to_string();
    }
    let rows: Vec<Vec<TableCell>> = views
        .iter()
        .map(|v| {
            vec![
                TableCell::plain(v.id.clone()),
                TableCell::plain(status_display(v)),
                TableCell::plain(v.kind.clone()),
                TableCell::plain(v.title.clone()),
            ]
        })
        .collect();
    render_table(&["ID", "STATUS", "TYPE", "TITLE"], &rows, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::board::Card;
    use crate::project::clock::{format_rfc3339, FixedClock};
    use crate::project::fs::FakeFs;

    /// A fixed "now": 2026-07-17T18:22:04Z.
    const NOW: i64 = 1_784_312_524;

    /// A board where b is blocked by a; both are To-do.
    fn seeded() -> (FakeFs, Paths) {
        let mut board = Board::parse(&layout::initial_board());
        board.insert_card(
            Status::Todo,
            Card::new_link(&CardId::parse("aaaaaa").unwrap()),
            false,
        );
        board.insert_card(
            Status::Todo,
            Card::new_link(&CardId::parse("bbbbbb").unwrap()),
            false,
        );
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &board.to_text())
            .with_file(
                "/b/issues/aaaaaa.md",
                "---\nid: aaaaaa\ntitle: A\ntype: feature\nblocked_by: []\n---\nbody a\n",
            )
            .with_file(
                "/b/issues/bbbbbb.md",
                "---\nid: bbbbbb\ntitle: B\ntype: bug\nblocked_by: [aaaaaa]\n---\nbody b\n",
            );
        (fs, Paths::new("/b"))
    }

    #[test]
    fn available_reflects_blocker_status() {
        let (fs, paths) = seeded();
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        let notes = layout::load_notes(&fs, &paths).unwrap();

        // a has no blockers -> available; b is blocked by a (still To-do) -> not.
        let a = build_view(
            &board,
            &notes,
            &CardId::parse("aaaaaa").unwrap(),
            Some(Status::Todo),
        );
        let b = build_view(
            &board,
            &notes,
            &CardId::parse("bbbbbb").unwrap(),
            Some(Status::Todo),
        );
        assert!(a.available);
        assert!(!b.available);

        // The human status annotates the blocked one, not the grabbable one.
        assert_eq!(status_display(&a), "todo");
        assert_eq!(status_display(&b), "todo (blocked)");

        // Move a to ready: now b's blocker is cleared -> b available.
        let mut board2 = board.clone();
        board2.move_card(&CardId::parse("aaaaaa").unwrap(), Status::Ready);
        let b2 = build_view(
            &board2,
            &notes,
            &CardId::parse("bbbbbb").unwrap(),
            Some(Status::Todo),
        );
        assert!(b2.available);
    }

    #[test]
    fn list_json_emits_all_cards() {
        let (fs, paths) = seeded();
        let r = Renderer::new(true, false);
        let clock = FixedClock(NOW);
        // Should not error; JSON path serializes the Vec.
        show(&fs, &paths, &r, &clock, None, None, false, &[], false).unwrap();
        show(
            &fs,
            &paths,
            &r,
            &clock,
            None,
            Some(Status::Todo),
            false,
            &[],
            false,
        )
        .unwrap();
        show(&fs, &paths, &r, &clock, None, None, true, &[], false).unwrap();
        // The icebox view (set difference) also renders without error.
        show(&fs, &paths, &r, &clock, None, None, false, &[], true).unwrap();
    }

    /// A three-card board: two carry `PDD007`, one carries `debt`; one of the
    /// PDD007 cards also carries `security`. Mirrors the design's worked example.
    fn labelled() -> (FakeFs, Paths) {
        let mut board = Board::parse(&layout::initial_board());
        for id in ["aaaaaa", "bbbbbb", "cccccc"] {
            board.insert_card(
                Status::Todo,
                Card::new_link(&CardId::parse(id).unwrap()),
                false,
            );
        }
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &board.to_text())
            .with_file(
                "/b/issues/aaaaaa.md",
                "---\nid: aaaaaa\ntitle: A\ntype: feature\nblocked_by: []\nlabels: [PDD007, security]\n---\nbody a\n",
            )
            .with_file(
                "/b/issues/bbbbbb.md",
                "---\nid: bbbbbb\ntitle: B\ntype: bug\nblocked_by: []\nlabels: [debt]\n---\nbody b\n",
            )
            .with_file(
                "/b/issues/cccccc.md",
                "---\nid: cccccc\ntitle: C\ntype: feature\nblocked_by: []\nlabels: [PDD007]\n---\nbody c\n",
            );
        (fs, Paths::new("/b"))
    }

    /// Build the label-carrying views for the `labelled` fixture, in board order.
    fn labelled_views() -> Vec<CardView> {
        let (fs, paths) = labelled();
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        let notes = layout::load_notes(&fs, &paths).unwrap();
        ["aaaaaa", "bbbbbb", "cccccc"]
            .iter()
            .map(|id| {
                build_view(
                    &board,
                    &notes,
                    &CardId::parse(id).unwrap(),
                    Some(Status::Todo),
                )
            })
            .collect()
    }

    fn ids_matching(wanted: &[&str]) -> Vec<String> {
        let want: Vec<String> = wanted.iter().map(|s| s.to_string()).collect();
        labelled_views()
            .into_iter()
            .filter(|v| label_matches(v, &want))
            .map(|v| v.id)
            .collect()
    }

    #[test]
    fn it_selects_only_cards_that_carry_the_label() {
        assert_eq!(ids_matching(&["PDD007"]), vec!["aaaaaa", "cccccc"]);
    }

    #[test]
    fn it_matches_a_label_exactly_not_as_a_substring() {
        // `PDD` is a prefix of `PDD007` but not a whole label -> no match.
        assert!(ids_matching(&["PDD"]).is_empty());
        // `security` is a whole label on exactly one card.
        assert_eq!(ids_matching(&["security"]), vec!["aaaaaa"]);
    }

    #[test]
    fn it_ands_a_label_with_a_lane_filter() {
        // The lane filter runs before the label retain in `list_all`; here we
        // assert the label predicate itself narrows the To-do lane to PDD007.
        let todo_pdd007: Vec<String> = labelled_views()
            .into_iter()
            .filter(|v| v.status.as_deref() == Some("todo"))
            .filter(|v| label_matches(v, &["PDD007".to_string()]))
            .map(|v| v.id)
            .collect();
        assert_eq!(todo_pdd007, vec!["aaaaaa", "cccccc"]);
    }

    #[test]
    fn it_ands_two_labels() {
        assert_eq!(ids_matching(&["PDD007", "security"]), vec!["aaaaaa"]);
        assert!(ids_matching(&["PDD007", "debt"]).is_empty());
    }

    #[test]
    fn it_returns_the_whole_board_when_no_label_is_given() {
        assert_eq!(ids_matching(&[]), vec!["aaaaaa", "bbbbbb", "cccccc"]);
    }

    /// A board with one To-do card (`aaaaaa`) and two iced notes: `bbbbbb`
    /// (label `debt`) and `cccccc` (label `PDD001`), neither on the board.
    fn iceboxed() -> (FakeFs, Paths) {
        let mut board = Board::parse(&layout::initial_board());
        board.insert_card(
            Status::Todo,
            Card::new_link(&CardId::parse("aaaaaa").unwrap()),
            false,
        );
        let fs = FakeFs::new()
            .with_file("/b/BOARD.md", &board.to_text())
            .with_file(
                "/b/issues/aaaaaa.md",
                "---\nid: aaaaaa\ntitle: On board\ntype: feature\nblocked_by: []\n---\n",
            )
            .with_file(
                "/b/issues/bbbbbb.md",
                "---\nid: bbbbbb\ntitle: Iced debt\ntype: chore\nblocked_by: []\nlabels: [debt]\n---\n",
            )
            .with_file(
                "/b/issues/cccccc.md",
                "---\nid: cccccc\ntitle: Iced idea\ntype: feature\nblocked_by: []\nlabels: [PDD001]\n---\n",
            );
        (fs, Paths::new("/b"))
    }

    fn iced_ids(label_filter: &[String]) -> Vec<String> {
        let (fs, paths) = iceboxed();
        let board = Board::parse(&fs.get("/b/BOARD.md").unwrap());
        let notes = layout::load_notes(&fs, &paths).unwrap();
        iced_views(&board, &notes, label_filter)
            .into_iter()
            .map(|v| v.id)
            .collect()
    }

    #[test]
    fn it_lists_an_iced_note_under_status_icebox() {
        // The two notes with no board card are iced; the on-board card is not.
        assert_eq!(iced_ids(&[]), vec!["bbbbbb", "cccccc"]);
    }

    #[test]
    fn it_composes_status_icebox_with_a_label_filter() {
        assert_eq!(iced_ids(&["PDD001".to_string()]), vec!["cccccc"]);
        assert_eq!(iced_ids(&["debt".to_string()]), vec!["bbbbbb"]);
    }

    /// Notes for archived cards, keyed by `(id, disposition, disposition_at)`;
    /// an empty `at` omits the field entirely.
    fn archived_notes(specs: &[(&str, &str, &str)]) -> Vec<NoteEntry> {
        let mut fs = FakeFs::new();
        for (id, disp, at) in specs {
            let at_line = if at.is_empty() {
                String::new()
            } else {
                format!("disposition_at: {at}\n")
            };
            fs = fs.with_file(
                format!("/b/issues/{id}.md"),
                &format!(
                    "---\nid: {id}\ntitle: {id}\ntype: feature\nblocked_by: []\ndisposition: {disp}\n{at_line}---\n\nbody\n"
                ),
            );
        }
        layout::load_notes(&fs, &Paths::new("/b")).unwrap()
    }

    #[test]
    fn archive_window_hides_only_stale_cards() {
        let clock = FixedClock(NOW);
        let now = clock.now_unix();
        // Recent (10m ago), accepted and cancelled alike, stay visible.
        let notes = archived_notes(&[
            ("aaaaaa", "accepted", &format_rfc3339(now - 600)),
            ("bbbbbb", "cancelled", &format_rfc3339(now - 600)),
        ]);
        assert!(!is_stale_archive(
            &notes,
            &CardId::parse("aaaaaa").unwrap(),
            now
        ));
        assert!(!is_stale_archive(
            &notes,
            &CardId::parse("bbbbbb").unwrap(),
            now
        ));

        // Older than 1h (48h ago) — both hidden, regardless of disposition.
        let notes = archived_notes(&[
            ("cccccc", "accepted", &format_rfc3339(now - 48 * 3600)),
            ("dddddd", "cancelled", &format_rfc3339(now - 48 * 3600)),
        ]);
        assert!(is_stale_archive(
            &notes,
            &CardId::parse("cccccc").unwrap(),
            now
        ));
        assert!(is_stale_archive(
            &notes,
            &CardId::parse("dddddd").unwrap(),
            now
        ));
    }

    #[test]
    fn archive_window_hides_a_card_missing_disposition_at() {
        let now = FixedClock(NOW).now_unix();
        let notes = archived_notes(&[("aaaaaa", "accepted", "")]);
        // Missing timestamp -> treated as old -> hidden.
        assert!(is_stale_archive(
            &notes,
            &CardId::parse("aaaaaa").unwrap(),
            now
        ));
    }

    #[test]
    fn archive_window_boundary_is_inclusive_at_1h() {
        let now = FixedClock(NOW).now_unix();
        // Exactly 1h old is not yet "older than" the window -> still visible.
        let notes = archived_notes(&[(
            "aaaaaa",
            "accepted",
            &format_rfc3339(now - ARCHIVE_WINDOW_SECS),
        )]);
        assert!(!is_stale_archive(
            &notes,
            &CardId::parse("aaaaaa").unwrap(),
            now
        ));
    }
}
