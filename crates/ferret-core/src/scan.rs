use crate::index::NO_PARENT;
use crate::{Excludes, Index, Kind, Result};
use ignore::gitignore::Gitignore;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
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

/// .gitignore matchers in effect, outermost first; None outside a git repo.
type Gi = Option<Arc<Vec<Gitignore>>>;

/// Gitignore state after entering `dir`.
fn enter(gi: &Gi, dir: &Path, has_git: bool, has_gitignore: bool) -> Gi {
    let mut gi = if has_git {
        Some(Arc::new(Vec::new()))
    } else {
        gi.clone()
    };
    if has_gitignore {
        if let Some(v) = &mut gi {
            Arc::make_mut(v).push(Gitignore::new(dir.join(".gitignore")).0);
        }
    }
    gi
}

fn ignored(gi: &Gi, path: &Path, is_dir: bool) -> bool {
    let Some(v) = gi else { return false };
    for m in v.iter().rev() {
        match m.matched(path, is_dir) {
            ignore::Match::Ignore(_) => return true,
            ignore::Match::Whitelist(_) => return false,
            ignore::Match::None => {}
        }
    }
    false
}

/// Gitignore state inherited by `dir` from its ancestors, for rescans that start mid-tree.
fn gi_above(dir: &Path) -> Gi {
    let anc: Vec<&Path> = dir.ancestors().skip(1).collect();
    let top = anc.iter().position(|a| a.join(".git").exists())?;
    anc[..=top].iter().rev().fold(None, |gi, a| {
        enter(&gi, a, *a == anc[top], a.join(".gitignore").is_file())
    })
}

/// Drop `dir`'s excluded entries from its listing `v`; returns the gitignore state for its subdirs.
/// A virtualenv (has `pyvenv.cfg`) keeps its own node but loses every child.
fn retain<T>(
    ex: &Excludes,
    dir: &Path,
    gi: &Gi,
    v: &mut Vec<T>,
    f: impl Fn(&T) -> (&str, bool),
) -> Gi {
    if !ex.is_active() {
        return None;
    }
    let has = |n: &str| v.iter().any(|e| f(e).0 == n);
    if ex.defaults && has("pyvenv.cfg") {
        v.clear();
        return None;
    }
    let gi = if ex.defaults {
        enter(gi, dir, has(".git"), has(".gitignore"))
    } else {
        None
    };
    v.retain(|e| {
        let (n, d) = f(e);
        !ex.skip(dir, n, d) && !(gi.is_some() && ignored(&gi, &dir.join(n), d))
    });
    gi
}

/// Walk `root` without following symlinks and build an index.
pub fn scan(root: &Path) -> Result<(Index, ScanStats)> {
    scan_with_progress(root, &Excludes::none(), |_, _| {})
}

/// `scan` minus `ex`, calling `progress(entries_so_far, current_path)` every 8192 entries.
pub fn scan_with_progress(
    root: &Path,
    ex: &Excludes,
    mut progress: impl FnMut(u64, &Path),
) -> Result<(Index, ScanStats)> {
    let start = Instant::now();
    let root_meta = std::fs::symlink_metadata(root)?; // fail fast on a missing root
    let mut index = Index::default();
    let mut stats = ScanStats::default();
    let (kind, size, mtime) = meta_of(&root_meta);
    count(&mut stats, kind);
    index.push(NO_PARENT, &root.to_string_lossy(), kind, size, mtime);
    if kind != Kind::Dir {
        stats.elapsed = start.elapsed();
        return Ok((index, stats));
    }

    // Workers list directories in parallel; this thread is the only one touching `index`,
    // so every child dir gets its node id before its own listing job is queued.
    // Each job carries the gitignore state its directory inherits.
    type Job = (u32, PathBuf, Gi);
    type Done = (u32, PathBuf, Gi, io::Result<Vec<Entry>>);
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let job_rx = Mutex::new(job_rx);
    let (done_tx, done_rx) = mpsc::channel::<Done>();
    // Measured on 8 cores: past ~8 threads APFS lock contention only adds sys time.
    let threads = std::thread::available_parallelism()
        .map_or(8, |n| n.get())
        .min(8);

    std::thread::scope(|s| {
        for _ in 0..threads {
            let done_tx = done_tx.clone();
            let job_rx = &job_rx;
            s.spawn(move || {
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    let job = job_rx.lock().unwrap().recv();
                    let Ok((id, path, gi)) = job else { break };
                    let res = list_dir(&path, &mut buf);
                    if done_tx.send((id, path, gi, res)).is_err() {
                        break;
                    }
                }
            });
        }
        let mut job_tx = Some(job_tx);
        let mut pending = 1usize;
        job_tx
            .as_ref()
            .unwrap()
            .send((
                0,
                root.to_path_buf(),
                if ex.defaults { gi_above(root) } else { None },
            ))
            .unwrap();
        while pending > 0 {
            let Ok((parent, dir, gi, res)) = done_rx.recv() else {
                break;
            };
            pending -= 1;
            let mut entries = match res {
                Ok(v) => v,
                Err(e) => {
                    if e.kind() == io::ErrorKind::PermissionDenied {
                        stats.permission_denied += 1;
                    }
                    continue;
                }
            };
            let gi = retain(ex, &dir, &gi, &mut entries, |e| {
                (&e.name, e.kind == Kind::Dir)
            });
            for e in entries {
                count(&mut stats, e.kind);
                let id = index.push(parent, &e.name, e.kind, e.size, e.mtime);
                if let Some(p) = e.dir_path {
                    job_tx.as_ref().unwrap().send((id, p, gi.clone())).unwrap();
                    pending += 1;
                }
                if id % 8192 == 0 {
                    progress(id as u64, &dir);
                }
            }
        }
        job_tx.take(); // workers see a closed channel and exit
    });
    stats.elapsed = start.elapsed();
    Ok((index, stats))
}

fn count(stats: &mut ScanStats, kind: Kind) {
    match kind {
        Kind::File => stats.files += 1,
        Kind::Dir => stats.dirs += 1,
        Kind::Symlink => stats.symlinks += 1,
    }
}

/// One directory entry as listed by `list_dir`.
struct Entry {
    name: String,
    kind: Kind,
    size: u64,
    mtime: i64,
    /// Full path (raw bytes, not NFC) for directories to descend into; None for SKIP dirs.
    dir_path: Option<PathBuf>,
}

impl Entry {
    fn new(dir: &Path, raw: &std::ffi::OsStr, kind: Kind, size: u64, mtime: i64) -> Entry {
        let dir_path = (kind == Kind::Dir)
            .then(|| dir.join(raw))
            .filter(|p| !SKIP.iter().any(|s| p == Path::new(s)));
        Entry {
            name: nfc(raw),
            kind,
            size: if kind == Kind::Dir { 0 } else { size },
            mtime,
            dir_path,
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn list_dir(dir: &Path, _buf: &mut Vec<u8>) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        if let Ok(m) = e.metadata() {
            let (kind, size, mtime) = meta_of(&m);
            out.push(Entry::new(dir, &e.file_name(), kind, size, mtime));
        }
    }
    Ok(out)
}

/// List `dir` with getattrlistbulk(2): names, types, sizes and mtimes for many entries
/// per syscall instead of readdir + one lstat per entry.
#[cfg(target_os = "macos")]
fn list_dir(dir: &Path, buf: &mut Vec<u8>) -> io::Result<Vec<Entry>> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    const VDIR: u32 = 2;
    const VLNK: u32 = 5;

    let c = std::ffi::CString::new(dir.as_os_str().to_owned().into_vec())?;
    // SAFETY: valid NUL-terminated path; fd is closed by OwnedFd.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    let mut al = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | ATTR_CMN_ERROR
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: libc::ATTR_FILE_DATALENGTH,
        forkattr: 0,
    };
    let mut out = Vec::new();
    loop {
        // SAFETY: `al` and `buf` are valid for the call; the kernel writes at most buf.len() bytes.
        let n = unsafe {
            libc::getattrlistbulk(
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                &mut al as *mut _ as *mut libc::c_void,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Ok(out);
        }
        let mut p = 0usize;
        for _ in 0..n {
            let Some(len) = rd_u32(buf, p) else { break };
            let len = len as usize;
            let Some(e) = buf.get(p..p + len).filter(|_| len >= 4) else {
                break;
            };
            p += len;
            if let Some((raw, kind, size, mtime)) = parse_entry(e, VDIR, VLNK) {
                out.push(Entry::new(
                    dir,
                    std::ffi::OsStr::from_bytes(raw),
                    kind,
                    size,
                    mtime,
                ));
            }
        }
    }
}

/// <sys/attr.h>; not exported by the libc crate.
#[cfg(target_os = "macos")]
const ATTR_CMN_ERROR: u32 = 0x2000_0000;

fn rd_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        b.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn rd_i64(b: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_ne_bytes(
        b.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

/// Parse one packed getattrlistbulk record (`e` includes its leading length word).
/// Layout: length, attribute_set_t returned, [error], [name ref], [objtype], [modtime], [datalength];
/// attributes missing from `returned` take no space. Every read is bounds-checked.
#[cfg(target_os = "macos")]
fn parse_entry(e: &[u8], vdir: u32, vlnk: u32) -> Option<(&[u8], Kind, u64, i64)> {
    let common = rd_u32(e, 4)?;
    let file = rd_u32(e, 4 + 12)?;
    let mut c = 4 + 20; // past length + attribute_set_t (5 x u32)
    if common & ATTR_CMN_ERROR != 0 {
        if rd_u32(e, c)? != 0 {
            return None; // kernel couldn't get this entry's attributes
        }
        c += 4;
    }
    if common & libc::ATTR_CMN_NAME == 0 {
        return None;
    }
    // attrreference_t: offset is relative to the attrreference itself; length includes the NUL.
    let off = rd_u32(e, c)? as i32;
    let nlen = rd_u32(e, c + 4)? as usize;
    let start = usize::try_from(c as i64 + off as i64).ok()?;
    let name = e.get(start..start.checked_add(nlen)?)?;
    let name = name.split(|&b| b == 0).next()?;
    if name.is_empty() {
        return None;
    }
    c += 8;
    let mut kind = Kind::File;
    if common & libc::ATTR_CMN_OBJTYPE != 0 {
        kind = match rd_u32(e, c)? {
            t if t == vdir => Kind::Dir,
            t if t == vlnk => Kind::Symlink,
            _ => Kind::File,
        };
        c += 4;
    }
    let mut mtime = 0;
    if common & libc::ATTR_CMN_MODTIME != 0 {
        mtime = rd_i64(e, c)?.max(0); // timespec.tv_sec; pre-epoch -> 0 like the std path
        c += 16;
    }
    let mut size = 0;
    if file & libc::ATTR_FILE_DATALENGTH != 0 {
        size = rd_i64(e, c)?.max(0) as u64;
    }
    Some((name, kind, size, mtime))
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
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
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

/// `read_dir` minus SKIP paths and entries that vanished meanwhile: (path, NFC name, lstat).
fn list_std(rd: std::fs::ReadDir) -> Vec<(PathBuf, String, std::fs::Metadata)> {
    rd.flatten()
        .filter(|e| !SKIP.iter().any(|s| e.path() == Path::new(s)))
        .filter_map(|e| Some((e.path(), nfc(&e.file_name()), e.metadata().ok()?)))
        .collect()
}

/// None if `name` in `dir` is excluded, else the gitignore state it inherits.
fn admit(ex: &Excludes, dir: &Path, name: &str, is_dir: bool) -> Option<Gi> {
    if !ex.is_active() {
        return Some(None);
    }
    if ex.skip(dir, name, is_dir) || (ex.defaults && dir.join("pyvenv.cfg").exists()) {
        return None;
    }
    let gi = if ex.defaults {
        let (git, gitignore) = (dir.join(".git").exists(), dir.join(".gitignore").is_file());
        enter(&gi_above(dir), dir, git, gitignore)
    } else {
        None
    };
    (!ignored(&gi, &dir.join(name), is_dir)).then_some(gi)
}

/// Push `path` under `parent`, and its whole subtree (minus `ex`) if it is a directory.
/// `gi` is the gitignore state `path` inherits.
fn add_tree(
    ix: &mut Index,
    parent: u32,
    path: &Path,
    name: &str,
    m: &std::fs::Metadata,
    ex: &Excludes,
    gi: &Gi,
) {
    let (kind, size, mtime) = meta_of(m);
    let id = ix.push(parent, name, kind, size, mtime);
    if kind != Kind::Dir {
        return;
    }
    let Ok(rd) = std::fs::read_dir(path) else {
        return;
    };
    let mut list = list_std(rd);
    let gi = retain(ex, path, gi, &mut list, |(_, n, m)| (n, m.is_dir()));
    for (p, n, m) in list {
        add_tree(ix, id, &p, &n, &m, ex, &gi);
    }
}

/// Diff one directory's direct children against disk: new -> push (with subtree),
/// missing -> tombstone (recursively), changed kind/size/mtime -> update.
/// If `dir` is not indexed or no longer on disk, the nearest indexed ancestor is rescanned instead.
/// Returns the paths of the live child directories of the directory actually rescanned.
/// Children now matching `ex` are removed.
pub fn rescan_dir(ix: &mut Index, dir: &Path, ex: &Excludes) -> Result<Vec<PathBuf>> {
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
        Err(e)
            if id != 0
                && matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
        {
            return rescan_dir(ix, dir.parent().unwrap_or(&dir), ex);
        }
        Err(e) => return Err(e.into()),
    };
    let mut list = list_std(rd);
    let gi = if ex.defaults { gi_above(&dir) } else { None };
    let gi = retain(ex, &dir, &gi, &mut list, |(_, n, m)| (n, m.is_dir()));
    if let Ok(m) = std::fs::symlink_metadata(&dir) {
        ix.nodes[id as usize].mtime = meta_of(&m).2;
    }
    let mut old: HashMap<String, u32> = ix
        .children(id)
        .map(|c| (ix.name(c).to_owned(), c))
        .collect();
    let mut subdirs = Vec::new();
    for (p, name, m) in list {
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
                add_tree(ix, id, &p, &name, &m, ex, &gi);
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
pub fn apply_path(ix: &mut Index, path: &Path, ex: &Excludes) -> Result<()> {
    let (Some(parent), Some(raw)) = (path.parent(), path.file_name()) else {
        return rescan_dir(ix, path, ex).map(|_| ());
    };
    if SKIP.iter().any(|s| path == Path::new(s)) {
        return Ok(());
    }
    let Some(pid) = ix
        .find(parent)
        .filter(|&p| ix.nodes[p as usize].kind == Kind::Dir)
    else {
        return rescan_dir(ix, parent, ex).map(|_| ());
    };
    let name = nfc(raw);
    let existing = ix.children(pid).find(|&c| ix.name(c) == name);
    let meta = std::fs::symlink_metadata(path);
    let gi = match &meta {
        Ok(m) => match admit(ex, parent, &name, m.is_dir()) {
            Some(gi) => gi,
            None => {
                if let Some(c) = existing {
                    ix.remove(c);
                }
                return Ok(());
            }
        },
        Err(_) => None,
    };
    match (meta, existing) {
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
            add_tree(ix, pid, path, &name, &m, ex, &gi);
        }
        (Err(e), Some(c)) if e.kind() == io::ErrorKind::NotFound => ix.remove(c),
        (Err(e), _) if e.kind() == io::ErrorKind::NotFound => {}
        (Err(e), _) => return Err(e.into()),
    }
    Ok(())
}

/// `rescan_dir` on `dir` and every directory below it.
pub fn rescan_subtree(ix: &mut Index, dir: &Path, ex: &Excludes) -> Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        match rescan_dir(ix, &d, ex) {
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

    static NONE: std::sync::LazyLock<Excludes> = std::sync::LazyLock::new(Excludes::none);

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
        assert_eq!(
            ix.nodes[h as usize].flags & crate::index::FLAG_HIDDEN,
            crate::index::FLAG_HIDDEN
        );
        find("\u{e9}.txt"); // NFC
    }

    /// Plain std::fs walk: (path, kind, size, mtime) for every entry below `dir`.
    fn reference(dir: &Path, out: &mut Vec<(String, Kind, u64, i64)>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let m = fs::symlink_metadata(e.path()).unwrap();
            let (kind, size, mtime) = meta_of(&m);
            out.push((
                e.path().to_string_lossy().nfc().collect(),
                kind,
                size,
                mtime,
            ));
            if kind == Kind::Dir {
                reference(&e.path(), out);
            }
        }
    }

    #[test]
    fn matches_std_walk() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let r = t.path().canonicalize().unwrap();
        fs::create_dir_all(r.join("a/b/c")).unwrap();
        for i in 0..3000 {
            fs::write(r.join(format!("a/f{i}")), vec![0u8; i % 7]).unwrap(); // > one bulk buffer
        }
        fs::write(r.join("a/b/c/x.txt"), b"hello").unwrap();
        fs::write(r.join(".hidden"), b"h").unwrap();
        fs::write(r.join("e\u{301}.txt"), b"").unwrap();
        fs::create_dir(r.join("d\u{301}ir")).unwrap();
        fs::write(r.join("d\u{301}ir/in"), b"").unwrap();
        std::os::unix::fs::symlink("a/b", r.join("link")).unwrap();
        fs::File::create(r.join("big"))
            .unwrap()
            .set_len(5 << 30)
            .unwrap();
        fs::write(r.join("n".repeat(255)), b"").unwrap();
        fs::create_dir(r.join("locked")).unwrap();
        fs::write(r.join("locked/secret"), b"").unwrap();
        fs::set_permissions(r.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

        let (ix, st) = scan(&r).unwrap();
        let mut want = Vec::new();
        reference(&r, &mut want);
        fs::set_permissions(r.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();
        let mut got: Vec<_> = (1..ix.len() as u32)
            .map(|i| {
                let n = ix.nodes[i as usize];
                (ix.path(i), n.kind, n.size, n.mtime)
            })
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got, want);
        assert_eq!(st.permission_denied, 1);
        assert_eq!(st.symlinks, 1);
        assert_eq!(ix.path(0), r.to_string_lossy());
    }

    fn live_paths(ix: &Index) -> Vec<String> {
        let mut v: Vec<String> = (0..ix.len() as u32)
            .filter(|&i| ix.is_live(i))
            .map(|i| ix.path(i))
            .collect();
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
        rescan_dir(&mut ix, &r.join("a"), &NONE).unwrap();

        let want: Vec<String> = [
            "",
            "/a",
            "/a/keep.txt",
            "/a/nd",
            "/a/nd/x",
            "/a/nd/x/in.txt",
            "/a/new.txt",
        ]
        .iter()
        .map(|s| format!("{rs}{s}"))
        .collect();
        assert_eq!(live_paths(&ix), want);
        assert_eq!(ix.live_count(), want.len());
        assert_eq!(
            ix.nodes[ix.find(&r.join("a/keep.txt")).unwrap() as usize].size,
            5
        );
        assert!(ix.find(&r.join("a/b/c/deep.txt")).is_none());
        // tombstones are hidden from search
        let res = crate::search(&ix, &crate::parse("deep").unwrap(), 10, crate::Sort::Score);
        assert_eq!(res.total, 0);

        // event for a path that no longer exists falls back to the nearest ancestor
        fs::remove_dir_all(r.join("a/nd")).unwrap();
        rescan_dir(&mut ix, &r.join("a/nd/x"), &NONE).unwrap();
        assert!(ix.find(&r.join("a/nd")).is_none());

        // subtree rescan catches changes below the first level
        fs::write(r.join("a/nd2.txt"), b"").unwrap();
        fs::create_dir_all(r.join("z/y")).unwrap();
        rescan_subtree(&mut ix, &r, &NONE).unwrap();
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
        rescan_dir(&mut ix, &r, &NONE).unwrap();
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
        rescan_dir(&mut ix, &r.join("d"), &NONE).unwrap();
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
        apply_path(&mut ix, &r.join("e\u{301}.md"), &NONE).unwrap();
        assert!(ix.find(&r.join("\u{e9}.md")).is_some());
        // modify
        fs::write(r.join("a.txt"), b"12345").unwrap();
        apply_path(&mut ix, &r.join("a.txt"), &NONE).unwrap();
        assert_eq!(
            ix.nodes[ix.find(&r.join("a.txt")).unwrap() as usize].size,
            5
        );
        // new dir brings its subtree
        fs::create_dir_all(r.join("n/m")).unwrap();
        fs::write(r.join("n/m/deep"), b"").unwrap();
        apply_path(&mut ix, &r.join("n"), &NONE).unwrap();
        assert!(ix.find(&r.join("n/m/deep")).is_some());
        // delete dir removes subtree; deleting twice is fine
        fs::remove_dir_all(r.join("n")).unwrap();
        apply_path(&mut ix, &r.join("n"), &NONE).unwrap();
        apply_path(&mut ix, &r.join("n"), &NONE).unwrap();
        assert!(ix.find(&r.join("n/m/deep")).is_none());
        // parent not indexed yet -> falls back to rescanning the ancestor
        fs::create_dir_all(r.join("x/y")).unwrap();
        fs::write(r.join("x/y/z"), b"").unwrap();
        apply_path(&mut ix, &r.join("x/y/z"), &NONE).unwrap();
        assert!(ix.find(&r.join("x/y/z")).is_some());
    }

    /// Junk dirs, a venv with a custom name, gitignored output, user globs.
    fn junk_tree(r: &Path) {
        fs::create_dir_all(r.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(r.join("myenv/lib")).unwrap();
        fs::write(r.join("myenv/pyvenv.cfg"), b"").unwrap();
        fs::create_dir_all(r.join("repo/.git")).unwrap();
        fs::create_dir_all(r.join("repo/out")).unwrap();
        fs::create_dir_all(r.join("repo/src")).unwrap();
        fs::write(r.join("repo/.gitignore"), b"out/\n*.tmp\n").unwrap();
        fs::write(r.join("repo/out/a.js"), b"").unwrap();
        fs::write(r.join("repo/src/x.tmp"), b"").unwrap();
        fs::write(r.join("repo/src/main.rs"), b"").unwrap();
        fs::write(r.join("notes.log"), b"").unwrap();
        fs::write(r.join("keep.txt"), b"").unwrap();
    }

    fn junk_ex() -> Excludes {
        Excludes::new(true, &["*.log".into()], Path::new("/")).unwrap()
    }

    fn junk_want(r: &Path) -> Vec<String> {
        let mut v: Vec<String> = [
            "",
            "/keep.txt",
            "/myenv",
            "/repo",
            "/repo/.gitignore",
            "/repo/src",
            "/repo/src/main.rs",
        ]
        .iter()
        .map(|s| format!("{}{s}", r.display()))
        .collect();
        v.sort();
        v
    }

    #[test]
    fn scan_skips_excludes() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().canonicalize().unwrap();
        junk_tree(&r);
        let (ix, _) = scan_with_progress(&r, &junk_ex(), |_, _| {}).unwrap();
        assert_eq!(live_paths(&ix), junk_want(&r));
        // and nothing is skipped without excludes
        let (ix, _) = scan(&r).unwrap();
        assert!(ix.find(&r.join("node_modules/pkg")).is_some());
    }

    #[test]
    fn rescans_keep_excludes_out() {
        let t = tempfile::tempdir().unwrap();
        let r = t.path().canonicalize().unwrap();
        let (mut ix, _) = scan(&r).unwrap();
        junk_tree(&r);
        let ex = junk_ex();
        rescan_subtree(&mut ix, &r, &ex).unwrap();
        assert_eq!(live_paths(&ix), junk_want(&r));
        // single-path events inside a repo, a venv and a junk dir
        for p in [
            "repo/src/y.tmp",
            "repo/src/ok.rs",
            "myenv/lib/z.py",
            "node_modules/q.js",
        ] {
            fs::write(r.join(p), b"").unwrap();
            apply_path(&mut ix, &r.join(p), &ex).unwrap();
        }
        let mut want = junk_want(&r);
        want.push(format!("{}/repo/src/ok.rs", r.display()));
        want.sort();
        assert_eq!(live_paths(&ix), want);
        // an existing index loses newly excluded entries on rescan
        let (mut ix, _) = scan(&r).unwrap();
        rescan_subtree(&mut ix, &r, &ex).unwrap();
        assert_eq!(live_paths(&ix), want);
    }
}
