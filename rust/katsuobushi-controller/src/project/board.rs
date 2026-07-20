//! The Obsidian-Kanban BOARD.md parser/writer — the authoritative store of
//! lifecycle (lane) and priority (position).
//!
//! On-disk contract (verified against the plugin's `src/parsers/common.ts`):
//! frontmatter carries `kanban-plugin`; lanes are `## Heading`; cards are
//! `- [ ] …` list items; the archive is the `## Archive` lane (optionally
//! preceded by a `***`/`---` thematic break, which we treat as decoration —
//! see `parse`); settings are a trailing `%% kanban:settings` code-fence block.
//!
//! Two regions are preserved **verbatim** so nothing a human/Obsidian writes is
//! lost: the `preamble` (everything up to the first lane — frontmatter and all)
//! and the `settings` block. Lanes and the archive are modelled and re-emitted.

use super::model::{CardId, Status};

/// The `kanban-plugin` frontmatter value. `basic` is recognised by every plugin
/// version (current writes `board` but still reads `basic`); we emit the
/// widest-compatible spelling.
pub const KANBAN_PLUGIN_VALUE: &str = "basic";
/// The thematic-break separator emitted before `## Archive`. We write `---`
/// because that is what a prettier-style formatter normalizes `***` to, so a
/// CLI rewrite is byte-stable under the repo's `markdown format` gate. Parsing
/// no longer keys on it (see `parse`) — the `## Archive` heading is the anchor —
/// so any spelling (`***`, `---`) or its absence is tolerated on read.
const ARCHIVE_SEP: &str = "---";
const ARCHIVE_HEADING: &str = "Archive";
const SETTINGS_MARKER: &str = "%% kanban:settings";

/// One card: its full raw list-item block (the `- [x] …` line plus any indented
/// continuation lines a human added), preserved so moves never mangle content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    raw: String,
}

impl Card {
    /// A fresh bare-link card for `new`: `- [ ] [[<id>]]`. The link target is
    /// the bare id (filename `issues/<id>.md`); Obsidian shows the short id as
    /// the card's display line, and surfaces the note's `title` (and other
    /// fields) beneath it via the board's metadata-keys — so nothing is copied.
    pub fn new_link(id: &CardId) -> Card {
        Card {
            raw: format!("- [ ] [[{id}]]"),
        }
    }

    /// The raw list-item block (for lint diagnostics).
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The card's id — the first `[[wikilink]]`'s target (the part before any
    /// `]` or `|`). `None` if the card carries no resolvable link.
    pub fn id(&self) -> Option<CardId> {
        let start = self.raw.find("[[")? + 2;
        let rest = &self.raw[start..];
        let end = rest.find([']', '|'])?;
        CardId::parse(&rest[..end])
    }

    /// Set the checkbox char (`x` when archiving a terminal card).
    fn set_check(&mut self, checked: bool) {
        let mark = if checked { 'x' } else { ' ' };
        if let Some(open) = self.raw.find("- [") {
            let idx = open + 3;
            if self.raw.as_bytes().get(idx).is_some()
                && self.raw.as_bytes().get(idx + 1) == Some(&b']')
            {
                self.raw.replace_range(idx..idx + 1, &mark.to_string());
            }
        }
    }
}

/// One lane: a title and its ordered cards (order = priority).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lane {
    pub title: String,
    pub cards: Vec<Card>,
    /// The Kanban WIP-limit annotation (`## To-do (5)` → `Some(5)`), preserved
    /// verbatim across a round-trip so a hand-set limit survives a CLI mutation.
    pub max_items: Option<u32>,
}

/// Where a card sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    Lane(usize),
    Archive,
}

/// The parsed board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Board {
    /// Everything before the first lane heading (frontmatter + blanks), verbatim.
    preamble: String,
    lanes: Vec<Lane>,
    archive: Vec<Card>,
    /// Whether an archive section existed / should be emitted.
    archive_present: bool,
    /// Everything after the lanes/archive, verbatim (`None` if nothing follows).
    /// The `%% kanban:settings … %%` block in the common case, but also any
    /// foreign trailing content a human left, so a rewrite never drops it.
    trailer: Option<String>,
}

impl Board {
    /// Parse BOARD.md. Tolerant: unknown lanes are kept; the preamble and
    /// settings block are captured verbatim.
    pub fn parse(text: &str) -> Board {
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;

        // Preamble: up to the first lane heading or settings. We do NOT break on
        // a thematic break here — the separator we emit (`---`) is also the
        // frontmatter delimiter, so keying the preamble on it would truncate at
        // the frontmatter's opening line.
        let mut preamble = String::new();
        while i < lines.len() {
            let l = lines[i];
            if is_lane_heading(l) || l.starts_with(SETTINGS_MARKER) {
                break;
            }
            preamble.push_str(l);
            preamble.push('\n');
            i += 1;
        }

        // Lanes, then the archive. The archive is anchored on the `## Archive`
        // heading itself — NOT on a preceding `***`/`---` separator, which a
        // formatter may have rewritten or the board may lack. The separator is
        // decoration we skip as a stray line. From `## Archive` onward we consume
        // archived items, skipping duplicate `## Archive` headings (the old
        // separator-loss append bug, card 5b4df3) so they merge into one archive
        // on the next write. A following *active-status* lane heading, however,
        // means the board is out of canonical "archive last" order; we yield back
        // to lane parsing rather than swallow its live cards into the archive.
        let mut lanes: Vec<Lane> = Vec::new();
        let mut archive: Vec<Card> = Vec::new();
        let mut archive_present = false;
        while i < lines.len() {
            let l = lines[i];
            if l.starts_with(SETTINGS_MARKER) {
                break;
            }
            if let Some((title, max_items)) = lane_heading(l) {
                if title == ARCHIVE_HEADING {
                    archive_present = true;
                    i += 1;
                    while i < lines.len() && !lines[i].starts_with(SETTINGS_MARKER) {
                        if let Some((t, _)) = lane_heading(lines[i]) {
                            // A real active lane after the archive: stop archiving
                            // and let the outer loop parse it as a lane. Any other
                            // heading (a duplicate `## Archive`, an unknown lane) is
                            // archive noise we skip and keep merging.
                            if t != ARCHIVE_HEADING && Status::from_lane_title(t).is_some() {
                                break;
                            }
                            i += 1;
                        } else if is_list_item(lines[i]) {
                            let (card, next) = take_card(&lines, i);
                            archive.push(card);
                            i = next;
                        } else {
                            i += 1; // blanks, separators
                        }
                    }
                    continue; // resume: settings, EOF, or an active lane we yielded to
                }
                lanes.push(Lane {
                    title: title.to_string(),
                    cards: Vec::new(),
                    max_items,
                });
                i += 1;
                continue;
            }
            if is_list_item(l) {
                let (card, next) = take_card(&lines, i);
                if let Some(lane) = lanes.last_mut() {
                    lane.cards.push(card);
                }
                i = next;
                continue;
            }
            if l.trim().is_empty() || is_thematic_break(l) {
                i += 1; // a blank line or the `***`/`---` archive separator: decoration
            } else if lines[i + 1..].iter().any(|x| lane_heading(x).is_some()) {
                // A stray line with a lane/archive heading still ahead is
                // mid-board, not trailing: skip it (as the parser always has)
                // so the structure after it still parses. Swallowing it into the
                // trailer would hide every later lane and the archive — a worse
                // loss than dropping one stray line (and it would strand the
                // archive, reviving the card 5b4df3 duplicate-append bug).
                i += 1;
            } else {
                break; // genuinely trailing foreign content — kept by the trailer
            }
        }

        // Trailer: everything from here to EOF, verbatim. The `%% kanban:settings`
        // block in the common case, but also any foreign content a human left
        // *trailing* the lanes/archive, so a CLI rewrite never silently drops it
        // (card 0e6516). The loop above stops here only when no lane/archive
        // heading follows — mid-board strays are skipped, not captured.
        let trailer = (i < lines.len()).then(|| lines[i..].join("\n"));

        Board {
            preamble,
            lanes,
            archive,
            archive_present,
            trailer,
        }
    }

    /// The cards in a given active status's lane, in priority order.
    pub fn cards_in(&self, status: Status) -> &[Card] {
        status
            .lane_title()
            .and_then(|t| self.lanes.iter().find(|l| l.title == t))
            .map(|l| l.cards.as_slice())
            .unwrap_or(&[])
    }

    /// The archived cards.
    pub fn archived(&self) -> &[Card] {
        &self.archive
    }

    /// All lanes (for lint / rendering).
    pub fn lanes(&self) -> &[Lane] {
        &self.lanes
    }

    /// Locate a card by id.
    pub fn locate(&self, id: &CardId) -> Option<Location> {
        for (idx, lane) in self.lanes.iter().enumerate() {
            if lane.cards.iter().any(|c| c.id().as_ref() == Some(id)) {
                return Some(Location::Lane(idx));
            }
        }
        if self.archive.iter().any(|c| c.id().as_ref() == Some(id)) {
            return Some(Location::Archive);
        }
        None
    }

    /// The active status a card currently has (its lane's status), if it is in a
    /// recognised active lane.
    pub fn status_of(&self, id: &CardId) -> Option<Status> {
        match self.locate(id)? {
            Location::Lane(idx) => Status::from_lane_title(&self.lanes[idx].title),
            Location::Archive => None,
        }
    }

    /// Remove a card by id from wherever it is (lane or archive), returning it.
    pub fn remove_card(&mut self, id: &CardId) -> Option<Card> {
        for lane in &mut self.lanes {
            if let Some(pos) = lane.cards.iter().position(|c| c.id().as_ref() == Some(id)) {
                return Some(lane.cards.remove(pos));
            }
        }
        if let Some(pos) = self
            .archive
            .iter()
            .position(|c| c.id().as_ref() == Some(id))
        {
            return Some(self.archive.remove(pos));
        }
        None
    }

    /// Insert a fresh card at the top or bottom of an active status's lane.
    /// Returns `false` if the target lane is missing (a lint condition).
    pub fn insert_card(&mut self, status: Status, card: Card, at_top: bool) -> bool {
        let Some(title) = status.lane_title() else {
            return false;
        };
        let Some(lane) = self.lanes.iter_mut().find(|l| l.title == title) else {
            return false;
        };
        if at_top {
            lane.cards.insert(0, card);
        } else {
            lane.cards.push(card);
        }
        true
    }

    /// Move a card to a new status. Active target → bottom of that lane;
    /// terminal target → the archive (checkbox marked). Returns `false` if the
    /// card or a required active lane is missing.
    pub fn move_card(&mut self, id: &CardId, to: Status) -> bool {
        let Some(mut card) = self.remove_card(id) else {
            return false;
        };
        if to.is_terminal() {
            card.set_check(true);
            self.archive.push(card);
            self.archive_present = true;
            true
        } else {
            card.set_check(false);
            let title = to.lane_title().expect("active status has a lane");
            match self.lanes.iter_mut().find(|l| l.title == title) {
                Some(lane) => {
                    lane.cards.push(card);
                    true
                }
                None => false,
            }
        }
    }

    /// Reorder a card within its current lane to the position named by `anchor`
    /// (`Top`/`Bottom`, or `Before`/`After` a sibling). Returns `false` if the
    /// card is not found in an active lane.
    pub fn reorder(&mut self, id: &CardId, anchor: Anchor) -> bool {
        let Some(Location::Lane(idx)) = self.locate(id) else {
            return false;
        };
        let lane = &mut self.lanes[idx];
        let Some(from) = lane.cards.iter().position(|c| c.id().as_ref() == Some(id)) else {
            return false;
        };
        let card = lane.cards.remove(from);
        let target = match anchor {
            Anchor::Top => 0,
            Anchor::Bottom => lane.cards.len(),
            Anchor::Before(ref other) => lane
                .cards
                .iter()
                .position(|c| c.id().as_ref() == Some(other))
                .unwrap_or(lane.cards.len()),
            Anchor::After(ref other) => lane
                .cards
                .iter()
                .position(|c| c.id().as_ref() == Some(other))
                .map(|p| p + 1)
                .unwrap_or(lane.cards.len()),
        };
        lane.cards.insert(target.min(lane.cards.len()), card);
        true
    }

    /// Serialize back to BOARD.md.
    ///
    /// The output is formatter-stable: a board round-tripped through this writer
    /// and then a prettier-style markdown formatter is byte-identical (card
    /// 3e9510). The two things that used to drift are handled here — an empty
    /// lane emits a single blank line (not the `\n\n\n` a formatter collapses),
    /// and the archive separator is `---` (what prettier normalizes `***` to).
    /// Verbatim regions (preamble/frontmatter, settings) round-trip unchanged, so
    /// they stay stable provided the on-disk board was formatted once.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        // A frontmatter-less board has an empty preamble; emitting the `\n\n`
        // separator anyway would open the file with blank lines a formatter
        // strips (card bf452e). Only emit the preamble block when it has content.
        let preamble = self.preamble.trim_end();
        if !preamble.is_empty() {
            out.push_str(preamble);
            out.push_str("\n\n");
        }
        for lane in &self.lanes {
            match lane.max_items {
                Some(n) => out.push_str(&format!("## {} ({n})\n", lane.title)),
                None => out.push_str(&format!("## {}\n", lane.title)),
            }
            push_cards(&mut out, &lane.cards);
            out.push('\n');
        }
        if self.archive_present || !self.archive.is_empty() {
            out.push_str(&format!("{ARCHIVE_SEP}\n\n## {ARCHIVE_HEADING}\n"));
            push_cards(&mut out, &self.archive);
            out.push('\n');
        }
        if let Some(trailer) = &self.trailer {
            out.push_str(trailer);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

/// Where to place a card when reordering.
#[derive(Clone, Debug)]
pub enum Anchor {
    Top,
    Bottom,
    Before(CardId),
    After(CardId),
}

// ---- serialization helpers --------------------------------------------------

/// Emit a lane/archive's cards under an already-written `## Heading\n`: a single
/// blank line between the heading and the first card, then one card per line.
/// An empty lane emits nothing, so the heading is followed only by the caller's
/// trailing blank line — never the double blank a formatter would collapse.
fn push_cards(out: &mut String, cards: &[Card]) {
    for (idx, card) in cards.iter().enumerate() {
        if idx == 0 {
            out.push('\n');
        }
        out.push_str(&card.raw);
        out.push('\n');
    }
}

// ---- line classifiers -------------------------------------------------------

/// A markdown thematic break: a line of three or more matching `-`, `*`, or `_`
/// markers (spaces allowed between them), and nothing else. This is the archive
/// separator (`***`/`---`) plus any human-written rule; parse treats it as
/// skippable decoration in the lane region, so it never counts as the foreign
/// content that stops lane parsing.
fn is_thematic_break(l: &str) -> bool {
    let t = l.trim();
    let marker = match t.chars().next() {
        Some(c @ ('-' | '*' | '_')) => c,
        _ => return false,
    };
    t.chars().filter(|c| !c.is_whitespace()).count() >= 3
        && t.chars().all(|c| c == marker || c == ' ' || c == '\t')
}

fn is_lane_heading(l: &str) -> bool {
    lane_title(l).is_some()
}

/// The clean title of a `## Lane` heading (WIP annotation stripped), for status
/// matching.
fn lane_title(l: &str) -> Option<&str> {
    lane_heading(l).map(|(title, _)| title)
}

/// Parse a `## Lane` heading into `(clean title, optional WIP limit)`. A trailing
/// ` (n)` max-items marker is split off so it can be preserved on re-emit — but
/// only when `n` is a bare number (a title that merely ends in `)` keeps it).
fn lane_heading(l: &str) -> Option<(&str, Option<u32>)> {
    let rest = l.strip_prefix("## ")?.trim();
    if let Some(p) = rest.rfind('(') {
        if let Some(inner) = rest[p..]
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
        {
            if let Ok(n) = inner.trim().parse::<u32>() {
                let title = rest[..p].trim();
                if !title.is_empty() {
                    return Some((title, Some(n)));
                }
            }
        }
    }
    (!rest.is_empty()).then_some((rest, None))
}

fn is_list_item(l: &str) -> bool {
    let t = l.trim_start();
    t.starts_with("- [") || t.starts_with("- ")
}

/// Take a card starting at `i`: its list-item line plus any indented
/// continuation lines. Returns the card and the index just past it.
fn take_card(lines: &[&str], i: usize) -> (Card, usize) {
    let mut raw = String::from(lines[i]);
    let mut j = i + 1;
    while j < lines.len() {
        let l = lines[j];
        // Continuation = an indented, non-empty line that is not a new item.
        if !l.is_empty() && l.starts_with([' ', '\t']) && !is_list_item(l) {
            raw.push('\n');
            raw.push_str(l);
            j += 1;
        } else {
            break;
        }
    }
    (Card { raw }, j)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(t: &str) -> Status {
        Status::from_token(t).unwrap()
    }
    fn id(s: &str) -> CardId {
        CardId::parse(s).unwrap()
    }

    // Prettier-canonical: single-blank frontmatter, single blank lines between
    // lanes, blanks around the settings code-fence. Kept byte-exact so the
    // golden snapshots below double as a formatter-stability guard.
    const BOARD: &str = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n- [ ] [[a3f7b2]]\n- [ ] [[1a2b3c]]\n\n## In Progress\n\n- [ ] [[ff09ab]]\n\n## Needs Review\n\n## Ready\n\n%% kanban:settings\n\n```\n{\"kanban-plugin\":\"basic\"}\n```\n\n%%\n";

    #[test]
    fn parses_lanes_cards_and_settings() {
        let b = Board::parse(BOARD);
        assert_eq!(b.cards_in(st("todo")).len(), 2);
        assert_eq!(b.cards_in(st("in-progress")).len(), 1);
        assert_eq!(b.cards_in(st("needs-review")).len(), 0);
        assert_eq!(b.cards_in(st("todo"))[0].id(), Some(id("a3f7b2")));
        assert!(b.trailer.as_deref().unwrap().contains("kanban:settings"));
        assert!(b.preamble.contains("kanban-plugin: basic"));
    }

    #[test]
    fn id_is_extracted_from_a_bare_link() {
        assert_eq!(Card::new_link(&id("a3f7b2")).id(), Some(id("a3f7b2")));
        // A link with an alias (e.g. hand-added) still resolves the id.
        let b = Board::parse(
            "---\nkanban-plugin: basic\n---\n\n## To-do\n\n- [ ] [[1a2b3c|Some title]]\n",
        );
        assert_eq!(b.cards_in(st("todo"))[0].id(), Some(id("1a2b3c")));
    }

    #[test]
    fn round_trip_preserves_preamble_and_settings() {
        let b = Board::parse(BOARD);
        let out = b.to_text();
        let b2 = Board::parse(&out);
        assert_eq!(b, b2);
        assert!(out.contains("kanban-plugin: basic"));
        assert!(out.contains("%% kanban:settings"));
    }

    #[test]
    fn status_of_reads_the_lane() {
        let b = Board::parse(BOARD);
        assert_eq!(b.status_of(&id("a3f7b2")), Some(st("todo")));
        assert_eq!(b.status_of(&id("ff09ab")), Some(st("in-progress")));
        assert_eq!(b.status_of(&id("000000")), None);
    }

    #[test]
    fn move_between_lanes() {
        let mut b = Board::parse(BOARD);
        assert!(b.move_card(&id("a3f7b2"), st("in-progress")));
        assert_eq!(b.status_of(&id("a3f7b2")), Some(st("in-progress")));
        assert_eq!(b.cards_in(st("todo")).len(), 1);
        assert_eq!(b.cards_in(st("in-progress")).len(), 2);
    }

    #[test]
    fn move_to_terminal_archives_and_checks() {
        let mut b = Board::parse(BOARD);
        assert!(b.move_card(&id("ff09ab"), st("accepted")));
        assert_eq!(b.status_of(&id("ff09ab")), None);
        assert_eq!(b.archived().len(), 1);
        assert!(b.archived()[0].raw.contains("- [x]"));
        // The archive section now serializes, under a `---` separator (the
        // formatter-stable spelling) immediately before the `## Archive` heading.
        assert!(b.to_text().contains("\n---\n\n## Archive\n"));
    }

    #[test]
    fn insert_and_reorder() {
        let mut b = Board::parse(BOARD);
        b.insert_card(st("todo"), Card::new_link(&id("cccccc")), true);
        assert_eq!(b.cards_in(st("todo"))[0].id(), Some(id("cccccc")));

        // Move it to the bottom.
        assert!(b.reorder(&id("cccccc"), Anchor::Bottom));
        let todo = b.cards_in(st("todo"));
        assert_eq!(todo.last().unwrap().id(), Some(id("cccccc")));

        // Place it right after a3f7b2.
        assert!(b.reorder(&id("cccccc"), Anchor::After(id("a3f7b2"))));
        let todo = b.cards_in(st("todo"));
        assert_eq!(todo[0].id(), Some(id("a3f7b2")));
        assert_eq!(todo[1].id(), Some(id("cccccc")));
    }

    #[test]
    fn missing_lane_move_fails_gracefully() {
        // A board with no Ready lane.
        let b = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n- [ ] [[a3f7b2|X]]\n";
        let mut board = Board::parse(b);
        assert!(!board.move_card(&id("a3f7b2"), st("ready")));
    }

    #[test]
    fn preserves_a_multiline_card_and_unknown_lane() {
        let text = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n- [ ] [[a3f7b2|X]]\n  extra human line\n\n## Icebox\n\n- [ ] [[1a2b3c|Y]]\n";
        let b = Board::parse(text);
        assert_eq!(
            b.cards_in(st("todo"))[0].raw,
            "- [ ] [[a3f7b2|X]]\n  extra human line"
        );
        // The unknown "Icebox" lane is preserved on round-trip.
        assert!(b.to_text().contains("## Icebox"));
    }

    // ---- WIP-limit preservation + existing-archive survival --------------------

    #[test]
    fn wip_limit_annotation_survives_a_round_trip() {
        let text = "---\nkanban-plugin: basic\n---\n\n## To-do (3)\n\n- [ ] [[a3f7b2]]\n\n## In Progress\n";
        let b = Board::parse(text);
        assert_eq!(b.lanes()[0].max_items, Some(3));
        // Emitted back verbatim, and a title that merely ends in `)` is untouched.
        assert!(b.to_text().contains("## To-do (3)"));
    }

    #[test]
    fn lane_heading_only_splits_a_numeric_wip_marker() {
        assert_eq!(lane_heading("## To-do (3)"), Some(("To-do", Some(3))));
        // A non-numeric parenthetical stays part of the title.
        assert_eq!(
            lane_heading("## Done (notes)"),
            Some(("Done (notes)", None))
        );
        // A title that merely ends in `)` is kept whole.
        assert_eq!(lane_heading("## Wrap (up)"), Some(("Wrap (up)", None)));
    }

    /// A board that already has a populated archive plus a card in a lane.
    /// Prettier-canonical, including the `---` archive separator prettier
    /// normalizes `***` to.
    const POPULATED_ARCHIVE: &str = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n## In Progress\n\n- [ ] [[ff09ab]]\n\n## Needs Review\n\n## Ready\n\n---\n\n## Archive\n\n- [x] [[dddddd]]\n\n%% kanban:settings\n\n```\n{\"kanban-plugin\":\"basic\"}\n```\n\n%%\n";

    #[test]
    fn existing_archive_content_survives_a_mutation() {
        let mut b = Board::parse(POPULATED_ARCHIVE);
        assert_eq!(b.archived().len(), 1);
        // Archive a second card; the pre-existing one must remain.
        assert!(b.move_card(&id("ff09ab"), st("accepted")));
        let archived: Vec<_> = b.archived().iter().filter_map(|c| c.id()).collect();
        assert!(
            archived.contains(&id("dddddd")),
            "pre-existing archived card kept"
        );
        assert!(
            archived.contains(&id("ff09ab")),
            "newly archived card added"
        );
        // And the settings block round-trips.
        assert!(b.to_text().contains("%% kanban:settings"));
    }

    // ---- archive-separator tolerance (cards 5b4df3 / 3e9510) -------------------

    /// A board whose `## Archive` heading has NO thematic-break separator before
    /// it — exactly what a formatter leaves when it strips/rewrites the `***`.
    const ARCHIVE_NO_SEP: &str = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n- [ ] [[ff09ab]]\n\n## In Progress\n\n## Needs Review\n\n## Ready\n\n## Archive\n\n- [x] [[dddddd]]\n\n%% kanban:settings\n\n```\n{}\n```\n\n%%\n";

    #[test]
    fn archive_is_recognized_without_a_separator() {
        let b = Board::parse(ARCHIVE_NO_SEP);
        // The `## Archive` heading anchors the archive even with no `***`/`---`.
        assert_eq!(b.archived().len(), 1);
        assert_eq!(b.archived()[0].id(), Some(id("dddddd")));
        // ...and it is NOT mistaken for an active lane.
        assert!(!b.lanes().iter().any(|l| l.title == "Archive"));
    }

    #[test]
    fn accepting_into_a_separatorless_archive_does_not_duplicate_it() {
        let mut b = Board::parse(ARCHIVE_NO_SEP);
        assert!(b.move_card(&id("ff09ab"), st("accepted")));
        let out = b.to_text();
        // Exactly one Archive section, holding both the old and new card, and the
        // separator is restored to the formatter-stable `---`.
        assert_eq!(out.matches("## Archive").count(), 1);
        assert!(out.contains("\n---\n\n## Archive\n"));
        let archived: Vec<_> = Board::parse(&out).archived().iter().filter_map(|c| c.id()).collect();
        assert!(archived.contains(&id("dddddd")) && archived.contains(&id("ff09ab")));
    }

    #[test]
    fn duplicate_archive_sections_merge_on_rewrite() {
        // A board corrupted by the old append bug: two `## Archive` sections.
        let corrupt = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n## In Progress\n\n## Needs Review\n\n## Ready\n\n---\n\n## Archive\n\n- [x] [[aaaaaa]]\n\n---\n\n## Archive\n\n- [x] [[bbbbbb]]\n\n%% kanban:settings\n\n```\n{}\n```\n\n%%\n";
        let b = Board::parse(corrupt);
        // Both stranded cards are recovered into the single archive.
        let archived: Vec<_> = b.archived().iter().filter_map(|c| c.id()).collect();
        assert!(archived.contains(&id("aaaaaa")) && archived.contains(&id("bbbbbb")));
        // ...and re-emit collapses to exactly one Archive section.
        assert_eq!(b.to_text().matches("## Archive").count(), 1);
    }

    #[test]
    fn an_active_lane_after_archive_is_not_swallowed() {
        // Out-of-canonical-order board: a live `## Ready` card sits below the
        // `## Archive`. Anchoring on the heading must not archive the live card.
        let out_of_order = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n## In Progress\n\n## Needs Review\n\n---\n\n## Archive\n\n- [x] [[dddddd]]\n\n## Ready\n\n- [ ] [[eeeeee]]\n\n%% kanban:settings\n\n```\n{}\n```\n\n%%\n";
        let b = Board::parse(out_of_order);
        // The live card stays in its active lane, not the archive.
        assert_eq!(b.status_of(&id("eeeeee")), Some(st("ready")));
        assert_eq!(b.archived().iter().filter_map(|c| c.id()).collect::<Vec<_>>(), vec![id("dddddd")]);
    }

    #[test]
    fn old_star_separator_is_read_and_rewritten_stable() {
        // A pre-existing board still using `***`; we read it and emit `---`.
        let starred = POPULATED_ARCHIVE.replace("\n---\n\n## Archive", "\n***\n\n## Archive");
        let b = Board::parse(&starred);
        assert_eq!(b.archived().len(), 1);
        assert!(!b.to_text().contains("***"));
        assert!(b.to_text().contains("\n---\n\n## Archive\n"));
    }

    // ---- formatter stability (card 3e9510) -------------------------------------

    #[test]
    fn no_lane_ever_emits_a_double_blank_line() {
        // Empty lanes are the classic drift source; a triple newline is the
        // signature of a blank line a prettier-style formatter would collapse.
        for src in [BOARD, POPULATED_ARCHIVE, ARCHIVE_NO_SEP] {
            let out = Board::parse(src).to_text();
            assert!(
                !out.contains("\n\n\n"),
                "emitted board has a collapsible double blank line:\n{out}"
            );
        }
        assert!(!crate::project::layout::initial_board().contains("\n\n\n"));
    }

    #[test]
    fn trailing_human_content_survives_a_rewrite() {
        // Prose after the last lane on a board with NO settings block used to be
        // consumed as stray lines and dropped on rewrite (card 0e6516).
        let text = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n- [ ] [[a3f7b2]]\n\n## In Progress\n\n## Needs Review\n\n## Ready\n\nSome human note the tool didn't write.\n";
        let b = Board::parse(text);
        let out = b.to_text();
        assert!(
            out.contains("Some human note the tool didn't write."),
            "trailing content dropped:\n{out}"
        );
        // ...and it round-trips loss-free (idempotent thereafter).
        assert_eq!(Board::parse(&out).to_text(), out);
    }

    #[test]
    fn a_mid_board_stray_line_does_not_swallow_later_lanes() {
        // A stray line BETWEEN lanes must not turn every following lane into
        // opaque trailer text — the later lane and its cards stay modelled.
        let text = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n- [ ] [[a3f7b2]]\n\nstray note\n\n## In Progress\n\n- [ ] [[ff09ab]]\n\n## Needs Review\n\n## Ready\n\n%% kanban:settings\n\n```\n{}\n```\n\n%%\n";
        let b = Board::parse(text);
        // The lane after the stray line is still parsed and its card reachable.
        assert_eq!(b.status_of(&id("ff09ab")), Some(st("in-progress")));
        assert_eq!(b.lanes().len(), 4);
        // The trailer is the settings block, not the whole tail from the stray.
        assert!(b.trailer.as_deref().unwrap().starts_with("%% kanban:settings"));
    }

    #[test]
    fn a_stray_line_before_the_archive_does_not_strand_it() {
        // Prose before `## Archive` must not fold the archive into the trailer
        // (which would revive the card 5b4df3 duplicate-append bug).
        let text = "---\nkanban-plugin: basic\n---\n\n## To-do\n\n## In Progress\n\n## Needs Review\n\n## Ready\n\nleftover prose\n\n---\n\n## Archive\n\n- [x] [[dddddd]]\n\n%% kanban:settings\n\n```\n{}\n```\n\n%%\n";
        let b = Board::parse(text);
        assert_eq!(b.archived().len(), 1);
        assert_eq!(b.archived()[0].id(), Some(id("dddddd")));
        assert!(b.trailer.as_deref().unwrap().starts_with("%% kanban:settings"));
    }

    #[test]
    fn foreign_content_does_not_swallow_the_archive_separator() {
        // The `---`/`***` separator is decoration, not "foreign content": it must
        // still be skipped so the `## Archive` after it parses as the archive.
        assert!(is_thematic_break("---"));
        assert!(is_thematic_break("***"));
        assert!(is_thematic_break("* * *"));
        assert!(!is_thematic_break("-- x"));
        assert!(!is_thematic_break("A note."));
        let b = Board::parse(POPULATED_ARCHIVE);
        assert_eq!(b.archived().len(), 1);
        assert!(b.trailer.as_deref().unwrap().starts_with("%% kanban:settings"));
    }

    #[test]
    fn frontmatter_less_board_emits_no_leading_blank_lines() {
        // With an empty preamble, `to_text` must start at the first lane, not
        // with the `\n\n` a formatter would strip (card bf452e).
        let b = Board::parse("## To-do\n\n- [ ] [[a3f7b2]]\n");
        let out = b.to_text();
        assert!(!out.starts_with('\n'), "leading blank line:\n{out:?}");
        assert!(out.starts_with("## To-do"));
        // Still idempotent for this shape.
        assert_eq!(Board::parse(&out).to_text(), out);
    }

    #[test]
    fn emit_is_idempotent_for_canonical_boards() {
        // A canonical board round-tripped through the writer is byte-identical —
        // the property the prettier gate needs (drift-free CLI rewrites).
        for src in [BOARD, POPULATED_ARCHIVE] {
            let once = Board::parse(src).to_text();
            let twice = Board::parse(&once).to_text();
            assert_eq!(once, twice, "writer is not idempotent for:\n{src}");
            assert_eq!(once, src, "canonical input is not preserved byte-for-byte");
        }
    }

    // ---- golden byte-exact snapshots of BOARD.md mutations ---------------------
    // The parser/writer preserving frontmatter + settings + archive verbatim is
    // the single most fragile guarantee (design §5/§6); these pin the exact
    // serialized bytes, so any format drift the tolerant parser accepts is caught.

    #[test]
    fn golden_initial_board() {
        insta::assert_snapshot!(crate::project::layout::initial_board());
    }

    #[test]
    fn golden_after_move_to_lane() {
        let mut b = Board::parse(BOARD);
        b.move_card(&id("a3f7b2"), st("in-progress"));
        insta::assert_snapshot!(b.to_text());
    }

    #[test]
    fn golden_after_archive_into_populated_archive() {
        let mut b = Board::parse(POPULATED_ARCHIVE);
        b.move_card(&id("ff09ab"), st("accepted"));
        insta::assert_snapshot!(b.to_text());
    }
}
