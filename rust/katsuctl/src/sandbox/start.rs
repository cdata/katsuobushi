//! `katsuctl sandbox start` — boot a new instance (design §8). Stub.

use crate::Global;
use anyhow::{bail, Result};
use std::path::Path;

pub fn run(
    _config: &Path,
    _agent: bool,
    _name: Option<String>,
    _prompt: Option<String>,
    _global: Global,
) -> Result<()> {
    bail!("katsuctl sandbox start: not yet implemented")
}
