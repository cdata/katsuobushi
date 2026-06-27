//! `katsuctl sandbox stop` — QMP-quit an instance, optionally removing its state
//! (design §12). Stub.

use crate::Global;
use anyhow::{bail, Result};
use std::path::Path;

pub fn run(_config: &Path, _remove: bool, _instance: &str, _global: Global) -> Result<()> {
    bail!("katsuctl sandbox stop: not yet implemented")
}
