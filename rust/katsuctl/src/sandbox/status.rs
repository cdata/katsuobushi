//! `katsuctl sandbox status` — show one or all instances (design §13). Stub.

use crate::Global;
use anyhow::{bail, Result};
use std::path::Path;

pub fn run(_config: &Path, _instance: Option<String>, _global: Global) -> Result<()> {
    bail!("katsuctl sandbox status: not yet implemented")
}
