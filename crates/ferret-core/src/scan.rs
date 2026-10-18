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
    scan_with_progress(root, |_, _| {})
}

/// `scan`, calling `progress(entries_so_far, current_path)` every 8192 entries.
pub fn scan_with_progress(root: &Path, mut progress: impl FnMut(u64, &Path)) -> Result<(Index, ScanStats)> {
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
        if id % 8192 == 0 {
            progress(id as u64, &e.parent_path);
        }
    }
    stats.elapsed = start.elapsed();
    Ok((index, stats))
}

fn meta_of(m: &std::fs::Metadata) -> (Kind, u64, i64) {
    let ft = m.file_type();
    let kind = if ft.is_symlink() {
        Kind::Symlink
    } else if ft.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    };
    let mtime = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as i64);
    (kind, if kind == Kind::Dir { 0 } else { m.len() }, mtime)
}

fn nfc(raw: &std::ffi::OsStr) -> String {
    let raw = raw.to_string_lossy();
    if is_nfc(&raw) {
        raw.into_owned()
    } else {
        raw.nfc().collect()
    }
}

/// Push `path` under `parent`, and its whole subtree if it is a directory.
fn add_tree(ix: &mut Index, parent: u32, path: &Path, name: &str, m: &std::fs::Metadata) {
    let (kind, size, mtime) = meta_of(m);
    let id = ix.push(parent, name, kind, size, mtime);
    if kind != Kind::Dir {
        return;
    }
    let Ok(rd) = std::fs::read_dir(path) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if SKIP.iter().any(|s| p == Path::new(s)) {
            continue;
        }
        if let Ok(m) = e.metadata() {
            add_tree(ix, id, &p, &nfc(&e.file_name()), &m);
        }
    }
}

/// Diff one directory's direct children against disk: new -> push (with subtree),
/// missing -> tombstone (recursively), changed kind/size/mtime -> update.
/// If `dir` is not indexed or no longer on disk, the nearest indexed ancestor is rescanned instead.
/// Returns the paths of the live child directories of the directory actually rescanned.
pub fn rescan_dir(ix: &mut Index, dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dir = dir.to_path_buf();
    let id = loop {
        match ix.find(&dir) {
            Some(id) if ix.nodes[id as usize].kind == Kind::Dir => break id,
            _ => {}
        }
        if !dir.pop() {
            return Ok(Vec::new()); // outside the indexed root
        }
    };
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if id != 0 && matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory) => {
            return rescan_dir(ix, dir.parent().unwrap_or(&dir));
        }
        Err(e) => return Err(e.into()),
    };
    if let Ok(m) = std::fs::symlink_metadata(&dir) {
        ix.nodes[id as usize].mtime = meta_of(&m).2;
    }
    let mut old: HashMap<String, u32> = ix.children(id).map(|c| (ix.name(c).to_owned(), c)).collect();
    let mut subdirs = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if SKIP.iter().any(|s| p == Path::new(s)) {
            continue;
        }
        let Ok(m) = e.metadata() else { continue }; // vanished meanwhile
        let name = nfc(&e.file_name());
        let (kind, size, mtime) = meta_of(&m);
        match old.remove(&name) {
            Some(c) if ix.nodes[c as usize].kind == kind => {
                let n = &mut ix.nodes[c as usize];
                n.size = size;
                n.mtime = mtime;
            }
            prev => {
                if let Some(c) = prev {
                    ix.remove(c);
                }
                add_tree(ix, id, &p, &name, &m);
            }
        }
        if kind == Kind::Dir {
            subdirs.push(p);
        }
    }
    for c in old.into_values() {
        ix.remove(c);
    }
    Ok(subdirs)
}

/// Apply one changed path with a single lstat: add, update or remove just that entry
/// (a new directory brings its subtree). Falls back to `rescan_dir` on the parent when
/// the parent isn't indexed yet.
pub fn apply_path(ix: &mut Index, path: &Path) -> Result<()> {
    let (Some(parent), Some(raw)) = (path.parent(), path.file_name()) else {
        return rescan_dir(ix, path).map(|_| ());
    };
    if SKIP.iter().any(|s| path == Path::new(s)) {
        return Ok(());
    }
    let Some(pid) = ix.find(parent).filter(|&p| ix.nodes[p as usize].kind == Kind::Dir) else {
        return rescan_dir(ix, parent).map(|_| ());
    };
    let name = nfc(raw);
    let existing = ix.children(pid).find(|&c| ix.name(c) == name);
    match (std::fs::symlink_metadata(path), existing) {
        (Ok(m), Some(c)) if ix.nodes[c as usize].kind == meta_of(&m).0 => {
            let (_, size, mtime) = meta_of(&m);
            let n = &mut ix.nodes[c as usize];
            n.size = size;
            n.mtime = mtime;
        }
        (Ok(m), prev) => {
            if let Some(c) = prev {
                ix.remove(c);
            }
            add_tree(ix, pid, path, &name, &m);
        }
        (Err(e), Some(c)) if e.kind() == io::ErrorKind::NotFound => ix.remove(c),
        (Err(e), _) if e.kind() == io::ErrorKind::NotFound => {}
        (Err(e), _) => return Err(e.into()),
    }
    Ok(())
}

/// `rescan_dir` on `dir` and every directory below it.
pub fn rescan_subtree(ix: &mut Index, dir: &Path) -> Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        match rescan_dir(ix, &d) {
            Ok(sub) => stack.extend(sub),
            Err(e) if d == dir => return Err(e),
            Err(_) => {} // unreadable subdir: keep what we had
        }
    }
    Ok(())
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

    fn live_paths(ix: &Index) -> Vec<String> {
        let mut v: Vec<String> = (0..ix.len() as u32).filter(|&i| ix.is_live(i)).map(|i| ix.path(i)).collect();
        v.sort();
        v
    }

    #[test]
    fn rescan_dir_diff() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().canonicalize().unwrap();
        fs::create_dir_all(r.join("a/b/c")).unwrap();
        fs::write(r.join("a/b/c/deep.txt"), b"x").unwrap();
        fs::write(r.join("a/keep.txt"), b"1").unwrap();
        fs::write(r.join("a/gone.txt"), b"").unwrap();
        let (mut ix, _) = scan(&r).unwrap();
        let rs = r.to_string_lossy();

        // add (file + whole new subtree), remove, modify, nested remove
        fs::write(r.join("a/new.txt"), b"").unwrap();
        fs::create_dir_all(r.join("a/nd/x")).unwrap();
        fs::write(r.join("a/nd/x/in.txt"), b"").unwrap();
        fs::remove_file(r.join("a/gone.txt")).unwrap();
        fs::write(r.join("a/keep.txt"), b"12345").unwrap();
        fs::remove_dir_all(r.join("a/b")).unwrap();
        rescan_dir(&mut ix, &r.join("a")).unwrap();

        let want: Vec<String> = ["", "/a", "/a/keep.txt", "/a/nd", "/a/nd/x", "/a/nd/x/in.txt", "/a/new.txt"]
            .iter()
            .map(|s| format!("{rs}{s}"))
            .collect();
        assert_eq!(live_paths(&ix), want);
        assert_eq!(ix.live_count(), want.len());
        assert_eq!(ix.nodes[ix.find(&r.join("a/keep.txt")).unwrap() as usize].size, 5);
        assert!(ix.find(&r.join("a/b/c/deep.txt")).is_none());
        // tombstones are hidden from search
        let res = crate::search(&ix, &crate::parse("deep").unwrap(), 10, crate::Sort::Score);
        assert_eq!(res.total, 0);

        // event for a path that no longer exists falls back to the nearest ancestor
        fs::remove_dir_all(r.join("a/nd")).unwrap();
        rescan_dir(&mut ix, &r.join("a/nd/x")).unwrap();
        assert!(ix.find(&r.join("a/nd")).is_none());

        // subtree rescan catches changes below the first level
        fs::write(r.join("a/nd2.txt"), b"").unwrap();
        fs::create_dir_all(r.join("z/y")).unwrap();
        rescan_subtree(&mut ix, &r).unwrap();
        assert!(ix.find(&r.join("a/nd2.txt")).is_some() && ix.find(&r.join("z/y")).is_some());
    }

    #[test]
    fn compaction() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().canonicalize().unwrap();
        for i in 0..10 {
            fs::write(r.join(format!("f{i}")), b"").unwrap();
        }
        fs::create_dir(r.join("d")).unwrap();
        fs::write(r.join("d/kept"), b"").unwrap();
        let (mut ix, _) = scan(&r).unwrap();
        for i in 0..5 {
            fs::remove_file(r.join(format!("f{i}"))).unwrap();
        }
        rescan_dir(&mut ix, &r).unwrap();
        assert_eq!(ix.deleted, 5);
        let before = live_paths(&ix);
        assert!(ix.maybe_compact());
        assert_eq!(ix.deleted, 0);
        assert_eq!(ix.len(), before.len());
        assert_eq!(live_paths(&ix), before);
        assert_eq!(ix.path(0), r.to_string_lossy());
        assert!(ix.find(&r.join("d/kept")).is_some());
        assert!(!ix.maybe_compact());
        // links still work for further mutation
        fs::write(r.join("d/more"), b"").unwrap();
        rescan_dir(&mut ix, &r.join("d")).unwrap();
        assert!(ix.find(&r.join("d/more")).is_some());
    }

    #[test]
    fn apply_path_single_entries() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().canonicalize().unwrap();
        fs::write(r.join("a.txt"), b"1").unwrap();
        let (mut ix, _) = scan(&r).unwrap();
        // create, with a decomposed name as FSEvents may report it
        fs::write(r.join("e\u{301}.md"), b"").unwrap();
        apply_path(&mut ix, &r.join("e\u{301}.md")).unwrap();
        assert!(ix.find(&r.join("\u{e9}.md")).is_some());
        // modify
        fs::write(r.join("a.txt"), b"12345").unwrap();
        apply_path(&mut ix, &r.join("a.txt")).unwrap();
        assert_eq!(ix.nodes[ix.find(&r.join("a.txt")).unwrap() as usize].size, 5);
        // new dir brings its subtree
        fs::create_dir_all(r.join("n/m")).unwrap();
        fs::write(r.join("n/m/deep"), b"").unwrap();
        apply_path(&mut ix, &r.join("n")).unwrap();
        assert!(ix.find(&r.join("n/m/deep")).is_some());
        // delete dir removes subtree; deleting twice is fine
        fs::remove_dir_all(r.join("n")).unwrap();
        apply_path(&mut ix, &r.join("n")).unwrap();
        apply_path(&mut ix, &r.join("n")).unwrap();
        assert!(ix.find(&r.join("n/m/deep")).is_none());
        // parent not indexed yet -> falls back to rescanning the ancestor
        fs::create_dir_all(r.join("x/y")).unwrap();
        fs::write(r.join("x/y/z"), b"").unwrap();
        apply_path(&mut ix, &r.join("x/y/z")).unwrap();
        assert!(ix.find(&r.join("x/y/z")).is_some());
    }
}
