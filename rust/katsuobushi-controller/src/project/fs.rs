//! The `project` domain's world seams: filesystem IO and id entropy.
//!
//! Every verb touches the world only through [`Fs`] and [`Rng`], so each is
//! exercisable against an in-memory [`FakeFs`] / scripted [`SeqRng`] with no
//! tempdir and no real randomness — the same discipline `sandbox`'s [`Host`]
//! seam uses.

use std::io;
use std::path::Path;

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::path::PathBuf;

/// Filesystem operations the board verbs need. Production is [`RealFs`]; tests
/// drive [`FakeFs`].
pub trait Fs {
    fn exists(&self, p: &Path) -> bool;
    fn read(&self, p: &Path) -> io::Result<String>;
    /// Write a file whole. Production does it atomically (temp + rename) so a
    /// concurrent reader never sees a torn BOARD.md.
    fn write(&self, p: &Path, contents: &str) -> io::Result<()>;
    fn create_dir_all(&self, p: &Path) -> io::Result<()>;
    /// Immediate file names under `p` (not recursive; directories skipped).
    fn list_files(&self, p: &Path) -> io::Result<Vec<String>>;
}

/// A source of 32-bit entropy for id minting — the seam that makes `new`
/// deterministic under test.
pub trait Rng {
    fn next_u32(&mut self) -> u32;
}

/// Production filesystem.
pub struct RealFs;

impl Fs for RealFs {
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }
    fn read(&self, p: &Path) -> io::Result<String> {
        std::fs::read_to_string(p)
    }
    fn write(&self, p: &Path, contents: &str) -> io::Result<()> {
        // Atomic replace: write a sibling temp, then rename over the target.
        let tmp = match p.file_name() {
            Some(name) => {
                let mut t = name.to_os_string();
                t.push(".katsu-tmp");
                p.with_file_name(t)
            }
            None => p.to_path_buf(),
        };
        std::fs::write(&tmp, contents)?;
        std::fs::rename(&tmp, p)
    }
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        std::fs::create_dir_all(p)
    }
    fn list_files(&self, p: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(p)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
        Ok(names)
    }
}

/// Production entropy: `/dev/urandom`, falling back to a time+pid hash (matching
/// `sandbox/host.rs`'s `OsRng` rationale — no `rand` is vendored).
pub struct OsRng;

impl Rng for OsRng {
    fn next_u32(&mut self) -> u32 {
        use std::io::Read;
        let mut buf = [0u8; 4];
        if std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .is_ok()
        {
            return u32::from_le_bytes(buf);
        }
        // Fallback: hash the clock and pid.
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mixed = nanos ^ ((std::process::id() as u64) << 17).wrapping_mul(0x9E37_79B9);
        (mixed ^ (mixed >> 32)) as u32
    }
}

/// In-memory filesystem for tests.
#[cfg(test)]
#[derive(Default)]
pub struct FakeFs {
    files: RefCell<BTreeMap<PathBuf, String>>,
    dirs: RefCell<BTreeSet<PathBuf>>,
}

#[cfg(test)]
impl FakeFs {
    pub fn new() -> Self {
        Self::default()
    }
    /// Seed a file (also registering its parent directories).
    pub fn with_file(self, p: impl Into<PathBuf>, contents: &str) -> Self {
        let p = p.into();
        if let Some(parent) = p.parent() {
            let mut dirs = self.dirs.borrow_mut();
            for anc in parent.ancestors() {
                dirs.insert(anc.to_path_buf());
            }
        }
        self.files.borrow_mut().insert(p, contents.to_string());
        self
    }
    /// Read back a file's current contents (test assertions).
    pub fn get(&self, p: impl AsRef<Path>) -> Option<String> {
        self.files.borrow().get(p.as_ref()).cloned()
    }
}

#[cfg(test)]
impl Fs for FakeFs {
    fn exists(&self, p: &Path) -> bool {
        self.files.borrow().contains_key(p) || self.dirs.borrow().contains(p)
    }
    fn read(&self, p: &Path) -> io::Result<String> {
        self.files.borrow().get(p).cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no such file: {}", p.display()),
            )
        })
    }
    fn write(&self, p: &Path, contents: &str) -> io::Result<()> {
        if let Some(parent) = p.parent() {
            let mut dirs = self.dirs.borrow_mut();
            for anc in parent.ancestors() {
                dirs.insert(anc.to_path_buf());
            }
        }
        self.files
            .borrow_mut()
            .insert(p.to_path_buf(), contents.to_string());
        Ok(())
    }
    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        let mut dirs = self.dirs.borrow_mut();
        for anc in p.ancestors() {
            dirs.insert(anc.to_path_buf());
        }
        Ok(())
    }
    fn list_files(&self, p: &Path) -> io::Result<Vec<String>> {
        let files = self.files.borrow();
        let mut names = Vec::new();
        for path in files.keys() {
            if path.parent() == Some(p) {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    names.push(name.to_string());
                }
            }
        }
        Ok(names)
    }
}

/// Scripted entropy for tests: yields the given values in order, then repeats
/// the last one.
#[cfg(test)]
pub struct SeqRng {
    values: Vec<u32>,
    idx: usize,
}

#[cfg(test)]
impl SeqRng {
    pub fn new(values: Vec<u32>) -> Self {
        Self { values, idx: 0 }
    }
}

#[cfg(test)]
impl Rng for SeqRng {
    fn next_u32(&mut self) -> u32 {
        let v = self
            .values
            .get(self.idx)
            .copied()
            .unwrap_or(*self.values.last().unwrap_or(&0));
        self.idx += 1;
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_fs_round_trips_and_lists() {
        let fs = FakeFs::new().with_file("/board/BOARD.md", "hi");
        assert!(fs.exists(Path::new("/board/BOARD.md")));
        assert!(fs.exists(Path::new("/board"))); // ancestor dir registered
        assert_eq!(fs.read(Path::new("/board/BOARD.md")).unwrap(), "hi");

        fs.write(Path::new("/board/a3f7b2.md"), "note").unwrap();
        let mut names = fs.list_files(Path::new("/board")).unwrap();
        names.sort();
        assert_eq!(names, vec!["BOARD.md", "a3f7b2.md"]);
    }

    #[test]
    fn seq_rng_is_deterministic() {
        let mut r = SeqRng::new(vec![0xa3f7b2c1, 0x1a2b3c4d]);
        assert_eq!(r.next_u32(), 0xa3f7b2c1);
        assert_eq!(r.next_u32(), 0x1a2b3c4d);
        assert_eq!(r.next_u32(), 0x1a2b3c4d); // repeats last
    }
}
