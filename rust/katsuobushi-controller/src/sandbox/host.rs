//! The host IO seam (design/katsuctl.md §7.1, §2.3).
//!
//! Everything `katsuctl` does that touches the world — spawning the spec's
//! pinned tools, the filesystem, TCP port probing, QMP liveness, and the vsock
//! control connection — goes through [`Host`]. Production ([`HostImpl`]) shells
//! out to the exact store-path binaries Nix supplies and uses real sockets;
//! tests drive a [`FakeHost`] that records every call and returns canned
//! results, so every subcommand is exercisable without booting a VM (the three
//! test tiers, §7.2).
//!
//! Two probe-dependent decisions become *pure* once the world is behind the
//! seam: [`pick_cid`] (vsock CID allocation, lib/sandbox/default.nix:1346-1370)
//! and [`pick_port`] (free loopback port, :1483-1493). Both take an injected
//! predicate / RNG, so their loop-and-retry logic is unit-testable with a
//! deterministic fake.
//!
//! ## Deviations from the literal design
//!
//! - **RNG.** The design sketch types the helpers against `rand::RngCore`, but
//!   `rand` is not vendored and the sandbox has no crates.io access. This module
//!   defines a tiny local [`Rng`] trait instead: production [`OsRng`] is a
//!   `std`-only generator seeded from `/dev/urandom` (falling back to a
//!   `SystemTime`+pid hash), and tests use a scripted [`FakeRng`].
//! - **`vsock_connect`.** The rest of the seam is synchronous, but a
//!   [`tokio_vsock::VsockStream`] is an async resource bound to a runtime. To
//!   keep the trait sync (and object-safe) [`HostImpl`] owns a current-thread
//!   tokio runtime and `block_on`s the connect; the returned stream stays valid
//!   for the lifetime of the `HostImpl` (and its runtime). The future `prompt`
//!   path drives bytes over the stream; that wire round-trip is covered by the
//!   one true end-to-end gate (`checks.sandbox`, §7.2), not these unit tests.
//!
//! Lands ahead of its callers (the subcommands migrate command-by-command,
//! design §12), so most of the surface here is `dead_code` until then.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Result};
use tokio_vsock::{VsockAddr, VsockStream};

/// Everything `katsuctl` does that touches the world goes through this trait
/// (design §7.1). Production shells out to `spec.tools.*` and the real
/// filesystem/sockets; tests use a fake that records calls and returns canned
/// results.
pub trait Host {
    /// Spawn a (pinned) tool to completion and capture its output. Callers build
    /// the [`Command`] around a store-path program from `spec.tools.*` (§2.3).
    fn run(&self, cmd: &Command) -> io::Result<Output>;
    /// Read a file whole.
    fn read(&self, p: &Path) -> io::Result<Vec<u8>>;
    /// Write a file whole (create or truncate).
    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Rename `from` to `to` (atomically within a filesystem). Paired with
    /// [`write`](Host::write) to a temp sibling, this gives an atomic file
    /// update — a reader never observes a torn write (design §9).
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Whether a path exists.
    fn exists(&self, p: &Path) -> bool;
    /// List the immediate subdirectory names of `p` (each a directory entry's
    /// file name), in arbitrary order. Non-directory entries are skipped. The
    /// instance-index seam: routes `resolve.rs`'s enumeration through the host so
    /// it is `FakeHost`-testable (design §7.1).
    fn list_dir(&self, p: &Path) -> io::Result<Vec<String>>;
    /// Whether a loopback TCP port is free — the [`pick_port`] predicate (:1488).
    fn port_is_free(&self, port: u16) -> bool;
    /// Whether a qemu QMP monitor is listening at `sock` (native QMP, #006).
    fn qmp_alive(&self, sock: &Path) -> bool;
    /// Connect to the guest control server over vsock (design §2.3).
    fn vsock_connect(&self, cid: u32, port: u32) -> io::Result<VsockStream>;
}

/// A pluggable source of `u32` randomness — the local stand-in for
/// `rand::RngCore` (see the module deviation note). Production reads OS entropy;
/// tests inject a scripted sequence so the allocators are deterministic.
pub trait Rng {
    /// The next 32-bit value.
    fn next_u32(&mut self) -> u32;
}

/// Production [`Host`]: real processes, files, sockets, and an owned tokio
/// runtime for the async vsock connect.
pub struct HostImpl {
    /// Drives [`Host::vsock_connect`]; kept alive so returned streams stay valid.
    runtime: tokio::runtime::Runtime,
}

impl HostImpl {
    /// Build the production host, standing up the single-threaded runtime that
    /// backs vsock connects. Fails only if the runtime cannot be created.
    pub fn new() -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(Self { runtime })
    }
}

impl Host for HostImpl {
    fn run(&self, cmd: &Command) -> io::Result<Output> {
        // `Command::output` needs `&mut self`, but the seam hands us a shared
        // `&Command`, so rebuild an equivalent invocation from its parts
        // (program, args, cwd, and explicitly-set env overrides; the inherited
        // parent environment passes through untouched).
        let mut spawn = Command::new(cmd.get_program());
        spawn.args(cmd.get_args());
        if let Some(dir) = cmd.get_current_dir() {
            spawn.current_dir(dir);
        }
        for (key, value) in cmd.get_envs() {
            match value {
                Some(value) => {
                    spawn.env(key, value);
                }
                None => {
                    spawn.env_remove(key);
                }
            }
        }
        spawn.output()
    }

    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(p)
    }

    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(p, bytes)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }

    fn list_dir(&self, p: &Path) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(p)? {
            let entry = entry?;
            // Only directories are instances; a non-directory entry's type read
            // failing is treated as "not a directory" and skipped.
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
        Ok(names)
    }

    fn port_is_free(&self, port: u16) -> bool {
        // Free iff we can claim it: a successful bind on loopback. Mirrors the
        // shell probe (`/dev/tcp` connect inverted) at :1488 — binding is the
        // host-side analogue and avoids a phantom connect to a foreign listener.
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
    }

    fn qmp_alive(&self, sock: &Path) -> bool {
        // The native QMP module (#006) is the one true liveness probe (§2.3).
        crate::sandbox::qmp::is_alive(sock)
    }

    fn vsock_connect(&self, cid: u32, port: u32) -> io::Result<VsockStream> {
        self.runtime
            .block_on(VsockStream::connect(VsockAddr::new(cid, port)))
    }
}

/// A `std`-only [`Rng`]: an `xorshift64*` generator seeded once from OS entropy.
/// Good enough for picking a CID/port (not cryptographic) and avoids a
/// per-value syscall.
pub struct OsRng {
    state: u64,
}

impl OsRng {
    /// Seed from `/dev/urandom`, falling back to a `SystemTime`+pid hash if that
    /// device is unavailable. The state is forced nonzero (xorshift fixed point).
    pub fn new() -> Self {
        Self {
            state: os_seed() | 1,
        }
    }
}

impl Default for OsRng {
    fn default() -> Self {
        Self::new()
    }
}

impl Rng for OsRng {
    fn next_u32(&mut self) -> u32 {
        // xorshift64*: cheap, decent distribution, fully `std`.
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }
}

/// Read 8 bytes of OS entropy, or hash `SystemTime`+pid if `/dev/urandom`
/// cannot be read (e.g. a stripped container).
fn os_seed() -> u64 {
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut buf = [0u8; 8];
        if f.read_exact(&mut buf).is_ok() {
            return u64::from_le_bytes(buf);
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // Mix in the pid so two processes seeded in the same nanosecond diverge.
    nanos
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(u64::from(std::process::id()))
}

/// Lowest vsock CID an instance may claim — 0..=2 are reserved (hypervisor,
/// local, host), so the modulus leaves room to add 3 (:1359, design §7.1).
const CID_SPAN: u32 = 2_147_483_640;
/// First reserved-free CID.
const CID_BASE: u32 = 3;
/// Bound on allocation attempts before giving up (:1357).
const ALLOC_TRIES: usize = 100;

/// Lowest loopback port [`pick_port`] hands out (:1488).
const PORT_BASE: u16 = 20_000;
/// Size of the loopback port window, `[PORT_BASE, PORT_BASE + PORT_SPAN)` (:1488).
const PORT_SPAN: u32 = 20_000;

/// Allocate a vsock CID not already claimed by a sibling instance — a pure loop
/// over an injected RNG (design §7.1; default.nix:1346-1370). Tries up to
/// [`ALLOC_TRIES`] times, skipping CIDs in `used`, and bails if every draw
/// collides (a host that has somehow exhausted the space).
pub fn pick_cid(used: &HashSet<u32>, rng: &mut impl Rng) -> Result<u32> {
    for _ in 0..ALLOC_TRIES {
        let candidate = rng.next_u32() % CID_SPAN + CID_BASE;
        if !used.contains(&candidate) {
            return Ok(candidate);
        }
    }
    bail!("could not allocate a vsock CID (100 draws all collided)")
}

/// Pick a free loopback port — a pure loop over an injected `is_free` predicate
/// and RNG (design §7.1; default.nix:1483-1493). Draws from `[20000, 40000)`,
/// returning the first the predicate accepts; bails after [`ALLOC_TRIES`]
/// rejections.
pub fn pick_port(is_free: impl Fn(u16) -> bool, rng: &mut impl Rng) -> Result<u16> {
    for _ in 0..ALLOC_TRIES {
        let candidate = (rng.next_u32() % PORT_SPAN + u32::from(PORT_BASE)) as u16;
        if is_free(candidate) {
            return Ok(candidate);
        }
    }
    bail!("could not find a free loopback port (100 draws all in use)")
}

/// One recorded [`Host`] interaction (test introspection for [`FakeHost`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `run`, captured as the program followed by its args (lossy on non-UTF-8).
    Run(Vec<String>),
    /// `read(path)`.
    Read(PathBuf),
    /// `write(path, bytes)`.
    Write(PathBuf, Vec<u8>),
    /// `rename(from, to)`.
    Rename(PathBuf, PathBuf),
    /// `exists(path)`.
    Exists(PathBuf),
    /// `list_dir(path)`.
    ListDir(PathBuf),
    /// `port_is_free(port)`.
    PortIsFree(u16),
    /// `qmp_alive(sock)`.
    QmpAlive(PathBuf),
    /// `vsock_connect(cid, port)`.
    VsockConnect(u32, u32),
}

/// Test [`Host`]: records every call in order and returns scripted/canned
/// results set up per test. Uses `RefCell` interior mutability because the
/// trait methods take `&self`.
///
/// Result conventions: `run`/`read`/`write`/`list_dir` pop from a per-method
/// queue (so a test can script success then failure), defaulting to a benign
/// result when the queue is empty (`list_dir` defaults to `NotFound`, mirroring
/// a missing state root). `exists`/`port_is_free`/`qmp_alive` answer from a set of
/// "true" inputs (anything not in the set is `false`). `vsock_connect` cannot
/// fabricate a live [`VsockStream`], so it records the call and returns an
/// error — seam tests assert the connect was *attempted*; the byte round-trip
/// is the e2e gate's job (§7.2).
#[derive(Default)]
pub struct FakeHost {
    calls: RefCell<Vec<Call>>,
    run_results: RefCell<VecDeque<io::Result<Output>>>,
    read_results: RefCell<VecDeque<io::Result<Vec<u8>>>>,
    write_results: RefCell<VecDeque<io::Result<()>>>,
    rename_results: RefCell<VecDeque<io::Result<()>>>,
    list_dir_results: RefCell<VecDeque<io::Result<Vec<String>>>>,
    existing: HashSet<PathBuf>,
    free_ports: HashSet<u16>,
    alive_socks: HashSet<PathBuf>,
}

impl FakeHost {
    /// An empty fake: no scripted results, nothing exists, no port free.
    pub fn new() -> Self {
        Self::default()
    }

    /// Script the next `run` result.
    pub fn push_run(&mut self, result: io::Result<Output>) -> &mut Self {
        self.run_results.get_mut().push_back(result);
        self
    }

    /// Script the next `read` result.
    pub fn push_read(&mut self, result: io::Result<Vec<u8>>) -> &mut Self {
        self.read_results.get_mut().push_back(result);
        self
    }

    /// Script the next `write` result.
    pub fn push_write(&mut self, result: io::Result<()>) -> &mut Self {
        self.write_results.get_mut().push_back(result);
        self
    }

    /// Script the next `rename` result.
    pub fn push_rename(&mut self, result: io::Result<()>) -> &mut Self {
        self.rename_results.get_mut().push_back(result);
        self
    }

    /// Script the next `list_dir` result.
    pub fn push_list_dir(&mut self, result: io::Result<Vec<String>>) -> &mut Self {
        self.list_dir_results.get_mut().push_back(result);
        self
    }

    /// Make `exists(path)` answer `true`.
    pub fn with_existing(&mut self, path: impl Into<PathBuf>) -> &mut Self {
        self.existing.insert(path.into());
        self
    }

    /// Make `port_is_free(port)` answer `true`.
    pub fn with_free_port(&mut self, port: u16) -> &mut Self {
        self.free_ports.insert(port);
        self
    }

    /// Make `qmp_alive(sock)` answer `true`.
    pub fn with_alive_sock(&mut self, sock: impl Into<PathBuf>) -> &mut Self {
        self.alive_socks.insert(sock.into());
        self
    }

    /// The recorded calls so far, in order.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }
}

/// A benign empty success, returned when a `run` queue runs dry.
fn ok_output() -> Output {
    use std::os::unix::process::ExitStatusExt;
    Output {
        status: std::process::ExitStatus::from_raw(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

impl Host for FakeHost {
    fn run(&self, cmd: &Command) -> io::Result<Output> {
        let mut parts = vec![cmd.get_program().to_string_lossy().into_owned()];
        parts.extend(cmd.get_args().map(|a| a.to_string_lossy().into_owned()));
        self.calls.borrow_mut().push(Call::Run(parts));
        self.run_results
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Ok(ok_output()))
    }

    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        self.calls.borrow_mut().push(Call::Read(p.to_path_buf()));
        self.read_results
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err(io::Error::from(io::ErrorKind::NotFound)))
    }

    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
        self.calls
            .borrow_mut()
            .push(Call::Write(p.to_path_buf(), bytes.to_vec()));
        self.write_results
            .borrow_mut()
            .pop_front()
            .unwrap_or(Ok(()))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.calls
            .borrow_mut()
            .push(Call::Rename(from.to_path_buf(), to.to_path_buf()));
        self.rename_results
            .borrow_mut()
            .pop_front()
            .unwrap_or(Ok(()))
    }

    fn exists(&self, p: &Path) -> bool {
        self.calls.borrow_mut().push(Call::Exists(p.to_path_buf()));
        self.existing.contains(p)
    }

    fn list_dir(&self, p: &Path) -> io::Result<Vec<String>> {
        self.calls.borrow_mut().push(Call::ListDir(p.to_path_buf()));
        self.list_dir_results
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err(io::Error::from(io::ErrorKind::NotFound)))
    }

    fn port_is_free(&self, port: u16) -> bool {
        self.calls.borrow_mut().push(Call::PortIsFree(port));
        self.free_ports.contains(&port)
    }

    fn qmp_alive(&self, sock: &Path) -> bool {
        self.calls
            .borrow_mut()
            .push(Call::QmpAlive(sock.to_path_buf()));
        self.alive_socks.contains(sock)
    }

    fn vsock_connect(&self, cid: u32, port: u32) -> io::Result<VsockStream> {
        self.calls.borrow_mut().push(Call::VsockConnect(cid, port));
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "FakeHost cannot open a real vsock stream (call recorded)",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted [`Rng`] that yields a fixed sequence, then repeats the last
    /// value forever (so collision-exhaustion tests stay simple).
    struct FakeRng {
        values: Vec<u32>,
        next: usize,
    }

    impl FakeRng {
        fn new(values: &[u32]) -> Self {
            Self {
                values: values.to_vec(),
                next: 0,
            }
        }
    }

    impl Rng for FakeRng {
        fn next_u32(&mut self) -> u32 {
            let value = self.values[self.next.min(self.values.len() - 1)];
            self.next += 1;
            value
        }
    }

    #[test]
    fn it_picks_the_first_cid_when_unused() {
        // 10 % SPAN + 3 == 13.
        let mut rng = FakeRng::new(&[10]);
        let used = HashSet::new();
        assert_eq!(pick_cid(&used, &mut rng).unwrap(), 13);
    }

    #[test]
    fn it_skips_used_cids() {
        // First draw -> 13 (used); second -> 23 (free).
        let mut rng = FakeRng::new(&[10, 20]);
        let used = HashSet::from([13]);
        assert_eq!(pick_cid(&used, &mut rng).unwrap(), 23);
    }

    #[test]
    fn it_errors_after_100_cid_collisions() {
        // Every draw maps to 13, which is permanently in use.
        let mut rng = FakeRng::new(&[10]);
        let used = HashSet::from([13]);
        let err = pick_cid(&used, &mut rng).expect_err("100 collisions must bail");
        assert!(format!("{err:#}").contains("allocate a vsock CID"));
    }

    #[test]
    fn it_honors_the_port_predicate() {
        // 5 -> 20005 (not free), 6 -> 20006 (free).
        let mut rng = FakeRng::new(&[5, 6]);
        let port = pick_port(|p| p == 20_006, &mut rng).unwrap();
        assert_eq!(port, 20_006);
    }

    #[test]
    fn it_errors_when_no_port_is_free() {
        let mut rng = FakeRng::new(&[5]);
        let err = pick_port(|_| false, &mut rng).expect_err("no free port must bail");
        assert!(format!("{err:#}").contains("free loopback port"));
    }

    #[test]
    fn it_records_a_representative_call_sequence() {
        let sock = PathBuf::from("/run/katsu/qmp.sock");
        let state = PathBuf::from("/state/instance");
        let mut host = FakeHost::new();
        host.with_existing(state.clone())
            .with_free_port(20_000)
            .with_alive_sock(sock.clone())
            .push_read(Ok(b"agent-123".to_vec()))
            .push_list_dir(Ok(vec!["inst-a".to_string(), "inst-b".to_string()]));

        host.write(&state, b"agent-123").unwrap();
        assert_eq!(host.read(&state).unwrap(), b"agent-123");
        let mut cmd = Command::new("/nix/store/h1-git/bin/git");
        cmd.arg("clone").arg("--bare");
        host.run(&cmd).unwrap();
        assert!(host.exists(&state));
        assert_eq!(
            host.list_dir(Path::new("/state")).unwrap(),
            vec!["inst-a".to_string(), "inst-b".to_string()]
        );
        assert!(host.port_is_free(20_000));
        assert!(!host.port_is_free(20_001));
        assert!(host.qmp_alive(&sock));
        assert!(host.vsock_connect(42, 1024).is_err());

        assert_eq!(
            host.calls(),
            vec![
                Call::Write(state.clone(), b"agent-123".to_vec()),
                Call::Read(state.clone()),
                Call::Run(vec![
                    "/nix/store/h1-git/bin/git".to_string(),
                    "clone".to_string(),
                    "--bare".to_string(),
                ]),
                Call::Exists(state.clone()),
                Call::ListDir(PathBuf::from("/state")),
                Call::PortIsFree(20_000),
                Call::PortIsFree(20_001),
                Call::QmpAlive(sock.clone()),
                Call::VsockConnect(42, 1024),
            ]
        );
    }

    #[test]
    fn it_defaults_unset_fake_results_benignly() {
        let host = FakeHost::new();
        // Empty run queue -> success; empty read queue -> NotFound.
        assert!(host
            .run(&Command::new("/bin/true"))
            .unwrap()
            .status
            .success());
        assert_eq!(
            host.read(Path::new("/nope")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(!host.exists(Path::new("/nope")));
    }

    #[test]
    fn it_produces_distinct_os_rng_values() {
        // Smoke test: a real OsRng advances (not stuck on its seed).
        let mut rng = OsRng::new();
        let a = rng.next_u32();
        let b = rng.next_u32();
        assert_ne!(a, b, "xorshift must advance");
    }
}
