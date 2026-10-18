use crate::{Index, Result};
use std::path::{Path, PathBuf};

/// `~/Library/Caches/ferret/index.bin`
pub fn default_path() -> PathBuf {
    todo!("W1 scanner agent")
}

/// Write atomically (temp file + rename).
pub fn save(index: &Index, path: &Path) -> Result<()> {
    todo!("W1 scanner agent")
}

pub fn load(path: &Path) -> Result<Index> {
    todo!("W1 scanner agent")
}
