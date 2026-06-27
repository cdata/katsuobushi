//! `katsuctl sandbox prompt` — stream a prompt to a running instance (design
//! §11). Absorbs the old `katsuobushi-sandbox-prompt` binary. Stub.

use crate::Global;
use anyhow::{bail, Result};
use std::path::Path;

pub fn run(_config: &Path, _instance: &str, _text: Vec<String>, _global: Global) -> Result<()> {
    bail!("katsuctl sandbox prompt: not yet implemented")
}
