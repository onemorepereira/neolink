//! Atomic TOML config file persistence (temp file + `rename`).
//!
//! Stub: filled in by Task 5.

use std::path::Path;

use anyhow::Result;

use crate::config::Config;

pub(crate) fn persist_config(_path: &Path, _config: &Config) -> Result<()> {
    unimplemented!("Task 5: atomic TOML persist")
}
