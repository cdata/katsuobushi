//! Library half of `katsuobushi-sandbox-guest`, consumed by the guest server
//! (`katsuobushi-sandbox-guest`) binary in this crate. The host client was
//! retired into `katsuctl sandbox prompt`.
//!
//! The shared wire types now live in the standalone [`katsuobushi_sandbox_protocol`]
//! crate so the future host (`katsuctl`) and the guest server can both depend
//! on them without the host linking the guest server's deps. See
//! `design/sandbox-agent-mode.md` and `design/katsuctl.md` §3.
