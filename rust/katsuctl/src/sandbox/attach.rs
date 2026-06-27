//! `katsuctl sandbox attach` — emit the interactive tmux-attach handoff (design
//! §8.3). Stub.

use crate::Global;
use anyhow::{bail, Result};
use std::path::Path;

pub fn run(_config: &Path, _instance: &str, _global: Global) -> Result<()> {
    bail!("katsuctl sandbox attach: not yet implemented")
}
