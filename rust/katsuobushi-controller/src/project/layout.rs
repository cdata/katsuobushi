//! Board-directory layout: file paths, the `init` scaffolding templates, the
//! note catalog, and id resolution. The one place that knows where things live
//! under `--board-dir`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use super::board::KANBAN_PLUGIN_VALUE;
use super::fs::Fs;
use super::model::CardId;
use super::note::{Note, NoteMeta};

/// The board file (`BOARD.md`).
pub const BOARD_FILE: &str = "BOARD.md";
const README_FILE: &str = "README.md";
const TEMPLATE_FILE: &str = ".card-template.md";
/// Card notes live in this subdirectory of the board dir, keeping the top level
/// to just BOARD.md + docs. Obsidian resolves the bare `[[hex-slug]]` links to
/// them by name regardless of folder.
const ISSUES_DIR: &str = "issues";

/// Resolved paths under a board directory.
pub struct Paths {
    dir: PathBuf,
}

impl Paths {
    pub fn new(board_dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: board_dir.into(),
        }
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn board_md(&self) -> PathBuf {
        self.dir.join(BOARD_FILE)
    }
    pub fn readme(&self) -> PathBuf {
        self.dir.join(README_FILE)
    }
    pub fn template(&self) -> PathBuf {
        self.dir.join(TEMPLATE_FILE)
    }
    /// The `issues/` subdirectory that holds the card notes.
    pub fn issues_dir(&self) -> PathBuf {
        self.dir.join(ISSUES_DIR)
    }
    pub fn note(&self, id: &CardId) -> PathBuf {
        self.issues_dir().join(format!("{id}.md"))
    }
}

/// One card note on disk.
pub struct NoteEntry {
    pub filename: String,
    pub note: Note,
    pub meta: Result<NoteMeta>,
}

impl NoteEntry {
    /// The id from the parsed frontmatter, falling back to the filename stem.
    pub fn id(&self) -> Option<CardId> {
        if let Ok(meta) = &self.meta {
            return Some(meta.id.clone());
        }
        self.filename.get(..6).and_then(CardId::parse)
    }
}

/// Load and parse every `NNNNNNNN-*.md` card note in the board dir (excludes
/// BOARD.md, README.md, and the dotfile template). Parse failures are carried
/// on the entry (as `meta: Err`) rather than aborting, so `lint` can report
/// them.
pub fn load_notes(fs: &dyn Fs, paths: &Paths) -> Result<Vec<NoteEntry>> {
    let mut entries = Vec::new();
    let issues = paths.issues_dir();
    let names = match fs.list_files(&issues) {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(e) => return Err(e.into()),
    };
    let mut names: Vec<String> = names.into_iter().filter(|n| is_card_filename(n)).collect();
    names.sort();
    for filename in names {
        let text = fs.read(&issues.join(&filename))?;
        let (note, meta) = match Note::parse(&text) {
            Ok(note) => {
                let meta = NoteMeta::from_note(&note);
                (note, meta)
            }
            Err(e) => (
                // A note that won't even parse: keep a placeholder so lint sees it.
                Note::parse("---\nid: 00000000\n---\n").unwrap(),
                Err(e),
            ),
        };
        entries.push(NoteEntry {
            filename,
            note,
            meta,
        });
    }
    Ok(entries)
}

/// Whether a filename is a card note: `<6 hex>.md`.
fn is_card_filename(name: &str) -> bool {
    name.len() == 9 && &name[6..] == ".md" && CardId::parse(&name[..6]).is_some()
}

/// Resolve a user-supplied id or unique prefix against the known card ids.
pub fn resolve_id(input: &str, known: &[CardId]) -> Result<CardId> {
    if let Some(exact) = CardId::parse(input) {
        if known.contains(&exact) {
            return Ok(exact);
        }
        bail!("no card {input} on this board");
    }
    let matches: Vec<&CardId> = known
        .iter()
        .filter(|c| c.as_str().starts_with(input))
        .collect();
    match matches.as_slice() {
        [] => bail!("no card matches '{input}'"),
        [one] => Ok((*one).clone()),
        many => bail!(
            "'{input}' is ambiguous: {}",
            many.iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

// ---- init scaffolding -------------------------------------------------------

/// The initial BOARD.md: four empty active lanes + a settings block wiring the
/// `metadata keys` (so the plugin surfaces note frontmatter on the card face).
pub fn initial_board() -> String {
    format!(
        "---\n\nkanban-plugin: {KANBAN_PLUGIN_VALUE}\n\n---\n\n## To-do\n\n## In Progress\n\n## Needs Review\n\n## Ready\n\n\n{}\n",
        settings_block()
    )
}

/// The `%% kanban:settings` block. Metadata keys surface the note's fields on
/// the card face — derive-don't-copy (design/project.md §6).
fn settings_block() -> String {
    // The bare `[[<id>]]` link shows only the id; these keys surface the note's
    // fields on the card face, live from frontmatter — `title` first, so the
    // human title reads as the card's main text under the id.
    let settings = serde_json::json!({
        "kanban-plugin": KANBAN_PLUGIN_VALUE,
        "metadata-keys": [
            {"metadataKey": "title", "label": "", "shouldHideLabel": true, "containsMarkdown": false},
            {"metadataKey": "type", "label": "type", "shouldHideLabel": false, "containsMarkdown": false},
            {"metadataKey": "design", "label": "design", "shouldHideLabel": false, "containsMarkdown": false},
            {"metadataKey": "blocked_by", "label": "blocked by", "shouldHideLabel": false, "containsMarkdown": false},
            {"metadataKey": "labels", "label": "labels", "shouldHideLabel": false, "containsMarkdown": false}
        ]
    });
    format!("%% kanban:settings\n```\n{settings}\n```\n%%")
}

/// The body skeleton `new` writes when no body is piped, and the Obsidian Note
/// template `init` scaffolds.
pub fn card_body_template() -> &'static str {
    "## What to build\n\n<!-- What this card delivers. Cite its design doc via the `design:` field if any. -->\n\n## Acceptance criteria\n\n- [ ] \n\n## Review notes\n\n<!-- Reviewers and the owner append context here on a bounce or return. -->\n"
}

/// The board README (workflow guide).
pub fn readme() -> String {
    format!(
        r#"# Project board

An Obsidian-Kanban-native backlog managed by the `project` command.
**`{BOARD_FILE}` is the source of truth for lifecycle and
priority** — its lanes are the status and a card's vertical position is its
priority. Card notes live in `issues/` and hold everything else (title, type,
`blocked_by`, detail).

- **See the board:** `project status` (all cards) or `project status <id>` (one).
- **Move a card:** `project status set <id> <status>` — enforces the state
  machine. Or drag it in Obsidian (unconstrained by the tool).
- **Reprioritize:** `project prioritize <id> --top|--before <id>|--after <id>`,
  or drag within a lane.
- **New card:** `project new --title "..."` (mints the note + a card in To-do).
- **Check health:** `project lint` (board <-> note consistency).

## Lifecycle

```
To-do -> In Progress -> Needs Review -> Ready -> Accepted
```

`needs-review -> in-progress` is a reviewer bounce; `ready -> todo` is an owner
return. `cancelled` is reachable from any non-accepted state. Accepted and
Cancelled cards move to the Kanban **archive** with the outcome recorded on the
note as `disposition:`.

**Only a human (the product owner) moves a card to `accepted`.**

Do not hand-edit `{BOARD_FILE}`'s structure expecting the tool to reconcile it;
edit via the CLI or drag in Obsidian.
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::board::Board;

    #[test]
    fn is_card_filename_is_strict() {
        assert!(is_card_filename("a3f7b2.md"));
        assert!(!is_card_filename("BOARD.md"));
        assert!(!is_card_filename("README.md"));
        assert!(!is_card_filename(".card-template.md"));
        assert!(!is_card_filename("a3f7b2c1-slug.md")); // old slugged form
        assert!(!is_card_filename("nothex.md"));
        assert!(!is_card_filename("a3f7b.md")); // too short
    }

    #[test]
    fn initial_board_parses_and_has_four_lanes_plus_settings() {
        let b = Board::parse(&initial_board());
        assert_eq!(b.lanes().len(), 4);
        assert!(initial_board().contains("%% kanban:settings"));
        assert!(initial_board().contains("metadata-keys"));
        // Round-trips cleanly.
        assert_eq!(Board::parse(&b.to_text()), b);
    }

    #[test]
    fn resolve_id_handles_exact_prefix_and_ambiguity() {
        let known = vec![
            CardId::parse("a3f7b2").unwrap(),
            CardId::parse("a3f7ff").unwrap(),
            CardId::parse("1a2b3c").unwrap(),
        ];
        assert_eq!(resolve_id("1a2b3c", &known).unwrap().as_str(), "1a2b3c");
        assert_eq!(resolve_id("1a2b", &known).unwrap().as_str(), "1a2b3c");
        assert!(resolve_id("a3f7", &known).is_err()); // ambiguous
        assert!(resolve_id("deadbe", &known).is_err()); // absent
    }
}
