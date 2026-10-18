use crate::index::NO_PARENT;
use crate::{Index, Kind, Result};
use jwalk::WalkDir;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};
use unicode_normalization::{is_nfc, UnicodeNormalization};

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
    let start = Instant::now();
    std::fs::symlink_metadata(root)?; // fail fast on a missing root
    let mut index = Index::default();
    let mut stats = ScanStats::default();
    // Only dirs live here, so it stays small (~hundreds of K entries for `/`).
    let mut dirs: HashMap<PathBuf, u32> = HashMap::new();

    let walk = WalkDir::new(root)
        .skip_hidden(false)
        .follow_links(false)
        .sort(false)
        .process_read_dir(|_, _, _, children| {
            children.retain(|c| match c {
                Ok(e) => !SKIP.iter().any(|s| e.path() == Path::new(s)),
                Err(_) => true,
            });
        });

    for entry in walk {
        let e = match entry {
            Ok(e) => e,
            Err(err) => {
                if err.io_error().map(|e| e.kind()) == Some(io::ErrorKind::PermissionDenied) {
                    stats.permission_denied += 1;
                }
                continue;
            }
        };
        let ft = e.file_type();
        let kind = if ft.is_symlink() {
            stats.symlinks += 1;
            Kind::Symlink
        } else if ft.is_dir() {
            stats.dirs += 1;
            Kind::Dir
        } else {
            stats.files += 1;
            Kind::File
        };
        let (size, mtime) = match e.metadata() {
            Ok(m) => (
                if kind == Kind::Dir { 0 } else { m.len() },
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64),
            ),
            Err(_) => (0, 0),
        };
        let (parent, name) = if e.depth == 0 {
            (NO_PARENT, root.to_string_lossy().into_owned())
        } else {
            // jwalk yields depth-first, so the parent dir was already seen.
            let Some(&p) = dirs.get(e.parent_path.as_ref()) else { continue };
            let raw = e.file_name.to_string_lossy();
            let name = if is_nfc(&raw) { raw.into_owned() } else { raw.nfc().collect() };
            (p, name)
        };
        let id = index.push(parent, &name, kind, size, mtime);
        if kind == Kind::Dir {
            dirs.insert(e.path(), id);
        }
    }
    stats.elapsed = start.elapsed();
    Ok((index, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn scans_tree() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        fs::create_dir_all(r.join("a/b")).unwrap();
        fs::write(r.join("a/b/x.pdf"), b"hello").unwrap();
        fs::write(r.join(".hidden"), b"").unwrap();
        fs::write(r.join("e\u{301}.txt"), b"").unwrap();
        std::os::unix::fs::symlink("a", r.join("link")).unwrap();

        let (ix, st) = scan(r).unwrap();
        assert_eq!((st.dirs, st.files, st.symlinks), (3, 3, 1));
        assert_eq!(ix.len(), 7);
        assert_eq!(ix.path(0), r.to_string_lossy());
        let find = |n: &str| (0..ix.len() as u32).find(|&i| ix.name(i) == n).unwrap();
        let x = find("x.pdf");
        assert_eq!(ix.path(x), format!("{}/a/b/x.pdf", r.display()));
        assert_eq!(ix.nodes[x as usize].size, 5);
        assert!(ix.nodes[x as usize].mtime > 0);
        assert_eq!(ix.nodes[find("a") as usize].size, 0);
        assert_eq!(ix.nodes[find("link") as usize].kind, Kind::Symlink);
        let h = find(".hidden");
        assert_eq!(ix.nodes[h as usize].flags & crate::index::FLAG_HIDDEN, crate::index::FLAG_HIDDEN);
        find("\u{e9}.txt"); // NFC
    }
}
