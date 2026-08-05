//! The persisted launch directive — `directive.md` in an instance's state dir.
//!
//! A prompted launch (`sandbox start --agent --prompt …`, and therefore every
//! `sandbox dispatch`) composes its directive on the host, hands it to the
//! emitted recipe, and then boots QEMU **detached** before tail-calling `prompt`
//! to deliver it. That left the text living only in the launching process's argv
//! and the transient recipe: kill the launch after the VM detaches and you are
//! left with a healthy, idle VM, a card the board says is in-progress, and no
//! record anywhere of what the agent was supposed to do. Recovery meant
//! hand-recomposing the directive from the card plus the project's instructions
//! file — possible only for someone who knows the composition rule.
//!
//! Persisting it turns that into `sandbox prompt <inst> --redeliver`.
//!
//! Lifetime follows the state dir, so it needs no reaping of its own: an
//! ephemeral instance's directive is removed with the rest of its state, and a
//! named instance's survives stop/start exactly as its branch does.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Filename within the per-instance state dir.
const DIRECTIVE_FILE: &str = "directive.md";

/// `<state_dir>/<name>/directive.md`.
pub fn path(state_dir: &Path, name: &str) -> PathBuf {
    state_dir.join(name).join(DIRECTIVE_FILE)
}

/// Persist `text` as the instance's directive, creating the state dir if needed.
pub fn write(state_dir: &Path, name: &str, text: &str) -> Result<()> {
    let p = path(state_dir, name);
    let dir = p.parent().expect("directive path always has a parent");
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating instance state dir {}", dir.display()))?;
    std::fs::write(&p, text).with_context(|| format!("writing directive to {}", p.display()))?;
    Ok(())
}

/// Read the persisted directive, or explain that there is none.
///
/// The error names the two ways to get here — an instance launched without a
/// prompt, and one launched before directives were persisted — because both look
/// identical on disk and the fix differs.
pub fn read(state_dir: &Path, name: &str) -> Result<String> {
    let p = path(state_dir, name);
    let text = std::fs::read_to_string(&p).with_context(|| {
        format!(
            "no persisted directive at {} — instance '{name}' was launched without a --prompt \
             (nothing to redeliver), or predates directive persistence. Send the text explicitly: \
             sandbox prompt {name} \"<text>\"",
            p.display()
        )
    })?;
    if text.trim().is_empty() {
        anyhow::bail!(
            "the persisted directive at {} is empty; send the text explicitly: \
             sandbox prompt {name} \"<text>\"",
            p.display()
        );
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway state root under the test tempdir.
    fn tmp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "katsu-directive-{tag}-{}-{}",
            std::process::id(),
            tag.len()
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn a_written_directive_round_trips_verbatim() {
        let root = tmp_root("roundtrip");
        // Multi-line, with the quoting and markdown a real composed directive
        // carries — redelivery must be byte-identical, not re-escaped.
        let text = "PROJECT RULES: build via `test`.\n\nYou are implementing card `a3f7b2`.\n\n# Add \"the\" thing\n\nDo it.\n";
        write(&root, "card-a3f7b2", text).unwrap();
        assert_eq!(read(&root, "card-a3f7b2").unwrap(), text);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_directive_lands_in_the_instances_own_state_dir() {
        let root = tmp_root("path");
        write(&root, "card-a3f7b2", "x").unwrap();
        assert_eq!(
            path(&root, "card-a3f7b2"),
            root.join("card-a3f7b2").join("directive.md")
        );
        // The reap claim (AC 3) rests entirely on containment: `stop --remove`
        // deletes `<state>/<inst>/`, so anything strictly inside it goes too,
        // and a *named* instance's dir survives stop/start so its directive
        // does. Assert the containment rather than re-deleting the dir here and
        // calling that a test of `stop`.
        let inst_dir = root.join("card-a3f7b2");
        assert!(
            path(&root, "card-a3f7b2").starts_with(&inst_dir),
            "the directive must live inside the reaped per-instance dir"
        );
        assert_eq!(
            path(&root, "card-a3f7b2").parent(),
            Some(inst_dir.as_path()),
            "and directly in it, not in a sibling that reaping would miss"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_directive_explains_how_to_recover() {
        let root = tmp_root("missing");
        let err = read(&root, "inst-x").unwrap_err().to_string();
        assert!(err.contains("no persisted directive"), "{err}");
        assert!(err.contains("sandbox prompt inst-x"), "{err}");
    }

    #[test]
    fn an_empty_directive_is_rejected_rather_than_delivered() {
        let root = tmp_root("empty");
        write(&root, "inst-x", "   \n\n").unwrap();
        let err = read(&root, "inst-x").unwrap_err().to_string();
        assert!(err.contains("empty"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
