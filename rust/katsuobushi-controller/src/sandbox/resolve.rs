//! Shared instance resolution.
//!
//! Every sandbox subcommand that takes an `<instance>` argument resolves it the
//! same way the prior-art shell did (`instanceHelpers`,
//! ): an all-digit argument is a 1-based index
//! into the state directories sorted in `LC_ALL=C` (byte) order; anything else
//! is a literal instance name (every real instance name carries a `-`, so names
//! are never all-digits). Lives in its own module so the concurrently-developed
//! `instance.json` model can grow alongside without colliding.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::sandbox::host::Host;

/// Resolve `arg` to a concrete instance name under `state_glob`.
///
/// All-digit `arg` → a 1-based index into the byte-sorted directory listing
/// (error if out of range). Otherwise `arg` is a literal name and must name an
/// existing state directory (error if not). Mirrors `_resolve_instance` /
/// `_list_instances`.
pub fn resolve_instance(state_glob: &Path, host: &impl Host, arg: &str) -> Result<String> {
    // An empty selector would otherwise fall into the literal-name branch and
    // resolve to the state root itself (`Path::join("")`), failing only later at
    // the git layer. Reject it up front with a usage-style error, mirroring the
    // old wrapper guard.
    if arg.is_empty() {
        bail!("usage: an instance name or 1-based index is required (see sandbox:status)");
    }
    if is_index(arg) {
        return resolve_index(state_glob, host, arg);
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
/// `*[!0-9]*` test the shell uses.
fn is_index(arg: &str) -> bool {
    !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit())
}

/// Map a 1-based `arg` index onto the sorted listing.
fn resolve_index(state_glob: &Path, host: &impl Host, arg: &str) -> Result<String> {
    let instances = list_instances(state_glob, host)?;
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
/// `for d in glob/*/ … | LC_ALL=C sort`.
/// Routed through the [`Host`] seam (rather than `std::fs` directly) so index
/// resolution is `FakeHost`-testable. A missing root is an empty list, not an
/// error (matching the shell's `[ -d ] || return 0`).
///
/// Public so `sandbox status` can number its listing in exactly the order index
/// resolution counts against — one shared enumeration keeps the `#` printed by
/// `status` and the index every other command accepts denoting the same instance
/// (the `_list_instances`/`_resolve_instance` parity).
pub fn list_instances(state_glob: &Path, host: &impl Host) -> Result<Vec<String>> {
    let mut names = match host.list_dir(state_glob) {
        Ok(names) => names,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| {
                format!("listing sandbox instances under {}", state_glob.display())
            })
        }
    };

    // The shell glob `"${stateGlob}"/*/` excludes dot-prefixed directories;
    // `read_dir` does not, so drop them here to keep byte-sorted 1-based indices
    // aligned with `_list_instances`.
    names.retain(|name| !name.starts_with('.'));
    // `LC_ALL=C sort` is byte ordering; sort the UTF-8 names by their raw bytes
    // so this matches the shell listing the indices are numbered against.
    names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::host::{Call, FakeHost};
    use std::path::PathBuf;

    /// Names as an owned `Vec<String>`, the `list_dir` return shape.
    fn entries(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
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
        // Listing order is irrelevant: resolution sorts by bytes first. Driven
        // entirely through the seam — no real filesystem.
        let state = PathBuf::from("/state/cdata/katsuobushi");
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["inst-c", "inst-a", "inst-b"])));
        assert_eq!(resolve_instance(&state, &host, "2").unwrap(), "inst-b");
        // The index path goes through the host seam, not `std::fs`.
        assert_eq!(host.calls(), vec![Call::ListDir(state)]);
    }

    #[test]
    fn it_errors_on_an_out_of_range_index() {
        let state = PathBuf::from("/state/cdata/katsuobushi");
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["inst-a", "inst-b"])));
        let err = resolve_instance(&state, &host, "5").expect_err("index past the end must fail");
        assert!(format!("{err:#}").contains("index 5"));
    }

    #[test]
    fn it_rejects_a_zero_index_as_out_of_range() {
        // 1-based: index 0 is never valid.
        let state = PathBuf::from("/state/cdata/katsuobushi");
        let mut host = FakeHost::new();
        host.push_list_dir(Ok(entries(&["inst-a"])));
        assert!(resolve_instance(&state, &host, "0").is_err());
    }

    #[test]
    fn it_ignores_dot_prefixed_directories_when_indexing() {
        // The shell glob excludes dotdirs, so they must not occupy an index slot:
        // index 1 is the first non-dot entry in byte order.
        let state = PathBuf::from("/state/cdata/katsuobushi");
        let mut host = FakeHost::new();
        // One scripted listing per resolve call (the fake's queue pops once each).
        host.push_list_dir(Ok(entries(&[".hidden", "inst-a", ".git", "inst-b"])))
            .push_list_dir(Ok(entries(&[".hidden", "inst-a", ".git", "inst-b"])));
        assert_eq!(resolve_instance(&state, &host, "1").unwrap(), "inst-a");
        assert_eq!(resolve_instance(&state, &host, "2").unwrap(), "inst-b");
    }

    #[test]
    fn it_treats_a_missing_state_root_as_zero_instances() {
        // An unscripted `list_dir` queue yields `NotFound`, which resolves to an
        // empty listing — every index is then out of range.
        let state = PathBuf::from("/state/does/not/exist");
        let host = FakeHost::new();
        let err = resolve_instance(&state, &host, "1").expect_err("no instances -> out of range");
        assert!(format!("{err:#}").contains("index 1"));
    }

    #[test]
    fn it_rejects_an_empty_selector_with_a_usage_error() {
        // An empty arg must not resolve to the state root; it fails cleanly here.
        let state = PathBuf::from("/state/cdata/katsuobushi");
        let host = FakeHost::new();
        let err = resolve_instance(&state, &host, "").expect_err("empty selector must fail");
        assert!(format!("{err:#}").contains("required"));
        // Nothing was probed through the seam — the guard short-circuits.
        assert!(host.calls().is_empty());
    }
}
