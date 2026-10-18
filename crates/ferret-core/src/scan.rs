use crate::{Index, Result};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub permission_denied: u64,
    pub elapsed: Duration,
}

/// Paths never scanned when walking from `/`.
pub const SKIP: &[&str] = &["/System/Volumes", "/dev", "/private/var/vm", "/Volumes"];

/// Walk `root` without following symlinks and build an index.
pub fn scan(root: &Path) -> Result<(Index, ScanStats)> {
    todo!("W1 scanner agent")
}
