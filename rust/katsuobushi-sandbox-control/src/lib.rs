//! Library half of `katsuobushi-sandbox-control`: the shared sandbox controller wire types,
//! consumed by both the guest server (`katsuobushi-sandbox-control`) and the
//! host client (`katsuobushi-sandbox-prompt`) binaries in this crate, so the
//! two sides cannot drift. See `design/sandbox-agent-mode.md`.

pub mod protocol;
