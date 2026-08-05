//! Note files: an order-preserving, Obsidian-safe YAML-frontmatter editor plus
//! the typed [`NoteMeta`] view.
//!
//! No YAML crate is vendored, and a serde round-trip would **drop** any
//! frontmatter key a human or Obsidian adds (`aliases`, custom fields). So this
//! is a *line-oriented* editor: it reads the specific keys the domain cares
//! about, edits them surgically, and leaves every other line — order, unknown
//! keys, comments, body — byte-for-byte intact. We only ever *write* scalars
//! post-creation (`disposition`); lists are written once, at `new` time, in
//! flow style, and thereafter only read (flow or block).

use anyhow::{bail, Result};

use super::model::{CardId, Kind, Status};

/// A parsed note: the raw frontmatter lines (between the `---` fences, fences
/// excluded) and the body (everything after the closing fence, verbatim).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    front: Vec<String>,
    body: String,
}

impl Note {
    /// Parse a note. Requires a leading `---` fence and a closing `---`; a file
    /// without frontmatter is an error (every card note has frontmatter).
    pub fn parse(text: &str) -> Result<Note> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text); // tolerate a BOM
        let mut lines = text.lines();
        match lines.next() {
            Some(l) if l.trim_end() == "---" => {}
            _ => bail!("note has no YAML frontmatter (missing opening `---`)"),
        }
        let mut front = Vec::new();
        let mut closed = false;
        for line in lines.by_ref() {
            if line.trim_end() == "---" {
                closed = true;
                break;
            }
            front.push(line.to_string());
        }
        if !closed {
            bail!("note frontmatter is not closed (missing second `---`)");
        }
        let rest: Vec<&str> = lines.collect();
        let body = rest.join("\n");
        Ok(Note { front, body })
    }

    /// The body (everything after the closing fence).
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Serialize back to a full note. Frontmatter fences are re-added; the body
    /// is emitted verbatim after one blank line.
    pub fn to_text(&self) -> String {
        let mut out = String::from("---\n");
        for line in &self.front {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("---\n");
        if !self.body.is_empty() {
            // The body already begins with its own newline structure when it
            // was parsed off a well-formed note; normalize to exactly one blank
            // separator line.
            let body = self.body.trim_start_matches('\n');
            out.push('\n');
            out.push_str(body);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }

    /// Read a top-level scalar value (unquoted). `None` if absent or if the key
    /// carries a list/block value rather than a scalar.
    pub fn get_scalar(&self, key: &str) -> Option<String> {
        let (i, rest) = self.find_key(key)?;
        let rest = rest.trim();
        if rest.is_empty() {
            // Possibly a value Prettier reflowed onto continuation lines.
            let folded = self.folded_continuation(i)?;
            if folded.starts_with('[') {
                return None; // a wrapped flow list is not a scalar
            }
            return Some(unquote(&folded));
        }
        if rest.starts_with('[') {
            return None;
        }
        Some(unquote(rest))
    }

    /// Read a top-level list value, accepting both flow (`key: [a, b]`) and
    /// block (`key:` then `  - a`) styles.
    pub fn get_list(&self, key: &str) -> Vec<String> {
        let Some((i, rest)) = self.find_key(key) else {
            return Vec::new();
        };
        let rest = rest.trim();
        if rest.starts_with('[') {
            return parse_flow_list(rest);
        }
        if rest.is_empty() {
            // A flow list Prettier pushed onto its own indented line reads as an
            // empty inline value; fold it back before falling through to the
            // block-item scan (block items are never a valid folding).
            if let Some(folded) = self.folded_continuation(i) {
                if folded.starts_with('[') {
                    return parse_flow_list(&folded);
                }
                return vec![unquote(&folded)];
            }
            return self.collect_block_items(i + 1);
        }
        // A bare scalar under a list key: treat as a one-element list.
        vec![unquote(rest)]
    }

    /// Locate a top-level key, returning its line index and inline remainder.
    fn find_key(&self, key: &str) -> Option<(usize, &str)> {
        self.front.iter().enumerate().find_map(|(i, line)| {
            line_key(line).and_then(|(k, rest)| (k == key).then_some((i, rest)))
        })
    }

    /// Fold the indented continuation lines that follow the key at `key_line`
    /// into one value, or `None` when there are none.
    ///
    /// Prettier considers a frontmatter value that exceeds its print width
    /// canonical only once reflowed onto indented continuation lines:
    ///
    /// ```yaml
    /// title:
    ///   "Hit points in the heart: damage, the Damaged event, and phase-free
    ///   death"
    /// ```
    ///
    /// That is valid YAML — a multi-line flow scalar folds its lines together
    /// with a single space — but the key's own line now reads as empty, which
    /// used to make every such value parse as absent (a blank card title, or an
    /// emptied `blocked_by`). Reading the continuation keeps the line-oriented
    /// editor and its byte-for-byte preservation guarantee: only the *reader*
    /// grows, and nothing here rewrites the note.
    fn folded_continuation(&self, key_line: usize) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        for line in self.front.get(key_line + 1..)? {
            // An unindented line is the next top-level key; a blank line ends
            // the value.
            if !line.starts_with([' ', '\t']) {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            // Block-list items, nested mappings and comments are real YAML
            // structure, not a wrapped scalar — never fold those into a value.
            if trimmed == "-" || trimmed.starts_with("- ") || trimmed.starts_with('#') {
                break;
            }
            if parts.is_empty() && is_nested_mapping(trimmed) {
                return None;
            }
            parts.push(trimmed);
        }
        let folded = parts.join(" ");
        if folded.is_empty() {
            return None;
        }
        // A folded value that opens a quote without closing it was truncated by
        // something we stopped at (a blank line mid-scalar, say). Handing back
        // `"para one` — a value carrying a stray delimiter — is worse than
        // reporting absence, which `lint`'s `empty-title` then surfaces.
        let opens = folded.starts_with('"') || folded.starts_with('\'');
        if opens && !is_closed_quote(&folded) {
            return None;
        }
        Some(folded)
    }

    /// Collect `  - item` block-list entries starting at line index `from`.
    fn collect_block_items(&self, from: usize) -> Vec<String> {
        let mut items = Vec::new();
        for line in &self.front[from..] {
            let trimmed = line.trim_start();
            if let Some(item) = trimmed.strip_prefix("- ") {
                items.push(unquote(item.trim()));
            } else if trimmed == "-" {
                items.push(String::new());
            } else if line_key(line).is_some() {
                break; // next top-level key ends the block
            } else if trimmed.is_empty() {
                continue;
            } else {
                break;
            }
        }
        items
    }

    /// Upsert a top-level scalar key: replace the first line that carries it, or
    /// append a new line just before the closing fence. Order and all other
    /// lines are preserved.
    pub fn set_scalar(&mut self, key: &str, value: &str) {
        let rendered = format!("{key}: {}", emit_scalar(value));
        let Some((i, rest)) = self.find_key(key) else {
            self.front.push(rendered);
            return;
        };
        // If Prettier had reflowed the old value onto continuation lines, those
        // lines belong to it — drop them with it, or they would be orphaned
        // beneath the new inline value and reparse as garbage.
        let drop_through = if rest.trim().is_empty() {
            self.continuation_end(i)
        } else {
            i + 1
        };
        self.front.splice(i..drop_through, [rendered]);
    }

    /// One past the last continuation line belonging to the key at `key_line`.
    fn continuation_end(&self, key_line: usize) -> usize {
        let mut end = key_line + 1;
        while let Some(line) = self.front.get(end) {
            let trimmed = line.trim();
            if !line.starts_with([' ', '\t']) || trimmed.is_empty() {
                break;
            }
            end += 1;
        }
        end
    }
}

/// The typed projection of a card note's frontmatter. Excludes `status` and
/// `priority` by design — those are BOARD.md's job (design/project.md §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteMeta {
    pub id: CardId,
    pub title: String,
    pub kind: Kind,
    pub blocked_by: Vec<CardId>,
    pub design: Option<String>,
    pub labels: Vec<String>,
    pub created: Option<String>,
    /// Present only once a card is terminal (archived).
    pub disposition: Option<Status>,
    /// The RFC-3339 instant a card crossed into a terminal state — stamped
    /// alongside `disposition`, cleared on a forced reopen.
    pub disposition_at: Option<String>,
}

impl NoteMeta {
    /// Extract the typed view, validating `id` and `type`. Malformed
    /// `blocked_by` entries are dropped (lint surfaces them separately).
    pub fn from_note(note: &Note) -> Result<NoteMeta> {
        let id = note
            .get_scalar("id")
            .and_then(|s| CardId::parse(&s))
            .ok_or_else(|| anyhow::anyhow!("note frontmatter has no valid 6-hex `id:`"))?;
        let title = note.get_scalar("title").unwrap_or_default();
        let kind = note
            .get_scalar("type")
            .and_then(|s| Kind::from_token(&s))
            .unwrap_or_default();
        let blocked_by = note
            .get_list("blocked_by")
            .iter()
            .filter_map(|s| CardId::parse(s))
            .collect();
        let design = note.get_scalar("design").filter(|s| !s.is_empty());
        let labels = note.get_list("labels");
        let created = note.get_scalar("created").filter(|s| !s.is_empty());
        let disposition = note
            .get_scalar("disposition")
            .and_then(|s| Status::from_token(&s));
        let disposition_at = note.get_scalar("disposition_at").filter(|s| !s.is_empty());
        Ok(NoteMeta {
            id,
            title,
            kind,
            blocked_by,
            design,
            labels,
            created,
            disposition,
            disposition_at,
        })
    }
}

/// Render a fresh card note from scratch (used by `new`). Frontmatter keys are
/// emitted in canonical order; the body is the caller-supplied template/piped
/// content.
#[allow(clippy::too_many_arguments)]
pub fn render_new_note(
    id: &CardId,
    title: &str,
    kind: Kind,
    blocked_by: &[CardId],
    design: Option<&str>,
    labels: &[String],
    created: &str,
    body: &str,
) -> String {
    let mut fm = String::new();
    fm.push_str(&format!("id: {id}\n"));
    fm.push_str(&format!("title: {}\n", emit_scalar(title)));
    fm.push_str(&format!("type: {}\n", kind.token()));
    let bl: Vec<String> = blocked_by.iter().map(|c| c.to_string()).collect();
    fm.push_str(&format!("blocked_by: [{}]\n", bl.join(", ")));
    if let Some(d) = design {
        fm.push_str(&format!("design: {}\n", emit_scalar(d)));
    }
    let lb: Vec<String> = labels.iter().map(|l| emit_list_item(l)).collect();
    fm.push_str(&format!("labels: [{}]\n", lb.join(", ")));
    fm.push_str(&format!("created: {created}\n"));

    let body = body.trim_start_matches('\n');
    format!("---\n{fm}---\n\n{body}\n")
}

// ---- line-level YAML helpers (deliberately minimal) -------------------------

/// Parse a top-level `key: rest` line. Returns `(key, rest)` only for an
/// unindented line whose key is `[A-Za-z0-9_-]+` followed by `:`. Indented
/// lines, list items, comments, and blanks yield `None`.
fn line_key(line: &str) -> Option<(&str, &str)> {
    if line.starts_with([' ', '\t', '#', '-']) || line.is_empty() {
        return None;
    }
    let colon = line.find(':')?;
    let key = &line[..colon];
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some((key, &line[colon + 1..]))
}

/// Whether a value that starts with a quote also *ends* with the matching one,
/// i.e. the quoted scalar is complete rather than truncated.
fn is_closed_quote(v: &str) -> bool {
    let Some(q) = v.chars().next() else {
        return false;
    };
    v.len() >= 2 && v.ends_with(q)
}

/// Does an indented line open a nested mapping (`sub: value`) rather than
/// continue a wrapped scalar? Only an *unquoted* `key:` counts — a folded
/// value that Prettier quoted (the case for any title containing `: `) always
/// begins with the quote character.
fn is_nested_mapping(trimmed: &str) -> bool {
    let Some(colon) = trimmed.find(':') else {
        return false;
    };
    let key = &trimmed[..colon];
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && matches!(trimmed[colon + 1..].chars().next(), None | Some(' '))
}

/// Parse a flow list `[a, b, "c d"]` into its items (unquoted).
///
/// An item whose *raw* text is blank is dropped. That is what a **trailing
/// comma** produces, and Prettier emits one whenever a list is long enough to
/// explode one-item-per-line:
///
/// ```yaml
/// labels:
///   [
///     security,
///     tooling,      # <- trailing comma on the last item
///   ]
/// ```
///
/// Without this, every such list gained a phantom empty entry — harmless for
/// `blocked_by` (which filters through `CardId::parse`) but visible on
/// `labels`, which reaches `--json`, `project status`, and the Obsidian card
/// face. The check is on the raw text deliberately: an explicitly empty item is
/// written `""` by [`emit_list_item`], which is not blank before unquoting and
/// therefore still round-trips.
fn parse_flow_list(s: &str) -> Vec<String> {
    let inner = s.trim().trim_start_matches('[').trim_end_matches(']');
    if inner.trim().is_empty() {
        return Vec::new();
    }
    split_flow_items(inner)
        .into_iter()
        .filter(|i| !i.trim().is_empty())
        .map(|i| unquote(i.trim()))
        .collect()
}

/// Split a flow-list interior on top-level commas, respecting double quotes.
fn split_flow_items(inner: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let bytes = inner.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                items.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(&inner[start..]);
    items
}

/// Strip matching surrounding quotes and unescape. Bare values pass through.
fn unquote(raw: &str) -> String {
    let t = raw.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        t[1..t.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        t[1..t.len() - 1].replace("''", "'")
    } else {
        t.to_string()
    }
}

/// Emit a scalar, double-quoting (with escaping) when the value would otherwise
/// be ambiguous or invalid YAML.
fn emit_scalar(val: &str) -> String {
    if scalar_needs_quote(val) {
        format!("\"{}\"", val.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        val.to_string()
    }
}

fn scalar_needs_quote(v: &str) -> bool {
    v.is_empty()
        || v != v.trim()
        || v.contains(": ")
        || v.ends_with(':')
        || v.contains(" #")
        || v.contains('"')
        || v.starts_with([
            '!', '&', '*', '?', '|', '>', '@', '`', '\'', '%', '#', '[', ']', '{', '}', ',', '-',
        ])
}

/// Emit a flow-list item, quoting when it contains a comma, quote, or space.
fn emit_list_item(v: &str) -> String {
    if v.is_empty() || v.contains([',', '"', ' ', '[', ']']) {
        format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nid: a3f7b2\ntitle: Device identity and data root\ntype: feature\nblocked_by: [1a2b3c, ff09ab]\ndesign: PDD005\nlabels: [security, net]\ncreated: 2026-07-17T18:22:04Z\n---\n\n## What to build\nThe thing.\n";

    #[test]
    fn parses_scalars_lists_and_body() {
        let note = Note::parse(SAMPLE).unwrap();
        assert_eq!(note.get_scalar("id").as_deref(), Some("a3f7b2"));
        assert_eq!(
            note.get_scalar("title").as_deref(),
            Some("Device identity and data root")
        );
        assert_eq!(note.get_list("blocked_by"), vec!["1a2b3c", "ff09ab"]);
        assert_eq!(note.get_list("labels"), vec!["security", "net"]);
        assert!(note.body().contains("## What to build"));
    }

    #[test]
    fn typed_meta_extracts_and_validates() {
        let note = Note::parse(SAMPLE).unwrap();
        let meta = NoteMeta::from_note(&note).unwrap();
        assert_eq!(meta.id.as_str(), "a3f7b2");
        assert_eq!(meta.kind, Kind::Feature);
        assert_eq!(meta.blocked_by.len(), 2);
        assert_eq!(meta.design.as_deref(), Some("PDD005"));
        assert_eq!(meta.labels, vec!["security", "net"]);
        assert_eq!(meta.disposition, None);
    }

    #[test]
    fn missing_id_is_an_error() {
        let note = Note::parse("---\ntitle: no id\n---\n").unwrap();
        assert!(NoteMeta::from_note(&note).is_err());
    }

    #[test]
    fn no_frontmatter_is_an_error() {
        assert!(Note::parse("# just a heading\n").is_err());
        assert!(Note::parse("---\nid: x\n(no close)\n").is_err());
    }

    #[test]
    fn block_style_lists_are_read() {
        let text =
            "---\nid: a3f7b2\nblocked_by:\n  - 1a2b3c\n  - ff09ab\nlabels:\n  - security\n---\n";
        let note = Note::parse(text).unwrap();
        assert_eq!(note.get_list("blocked_by"), vec!["1a2b3c", "ff09ab"]);
        assert_eq!(note.get_list("labels"), vec!["security"]);
    }

    #[test]
    fn set_scalar_upserts_and_preserves_unknown_keys() {
        let text = "---\nid: a3f7b2\naliases:\n  - Device\ncustom: keep me\n---\n\nbody\n";
        let mut note = Note::parse(text).unwrap();
        note.set_scalar("disposition", "accepted");
        let out = note.to_text();
        // Unknown human/Obsidian keys survive untouched.
        assert!(out.contains("custom: keep me"));
        assert!(out.contains("aliases:"));
        assert!(out.contains("- Device"));
        // The new key is present exactly once.
        assert_eq!(out.matches("disposition: accepted").count(), 1);
        // Re-setting replaces in place, not appends.
        note.set_scalar("disposition", "cancelled");
        let out2 = note.to_text();
        assert_eq!(out2.matches("disposition:").count(), 1);
        assert!(out2.contains("disposition: cancelled"));
    }

    #[test]
    fn disposition_at_round_trips_through_meta() {
        // Absent on an active card.
        let note = Note::parse(SAMPLE).unwrap();
        assert_eq!(NoteMeta::from_note(&note).unwrap().disposition_at, None);

        // Stamped alongside disposition, it reads back on the typed view.
        let mut note = Note::parse(SAMPLE).unwrap();
        note.set_scalar("disposition", "accepted");
        note.set_scalar("disposition_at", "2026-07-19T00:00:00Z");
        let meta = NoteMeta::from_note(&note).unwrap();
        assert_eq!(meta.disposition, Some(Status::Accepted));
        assert_eq!(meta.disposition_at.as_deref(), Some("2026-07-19T00:00:00Z"));

        // Cleared (empty) on a reopen reads back as None, mirroring disposition.
        note.set_scalar("disposition_at", "");
        assert_eq!(NoteMeta::from_note(&note).unwrap().disposition_at, None);
    }

    #[test]
    fn round_trip_preserves_body() {
        let note = Note::parse(SAMPLE).unwrap();
        let out = note.to_text();
        let reparsed = Note::parse(&out).unwrap();
        assert_eq!(note, reparsed);
    }

    #[test]
    fn render_new_note_is_parseable_and_typed() {
        let id = CardId::parse("a3f7b2").unwrap();
        let text = render_new_note(
            &id,
            "Device identity and data root",
            Kind::Feature,
            &[CardId::parse("1a2b3c").unwrap()],
            Some("PDD005"),
            &["security".to_string()],
            "2026-07-17T18:22:04Z",
            "## What to build\n\n## Acceptance criteria\n- [ ] ...",
        );
        let note = Note::parse(&text).unwrap();
        let meta = NoteMeta::from_note(&note).unwrap();
        assert_eq!(meta.title, "Device identity and data root");
        assert_eq!(meta.blocked_by[0].as_str(), "1a2b3c");
        assert!(note.body().contains("Acceptance criteria"));
    }

    #[test]
    fn tricky_titles_round_trip_through_quoting() {
        for title in [
            "Foo: bar baz",
            "leading - dash",
            "has \"quotes\"",
            "trailing colon:",
        ] {
            let id = CardId::parse("a3f7b2").unwrap();
            let text = render_new_note(
                &id,
                title,
                Kind::Bug,
                &[],
                None,
                &[],
                "2026-01-01T00:00:00Z",
                "b",
            );
            let note = Note::parse(&text).unwrap();
            assert_eq!(
                note.get_scalar("title").as_deref(),
                Some(title),
                "title {title:?}"
            );
        }
    }

    #[test]
    fn quoted_flow_items_with_spaces_round_trip() {
        let id = CardId::parse("a3f7b2").unwrap();
        let text = render_new_note(
            &id,
            "t",
            Kind::Chore,
            &[],
            None,
            &["needs triage".to_string(), "net".to_string()],
            "2026-01-01T00:00:00Z",
            "b",
        );
        let note = Note::parse(&text).unwrap();
        assert_eq!(note.get_list("labels"), vec!["needs triage", "net"]);
    }

    // ---- Prettier-reflowed frontmatter (card 579595) ------------------------
    //
    // Every fixture below is Prettier 3's *actual* output at the shipped
    // `printWidth = 80`, not a hand-guessed shape: run `markdown format` over a
    // card whose value is long enough and this is what lands on disk. Before
    // the folding reader these all parsed as absent, silently and retroactively
    // — the card was filed correctly and a later unrelated format run broke it.

    #[test]
    fn reads_a_quoted_title_prettier_wrapped_onto_one_line() {
        let text = "---\nid: a94bd2\ntitle:\n  \"Hit points in the heart: damage, the Damaged event, and phase-free death\"\ntype: feature\nblocked_by: []\nlabels: []\ncreated: 2026-07-17T18:22:04Z\n---\n\nbody\n";
        let note = Note::parse(text).unwrap();
        assert_eq!(
            note.get_scalar("title").as_deref(),
            Some("Hit points in the heart: damage, the Damaged event, and phase-free death")
        );
        // The keys around the wrapped value must still resolve.
        assert_eq!(note.get_scalar("id").as_deref(), Some("a94bd2"));
        assert_eq!(note.get_scalar("type").as_deref(), Some("feature"));
    }

    #[test]
    fn folds_a_quoted_title_wrapped_across_several_lines() {
        let text = "---\nid: a94bd3\ntitle:\n  \"An extremely long title that goes well beyond any reasonable print width and\n  just keeps on going for a very long time indeed to force multiple wraps\"\ntype: feature\n---\n\nbody\n";
        let note = Note::parse(text).unwrap();
        assert_eq!(
            note.get_scalar("title").as_deref(),
            Some(
                "An extremely long title that goes well beyond any reasonable print width and just keeps on going for a very long time indeed to force multiple wraps"
            )
        );
    }

    #[test]
    fn folds_an_unquoted_title_wrapped_across_several_lines() {
        let text = "---\nid: a94bd4\ntitle:\n  A plain unquoted title that is quite long indeed and exceeds the print width\n  limit for sure\ntype: bug\n---\n\nbody\n";
        let note = Note::parse(text).unwrap();
        assert_eq!(
            note.get_scalar("title").as_deref(),
            Some("A plain unquoted title that is quite long indeed and exceeds the print width limit for sure")
        );
        assert_eq!(note.get_scalar("type").as_deref(), Some("bug"));
    }

    #[test]
    fn reads_flow_lists_prettier_pushed_onto_their_own_line() {
        // Same reflow hits `blocked_by` / `labels`, which emptied the
        // dependency graph rather than just a title.
        let text = "---\nid: a94bd5\nblocked_by:\n  [1a2b3c, 4d5e6f, 7a8b9c, 0d1e2f, 3a4b5c, 6d7e8f, 9a0b1c, 2d3e4f, 5a6b7c]\nlabels:\n  [security, networking, observability, performance, documentation, tooling]\n---\n\nbody\n";
        let note = Note::parse(text).unwrap();
        assert_eq!(
            note.get_list("blocked_by"),
            vec![
                "1a2b3c", "4d5e6f", "7a8b9c", "0d1e2f", "3a4b5c", "6d7e8f", "9a0b1c", "2d3e4f",
                "5a6b7c",
            ]
        );
        assert_eq!(note.get_list("labels").len(), 6);
        // A wrapped flow list is a list, not a scalar.
        assert_eq!(note.get_scalar("blocked_by"), None);
    }

    #[test]
    fn reads_a_flow_list_prettier_exploded_one_item_per_line() {
        // The shape above (a single indented line) only survives inside a
        // narrow width band: one item more and Prettier explodes the list
        // one-per-line **with a trailing comma**, which is by far the more
        // common on-disk form for a real label set. The trailing comma used to
        // yield a phantom empty entry.
        let text = "---\nid: a94be0\nlabels:\n  [\n    security,\n    networking,\n    observability,\n    performance,\n    documentation,\n    tooling,\n    extra,\n    more,\n    yetmore,\n  ]\n---\n\nbody\n";
        let note = Note::parse(text).unwrap();
        assert_eq!(
            note.get_list("labels"),
            vec![
                "security",
                "networking",
                "observability",
                "performance",
                "documentation",
                "tooling",
                "extra",
                "more",
                "yetmore",
            ],
            "a trailing comma must not add a phantom empty label"
        );
    }

    #[test]
    fn an_exploded_blocked_by_list_keeps_every_id_and_adds_none() {
        let text = "---\nid: a94be1\nblocked_by:\n  [\n    1a2b3c,\n    4d5e6f,\n    7a8b9c,\n    0d1e2f,\n    3a4b5c,\n    6d7e8f,\n    9a0b1c,\n    2d3e4f,\n    5a6b7c,\n  ]\n---\n\nbody\n";
        let meta = NoteMeta::from_note(&Note::parse(text).unwrap()).unwrap();
        assert_eq!(meta.blocked_by.len(), 9);
    }

    #[test]
    fn an_explicitly_empty_list_item_still_round_trips() {
        // The trailing-comma filter keys off the *raw* text, so a deliberately
        // empty item (written `""`) must survive it.
        let text = "---\nid: a94be2\nlabels: [\"\", net]\n---\n\nbody\n";
        let note = Note::parse(text).unwrap();
        assert_eq!(note.get_list("labels"), vec!["", "net"]);
    }

    #[test]
    fn a_malformed_folded_scalar_reads_as_absent_rather_than_corrupt() {
        // A blank line inside a quoted scalar is not something Prettier emits,
        // but a hand-edited note can carry it. Folding would stop at the blank
        // and hand back `"para one` — a value with a stray opening quote, which
        // is worse than nothing. Report absence instead, so `project lint`'s
        // `empty-title` catches it.
        let text =
            "---\nid: a94be3\ntitle:\n  \"para one\n\n  para two\"\ntype: bug\n---\n\nbody\n";
        let note = Note::parse(text).unwrap();
        assert_eq!(note.get_scalar("title"), None);
        assert_eq!(note.get_scalar("type").as_deref(), Some("bug"));
    }

    #[test]
    fn a_comment_under_an_empty_key_is_not_folded_into_the_value() {
        let text = "---\nid: a94be4\ndesign:\n  # a note to self\ntitle: fine\n---\n\nbody\n";
        let note = Note::parse(text).unwrap();
        assert_eq!(note.get_scalar("design"), None);
        assert_eq!(note.get_scalar("title").as_deref(), Some("fine"));
    }

    #[test]
    fn typed_meta_survives_a_fully_reflowed_note() {
        let text = "---\nid: a94bd6\ntitle:\n  \"Hit points in the heart: damage, the Damaged event, and phase-free death\"\ntype: feature\nblocked_by:\n  [1a2b3c, 4d5e6f, 7a8b9c, 0d1e2f, 3a4b5c, 6d7e8f, 9a0b1c, 2d3e4f, 5a6b7c]\nlabels:\n  [security, networking, observability, performance, documentation, tooling]\ncreated: 2026-07-17T18:22:04Z\n---\n\nbody\n";
        let meta = NoteMeta::from_note(&Note::parse(text).unwrap()).unwrap();
        assert_eq!(meta.id.as_str(), "a94bd6");
        assert!(!meta.title.is_empty());
        assert_eq!(meta.kind, Kind::Feature);
        assert_eq!(meta.blocked_by.len(), 9);
        assert_eq!(meta.labels.len(), 6);
    }

    #[test]
    fn block_lists_and_nested_maps_are_not_mistaken_for_wrapped_scalars() {
        let text = "---\nid: a94bd7\nblocked_by:\n  - 1a2b3c\n  - 4d5e6f\nnested:\n  sub: value\n  other: thing\ntitle: fine\n---\n\nbody\n";
        let note = Note::parse(text).unwrap();
        assert_eq!(note.get_list("blocked_by"), vec!["1a2b3c", "4d5e6f"]);
        // A block list is not a scalar, and a nested map is not a folded value.
        assert_eq!(note.get_scalar("blocked_by"), None);
        assert_eq!(note.get_scalar("nested"), None);
        assert_eq!(note.get_scalar("title").as_deref(), Some("fine"));
    }

    #[test]
    fn a_key_with_no_value_and_no_continuation_is_still_absent() {
        let text = "---\nid: a94bd8\ndesign:\ntitle: fine\n---\n\nbody\n";
        let note = Note::parse(text).unwrap();
        assert_eq!(note.get_scalar("design"), None);
        assert_eq!(note.get_scalar("title").as_deref(), Some("fine"));
    }

    #[test]
    fn setting_a_wrapped_scalar_replaces_its_continuation_lines() {
        // `set_scalar` rewrites the key's line; the continuation lines are part
        // of the old value and must go with it, or they reparse as garbage.
        let text = "---\nid: a94bd9\ntitle:\n  \"An extremely long title that goes well beyond any reasonable print width and\n  just keeps on going for a very long time indeed to force multiple wraps\"\ntype: feature\n---\n\nbody\n";
        let mut note = Note::parse(text).unwrap();
        note.set_scalar("title", "short");
        assert_eq!(note.get_scalar("title").as_deref(), Some("short"));
        assert_eq!(note.get_scalar("type").as_deref(), Some("feature"));
        let out = note.to_text();
        assert!(out.contains("title: short"), "{out}");
        assert!(!out.contains("just keeps on going"), "{out}");
        // Re-parsing the written note is stable.
        let again = Note::parse(&out).unwrap();
        assert_eq!(again.get_scalar("title").as_deref(), Some("short"));
        assert_eq!(again.get_scalar("type").as_deref(), Some("feature"));
    }

    #[test]
    fn setting_an_unwrapped_scalar_leaves_neighbours_alone() {
        let mut note = Note::parse(SAMPLE).unwrap();
        note.set_scalar("title", "renamed");
        assert_eq!(note.get_scalar("title").as_deref(), Some("renamed"));
        assert_eq!(note.get_scalar("type").as_deref(), Some("feature"));
        assert_eq!(note.get_list("blocked_by"), vec!["1a2b3c", "ff09ab"]);
        assert_eq!(note.get_scalar("design").as_deref(), Some("PDD005"));
    }
}
