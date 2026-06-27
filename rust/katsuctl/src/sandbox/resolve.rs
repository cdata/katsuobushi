//! Shared instance resolution (design/katsuctl.md §4, issue #005).
//!
//! Every sandbox subcommand that takes an `<instance>` argument resolves it the
//! same way the prior-art shell did (`instanceHelpers`,
//! lib/sandbox/default.nix:1680-1710): an all-digit argument is a 1-based index
//! into the state directories sorted in `LC_ALL=C` (byte) order; anything else
//! is a literal instance name (every real instance name carries a `-`, so names
//! are never all-digits). Lives in its own module so the concurrently-developed
//! `instance.json` model (#010/#012) can grow alongside without colliding.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::sandbox::host::Host;

/// Resolve `arg` to a concrete instance name under `state_glob`.
///
/// All-digit `arg` → a 1-based index into the byte-sorted directory listing
/// (error if out of range). Otherwise `arg` is a literal name and must name an
/// existing state directory (error if not). Mirrors `_resolve_instance` /
/// `_list_instances` (lib/sandbox/default.nix:1680-1710).
pub fn resolve_instance(state_glob: &Path, host: &impl Host, arg: &str) -> Result<String> {
    if is_index(arg) {
        return resolve_index(state_glob, arg);
    }
    // A literal name must have a state directory — checked through the seam so a
    // test can answer it without a real filesystem.
    if host.exists(&state_glob.join(arg)) {
        Ok(arg.to_string())
    } else {
        bail!("no sandbox named {arg:?} (see sandbox:status)")
    }
}

/// An argument is an index iff it is non-empty and all ASCII digits — the same
/// `*[!0-9]*` test the shell uses (lib/sandbox/default.nix:1692).
fn is_index(arg: &str) -> bool {
    !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit())
}

/// Map a 1-based `arg` index onto the sorted listing.
fn resolve_index(state_glob: &Path, arg: &str) -> Result<String> {
    let instances = list_instances(state_glob)?;
    // A digit string larger than `usize` is necessarily out of range.
    let index: usize = arg
        .parse()
        .with_context(|| format!("sandbox index {arg} is too large to resolve"))?;
    match index.checked_sub(1).and_then(|i| instances.get(i)) {
        Some(name) => Ok(name.clone()),
        None => bail!(
            "no sandbox at index {arg} (see sandbox:status; {} instance(s) present)",
            instances.len()
        ),
    }
}

/// Enumerate the immediate subdirectories of `state_glob` (each is an instance
/// name), sorted by raw bytes — the Rust equivalent of `_list_instances`'
/// `for d in glob/*/ … | LC_ALL=C sort` (lib/sandbox/default.nix:1681-1687). A
/// missing root is an empty list, not an error (matching the shell's
/// `[ -d ] || return 0`).
fn list_instances(state_glob: &Path) -> Result<Vec<String>> {
    let entries = match std::fs::read_dir(state_glob) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| {
                format!("listing sandbox instances under {}", state_glob.display())
            })
        }
    };

    let mut names = Vec::new();
    for entry in entries {
        let entry =
            entry.with_context(|| format!("reading an entry under {}", state_glob.display()))?;
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
    }
    // `LC_ALL=C sort` is byte ordering; sort the UTF-8 names by their raw bytes
    // so this matches the shell listing the indices are numbered against.
    names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host::FakeHost;
    use std::path::PathBuf;

    /// A fresh, empty temp state root containing the named (initially unsorted)
    /// instance directories. Labelled per test so concurrent runs never collide.
    fn temp_state(label: &str, dirs: &[&str]) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("katsuctl-resolve-{}-{}", std::process::id(), label));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create temp state root");
        for d in dirs {
            std::fs::create_dir_all(base.join(d)).expect("create instance dir");
        }
        base
    }

    #[test]
    fn it_resolves_a_literal_name_that_exists() {
        let state = PathBuf::from("/state/cdata/katsuobushi");
        let mut host = FakeHost::new();
        host.with_existing(state.join("inst-abc"));
        assert_eq!(
            resolve_instance(&state, &host, "inst-abc").unwrap(),
            "inst-abc"
        );
    }

    #[test]
    fn it_errors_on_a_literal_name_that_is_missing() {
        let state = PathBuf::from("/state/cdata/katsuobushi");
        let host = FakeHost::new();
        let err =
            resolve_instance(&state, &host, "nope-xyz").expect_err("missing name must fail loud");
        assert!(format!("{err:#}").contains("nope-xyz"));
    }

    #[test]
    fn it_resolves_an_in_range_index_in_byte_order() {
        // On-disk order is irrelevant: resolution sorts by bytes first.
        let state = temp_state("in-range", &["inst-c", "inst-a", "inst-b"]);
        let host = FakeHost::new();
        assert_eq!(resolve_instance(&state, &host, "1").unwrap(), "inst-a");
        assert_eq!(resolve_instance(&state, &host, "2").unwrap(), "inst-b");
        assert_eq!(resolve_instance(&state, &host, "3").unwrap(), "inst-c");
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn it_errors_on_an_out_of_range_index() {
        let state = temp_state("out-of-range", &["inst-a", "inst-b"]);
        let host = FakeHost::new();
        let err = resolve_instance(&state, &host, "5").expect_err("index past the end must fail");
        assert!(format!("{err:#}").contains("index 5"));
        // 1-based: index 0 is never valid.
        assert!(resolve_instance(&state, &host, "0").is_err());
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn it_treats_a_missing_state_root_as_zero_instances() {
        let state = PathBuf::from("/state/does/not/exist");
        let host = FakeHost::new();
        let err = resolve_instance(&state, &host, "1").expect_err("no instances -> out of range");
        assert!(format!("{err:#}").contains("index 1"));
    }
}
