//! The Obsidian-Kanban BOARD.md parser/writer — the authoritative store of
//! lifecycle (lane) and priority (position).
//!
//! On-disk contract (verified against the plugin's `src/parsers/common.ts`):
//! frontmatter carries `kanban-plugin`; lanes are `## Heading`; cards are
//! `- [ ] …` list items; the archive is the `***` separator + `## Archive` +
//! items; settings are a trailing `%% kanban:settings` code-fence block.
//!
//! Two regions are preserved **verbatim** so nothing a human/Obsidian writes is
//! lost: the `preamble` (everything up to the first lane — frontmatter and all)
//! and the `settings` block. Lanes and the archive are modelled and re-emitted.

use super::model::{CardId, Status};

/// The `kanban-plugin` frontmatter value. `basic` is recognised by every plugin
/// version (current writes `board` but still reads `basic`); we emit the
/// widest-compatible spelling.
pub const KANBAN_PLUGIN_VALUE: &str = "basic";
const ARCHIVE_SEP: &str = "***";
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
    /// The `%% kanban:settings … %%` block, verbatim (`None` if absent).
    settings: Option<String>,
}

impl Board {
    /// Parse BOARD.md. Tolerant: unknown lanes are kept; the preamble and
    /// settings block are captured verbatim.
    pub fn parse(text: &str) -> Board {
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;

        // Preamble: up to the first lane heading, archive separator, or settings.
        let mut preamble = String::new();
        while i < lines.len() {
            let l = lines[i];
            if is_lane_heading(l) || l.trim_end() == ARCHIVE_SEP || l.starts_with(SETTINGS_MARKER) {
                break;
            }
            preamble.push_str(l);
            preamble.push('\n');
            i += 1;
        }

        // Lanes.
        let mut lanes: Vec<Lane> = Vec::new();
        while i < lines.len() {
            let l = lines[i];
            if l.trim_end() == ARCHIVE_SEP || l.starts_with(SETTINGS_MARKER) {
                break;
            }
            if let Some((title, max_items)) = lane_heading(l) {
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
            i += 1; // blanks / stray lines between cards
        }

        // Archive (after `***`, under `## Archive`).
        let mut archive: Vec<Card> = Vec::new();
        let mut archive_present = false;
        if i < lines.len() && lines[i].trim_end() == ARCHIVE_SEP {
            archive_present = true;
            i += 1;
            while i < lines.len() && !lines[i].starts_with(SETTINGS_MARKER) {
                let l = lines[i];
                if is_list_item(l) {
                    let (card, next) = take_card(&lines, i);
                    archive.push(card);
                    i = next;
                    continue;
                }
                i += 1; // the `## Archive` heading, blanks
            }
        }

        // Settings: verbatim to EOF.
        let settings = if i < lines.len() && lines[i].starts_with(SETTINGS_MARKER) {
            Some(lines[i..].join("\n"))
        } else {
            None
        };

        Board {
            preamble,
            lanes,
            archive,
            archive_present,
            settings,
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
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(self.preamble.trim_end());
        out.push_str("\n\n");
        for lane in &self.lanes {
            match lane.max_items {
                Some(n) => out.push_str(&format!("## {} ({n})\n\n", lane.title)),
                None => out.push_str(&format!("## {}\n\n", lane.title)),
            }
            for card in &lane.cards {
                out.push_str(&card.raw);
                out.push('\n');
            }
            out.push('\n');
        }
        if self.archive_present || !self.archive.is_empty() {
            out.push_str(&format!("{ARCHIVE_SEP}\n\n## {ARCHIVE_HEADING}\n\n"));
            for card in &self.archive {
                out.push_str(&card.raw);
                out.push('\n');
            }
            out.push('\n');
        }
        if let Some(settings) = &self.settings {
            out.push_str(settings);
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

// ---- line classifiers -------------------------------------------------------

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

    const BOARD: &str = "---\n\nkanban-plugin: basic\n\n---\n\n## To-do\n\n- [ ] [[a3f7b2]]\n- [ ] [[1a2b3c]]\n\n## In Progress\n\n- [ ] [[ff09ab]]\n\n## Needs Review\n\n## Ready\n\n\n%% kanban:settings\n```\n{\"kanban-plugin\":\"basic\"}\n```\n%%\n";

    #[test]
    fn parses_lanes_cards_and_settings() {
        let b = Board::parse(BOARD);
        assert_eq!(b.cards_in(st("todo")).len(), 2);
        assert_eq!(b.cards_in(st("in-progress")).len(), 1);
        assert_eq!(b.cards_in(st("needs-review")).len(), 0);
        assert_eq!(b.cards_in(st("todo"))[0].id(), Some(id("a3f7b2")));
        assert!(b.settings.as_deref().unwrap().contains("kanban:settings"));
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
        // The archive section now serializes.
        assert!(b.to_text().contains("***"));
        assert!(b.to_text().contains("## Archive"));
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
    const POPULATED_ARCHIVE: &str = "---\n\nkanban-plugin: basic\n\n---\n\n## To-do\n\n## In Progress\n\n- [ ] [[ff09ab]]\n\n## Needs Review\n\n## Ready\n\n***\n\n## Archive\n\n- [x] [[dddddd]]\n\n%% kanban:settings\n```\n{\"kanban-plugin\":\"basic\"}\n```\n%%\n";

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
