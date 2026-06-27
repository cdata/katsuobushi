//! `katsuctl sandbox fetch` — pull the work-product branch (design §12). Stub.

use crate::Global;
use anyhow::{bail, Result};
use std::path::Path;

pub fn run(_config: &Path, _instance: &str, _global: Global) -> Result<()> {
    bail!("katsuctl sandbox fetch: not yet implemented")
}
