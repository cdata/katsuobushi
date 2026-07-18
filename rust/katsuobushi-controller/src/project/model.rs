//! The `project` domain's core value types: the lifecycle [`Status`], the card
//! [`Kind`] (feature/bug/chore/docs), and the [`CardId`]. All pure — no IO, no
//! frontmatter, no board — so the vocabulary the rest of the domain speaks is
//! defined and tested in one place.
//!
//! The central asymmetry lives here: **four active statuses map to BOARD.md
//! lanes, two terminal statuses do not** (they live in the Kanban archive with
//! the outcome stamped on the note as `disposition`). [`Status::lane_title`]
//! encodes that — it returns `Some(lane)` for active states and `None` for
//! terminal ones.

use std::fmt;

/// A card's lifecycle status. Active states are draggable lanes on the board;
/// terminal states are archived. The kebab `token` is the CLI/frontmatter
/// spelling; the `lane_title` is the `## Heading` spelling in BOARD.md.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    Todo,
    InProgress,
    NeedsReview,
    Ready,
    Accepted,
    Cancelled,
}

impl Status {
    /// Every status, in lifecycle order (terminal states last).
    pub const ALL: [Status; 6] = [
        Status::Todo,
        Status::InProgress,
        Status::NeedsReview,
        Status::Ready,
        Status::Accepted,
        Status::Cancelled,
    ];

    /// The four active statuses, in board order — these are the lanes.
    pub const ACTIVE: [Status; 4] = [
        Status::Todo,
        Status::InProgress,
        Status::NeedsReview,
        Status::Ready,
    ];

    /// The kebab-case spelling used on the CLI and in `disposition:`.
    pub fn token(self) -> &'static str {
        match self {
            Status::Todo => "todo",
            Status::InProgress => "in-progress",
            Status::NeedsReview => "needs-review",
            Status::Ready => "ready",
            Status::Accepted => "accepted",
            Status::Cancelled => "cancelled",
        }
    }

    /// Parse a kebab token back into a status.
    pub fn from_token(s: &str) -> Option<Status> {
        Status::ALL.into_iter().find(|st| st.token() == s)
    }

    /// The `## Lane` heading a card of this status sits under on the board, or
    /// `None` for a terminal status (which is archived, not laned).
    pub fn lane_title(self) -> Option<&'static str> {
        match self {
            Status::Todo => Some("To-do"),
            Status::InProgress => Some("In Progress"),
            Status::NeedsReview => Some("Needs Review"),
            Status::Ready => Some("Ready"),
            Status::Accepted | Status::Cancelled => None,
        }
    }

    /// Resolve a `## Lane` heading back into its active status.
    pub fn from_lane_title(title: &str) -> Option<Status> {
        Status::ACTIVE
            .into_iter()
            .find(|st| st.lane_title() == Some(title))
    }

    /// Terminal statuses are archived and immutable-ish (`accepted` fully so;
    /// `cancelled` is a tombstone). They carry no lane.
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Accepted | Status::Cancelled)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

/// The traditional issue kind. Maps 1:1 to a Conventional-Commit prefix
/// (feature→feat, bug→fix, chore→chore, docs→docs) so the commit that closes a
/// card inherits it — that mapping is documented in the skill and applied by
/// Milestone 2's dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Feature,
    Bug,
    Chore,
    Docs,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Feature, Kind::Bug, Kind::Chore, Kind::Docs];

    /// The `type:` frontmatter spelling.
    pub fn token(self) -> &'static str {
        match self {
            Kind::Feature => "feature",
            Kind::Bug => "bug",
            Kind::Chore => "chore",
            Kind::Docs => "docs",
        }
    }

    pub fn from_token(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.token() == s)
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

/// A card's immutable hex identity — the note filename (`issues/<id>.md`), the
/// `id:` frontmatter, and the `[[<id>|…]]` board link target. Six lowercase hex
/// chars (16.7M space): collision-free enough for the parallel/AFK card
/// creation a sandbox swarm does, with no coordination — we are not chasing
/// global uniqueness.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CardId(String);

impl CardId {
    /// Wrap a string as a `CardId` iff it is exactly 6 lowercase hex digits.
    pub fn parse(s: &str) -> Option<CardId> {
        let ok = s.len() == 6
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        ok.then(|| CardId(s.to_string()))
    }

    /// Mint an id from the low 24 bits of the supplied entropy (the generator is
    /// a seam — deterministic in tests).
    pub fn from_u32(bits: u32) -> CardId {
        CardId(format!("{:06x}", bits & 0x00ff_ffff))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_tokens_round_trip() {
        for st in Status::ALL {
            assert_eq!(Status::from_token(st.token()), Some(st));
        }
        assert_eq!(Status::from_token("bogus"), None);
    }

    #[test]
    fn active_statuses_have_lanes_and_terminal_do_not() {
        for st in Status::ACTIVE {
            assert!(!st.is_terminal());
            let lane = st.lane_title().expect("active status has a lane");
            assert_eq!(Status::from_lane_title(lane), Some(st));
        }
        for st in [Status::Accepted, Status::Cancelled] {
            assert!(st.is_terminal());
            assert_eq!(st.lane_title(), None);
        }
    }

    #[test]
    fn lane_titles_are_the_expected_spellings() {
        assert_eq!(Status::Todo.lane_title(), Some("To-do"));
        assert_eq!(Status::InProgress.lane_title(), Some("In Progress"));
        assert_eq!(Status::NeedsReview.lane_title(), Some("Needs Review"));
        assert_eq!(Status::Ready.lane_title(), Some("Ready"));
        assert_eq!(Status::from_lane_title("Nonsense"), None);
    }

    #[test]
    fn kind_tokens_round_trip_and_default() {
        for k in Kind::ALL {
            assert_eq!(Kind::from_token(k.token()), Some(k));
        }
        assert_eq!(Kind::default(), Kind::Feature);
        assert_eq!(Kind::from_token("epic"), None);
    }

    #[test]
    fn card_id_parse_is_strict() {
        assert!(CardId::parse("a3f7b2").is_some());
        assert!(CardId::parse("A3F7B2").is_none()); // uppercase rejected
        assert!(CardId::parse("a3f7b").is_none()); // too short
        assert!(CardId::parse("a3f7b2c").is_none()); // too long
        assert!(CardId::parse("a3f7g2").is_none()); // non-hex
        assert_eq!(CardId::from_u32(0x000000ff).as_str(), "0000ff");
        assert_eq!(CardId::from_u32(0xdeadbeef).as_str(), "adbeef"); // low 24 bits
    }
}
