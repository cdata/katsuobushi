//! Library half of `katsuobushi-sandbox-control`, consumed by both the guest
//! server (`katsuobushi-sandbox-control`) and the host client
//! (`katsuobushi-sandbox-prompt`) binaries in this crate.
//!
//! The shared wire types now live in the standalone [`katsuobushi_protocol`]
//! crate so the future host (`katsuctl`) and the guest server can both depend
//! on them without the host linking the guest server's deps. See
//! `design/sandbox-agent-mode.md` and `design/katsuctl.md` §3.
