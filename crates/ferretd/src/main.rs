//! ferretd: keeps the index in memory, live-updated from FSEvents, and answers NDJSON queries
//! on a Unix socket.

mod fsevents;

use ferret_core::protocol::{self, HitOut, KindFilter, Op, Request, Response, Status};
use ferret_core::snapshot::{self, Meta};
use ferret_core::{scan, Index, Kind};
use parking_lot::{Mutex, RwLock};
use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info, warn};

pub struct Shared {
    index: RwLock<Index>,
    root: PathBuf,
    snapshot: PathBuf,
    volume_uuid: String,
    last_event_id: AtomicU64,
    dirty: AtomicBool,
    indexing: AtomicBool,
    /// (progress 0..1, current path) while indexing.
    progress: Mutex<(f32, Option<String>)>,
    permission_denied: AtomicU64,
    save_lock: Mutex<()>,
}

struct Args {
    root: PathBuf,
    snapshot: PathBuf,
    socket: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { root: "/".into(), snapshot: snapshot::default_path(), socket: protocol::socket_path() };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().map(PathBuf::from).ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--root" => a.root = val()?,
            "--snapshot" => a.snapshot = val()?,
            "--socket" => a.socket = val()?,
            _ => return Err(format!("unknown argument {flag}\nusage: ferretd [--root /] [--snapshot PATH] [--socket PATH]")),
        }
    }
    Ok(a)
}

/// Root volume UUID from `diskutil info -plist /`; empty if unavailable.
fn volume_uuid() -> String {
    let out = std::process::Command::new("/usr/sbin/diskutil").args(["info", "-plist", "/"]).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    text.split_once("<key>VolumeUUID</key>")
        .and_then(|(_, rest)| rest.split_once("<string>"))
        .and_then(|(_, rest)| rest.split_once("</string>"))
        .map(|(uuid, _)| uuid.trim().to_owned())
        .unwrap_or_default()
}

fn save(sh: &Shared) {
    let _g = sh.save_lock.lock();
    sh.dirty.store(false, Relaxed);
    let start = Instant::now();
    let meta = Meta { last_event_id: sh.last_event_id.load(Relaxed), volume_uuid: sh.volume_uuid.clone() };
    let ix = sh.index.read();
    match snapshot::save_with(&ix, &meta, &sh.snapshot) {
        Ok(()) => info!(entries = ix.live_count(), ms = start.elapsed().as_millis() as u64, "snapshot saved"),
        Err(e) => {
            sh.dirty.store(true, Relaxed);
            error!("snapshot save failed: {e}");
        }
    }
}

fn full_scan(sh: &Shared) {
    sh.indexing.store(true, Relaxed);
    // Size of the index being replaced, if any; 0 = no idea, report progress 0.
    let guess = sh.index.read().len() as f32;
    info!(root = %sh.root.display(), "full scan started");
    let res = scan::scan_with_progress(&sh.root, |n, p| {
        let progress = if guess > 1.0 { (n as f32 / guess).min(0.99) } else { 0.0 };
        *sh.progress.lock() = (progress, Some(p.to_string_lossy().into_owned()));
    });
    match res {
        Ok((ix, st)) => {
            info!(entries = ix.len(), secs = st.elapsed.as_secs(), denied = st.permission_denied, "full scan done");
            sh.permission_denied.store(st.permission_denied, Relaxed);
            *sh.index.write() = ix;
            sh.dirty.store(true, Relaxed);
        }
        Err(e) => error!("full scan failed: {e}"),
    }
    *sh.progress.lock() = (0.0, None);
    sh.indexing.store(false, Relaxed);
    save(sh);
}

/// Batches FSEvents, then rescans each changed parent dir under the write lock.
fn writer(sh: Arc<Shared>, rx: Receiver<fsevents::Event>, overflow: fsevents::Overflow) {
    use fsevents::{KERNEL_DROPPED, MUST_SCAN_SUB_DIRS, USER_DROPPED};
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    let mut subtrees: HashSet<PathBuf> = HashSet::new();
    let mut max_id = 0u64;
    loop {
        let mut deadline = None;
        loop {
            let wait = deadline.map_or(Duration::from_secs(1), |d: Instant| d.saturating_duration_since(Instant::now()));
            let ev = match rx.recv_timeout(wait) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            };
            deadline.get_or_insert_with(|| Instant::now() + Duration::from_millis(200));
            max_id = max_id.max(ev.id);
            let path = if ev.path.starts_with(&sh.root) {
                ev.path
            } else if sh.root.starts_with(&ev.path) {
                sh.root.clone() // e.g. a dropped-events notice for "/"
            } else {
                continue;
            };
            if scan::SKIP.iter().any(|s| path.starts_with(s)) {
                continue;
            }
            if ev.flags & (MUST_SCAN_SUB_DIRS | USER_DROPPED | KERNEL_DROPPED) != 0 {
                subtrees.insert(path);
            } else if path == sh.root {
                dirs.insert(path);
            } else if let Some(parent) = path.parent() {
                dirs.insert(parent.to_path_buf());
            }
        }
        if let Some((p, id)) = overflow.lock().take() {
            warn!(ancestor = %p.display(), "event channel overflowed; rescanning subtree");
            max_id = max_id.max(id);
            subtrees.insert(if p.starts_with(&sh.root) { p } else { sh.root.clone() });
        }
        // shortcut: past 100k pending dirs, fall back to rescanning the whole root.
        if dirs.len() > 100_000 {
            dirs.clear();
            subtrees.insert(sh.root.clone());
        }
        // Events seen during a full scan are applied after the swap.
        if sh.indexing.load(Relaxed) || (dirs.is_empty() && subtrees.is_empty()) {
            continue;
        }
        let start = Instant::now();
        let n = dirs.len() + subtrees.len();
        dirs.retain(|d| !subtrees.iter().any(|s| d.starts_with(s)));
        for d in dirs.drain() {
            if let Err(e) = scan::rescan_dir(&mut sh.index.write(), &d) {
                warn!(dir = %d.display(), "rescan failed: {e}");
            }
        }
        for s in subtrees.drain() {
            // One directory per write-lock hold so searches keep flowing during big rescans.
            let mut stack = vec![s];
            while let Some(d) = stack.pop() {
                match scan::rescan_dir(&mut sh.index.write(), &d) {
                    Ok(sub) => stack.extend(sub),
                    Err(e) => warn!(dir = %d.display(), "rescan failed: {e}"),
                }
            }
        }
        {
            let mut ix = sh.index.write();
            if ix.maybe_compact() {
                info!(entries = ix.len(), "compacted index");
            }
        }
        if max_id > sh.last_event_id.load(Relaxed) {
            sh.last_event_id.store(max_id, Relaxed);
        }
        sh.dirty.store(true, Relaxed);
        // Only log big batches: routine lines would hit the log file, whose own events would log again.
        let ms = start.elapsed().as_millis() as u64;
        if n > 50 || ms > 100 {
            info!(dirs = n, ms, "applied events");
        }
    }
}

fn status(sh: &Shared) -> Status {
    let (progress, current_path) = sh.progress.lock().clone();
    Status {
        files: sh.index.read().live_count() as u64,
        indexing: sh.indexing.load(Relaxed),
        progress,
        current_path,
        last_event_id: sh.last_event_id.load(Relaxed),
        permission_denied: sh.permission_denied.load(Relaxed),
    }
}

fn handle(sh: &Shared, line: &str) -> Response {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            let v: serde_json::Value = serde_json::from_str(line).unwrap_or_default();
            return Response {
                v: protocol::VERSION,
                id: v["id"].as_u64().unwrap_or(0),
                error: Some(format!("bad request: {e}")),
                ..Default::default()
            };
        }
    };
    let mut resp = Response { v: req.v, id: req.id, ..Default::default() };
    match req.op {
        Op::Status => resp.status = Some(status(sh)),
        Op::Search { q, limit, sort, kind } => {
            let mut query = match ferret_core::parse(&q) {
                Ok(q) => q,
                Err(e) => {
                    resp.error = Some(e.to_string());
                    return resp;
                }
            };
            let want = match kind {
                KindFilter::All => None,
                KindFilter::File => Some(Kind::File),
                KindFilter::Dir => Some(Kind::Dir),
            };
            if let Some(w) = want {
                if query.kind.is_some_and(|k| k != w) {
                    return resp; // `type:dir` with kind=file: nothing can match
                }
                query.kind = Some(w);
            }
            let r = ferret_core::search(&sh.index.read(), &query, limit.min(10_000), sort.into());
            resp.took_us = r.took_us;
            resp.total = r.total;
            resp.hits =
                r.hits.into_iter().map(|h| HitOut { path: h.path, kind: h.kind, size: h.size, mtime: h.mtime }).collect();
        }
    }
    resp
}

async fn serve_conn(sh: Arc<Shared>, stream: UnixStream) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let sh2 = sh.clone();
        let resp = tokio::task::spawn_blocking(move || handle(&sh2, &line)).await?;
        let mut out = serde_json::to_vec(&resp)?;
        out.push(b'\n');
        w.write_all(&out).await?;
    }
    Ok(())
}

fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "another ferretd is already listening"));
    }
    let _ = std::fs::remove_file(path); // stale socket from a crash
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

fn main() {
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_ansi(false).init();
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let root = match args.root.canonicalize() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("bad --root {}: {e}", args.root.display());
            std::process::exit(2);
        }
    };
    let uuid = volume_uuid();
    let start = Instant::now();
    let loaded = match snapshot::load_with(&args.snapshot) {
        Ok((ix, meta)) if ix.is_empty() || Path::new(ix.name(0)) != root => {
            warn!("snapshot is for another root; rescanning");
            drop((ix, meta));
            None
        }
        // An empty uuid means the snapshot came from `ff index`: trust it, follow events from now.
        Ok((_, meta)) if !meta.volume_uuid.is_empty() && meta.volume_uuid != uuid => {
            warn!("volume UUID changed; rescanning");
            None
        }
        Ok(x) => Some(x),
        Err(e) => {
            warn!("no usable snapshot ({e}); rescanning");
            None
        }
    };
    let need_scan = loaded.is_none();
    let (index, meta) = loaded.unwrap_or_default();
    info!(entries = index.live_count(), ms = start.elapsed().as_millis() as u64, "snapshot loaded");
    let since = if meta.last_event_id > 0 { meta.last_event_id } else { fsevents::current_event_id() };
    let sh = Arc::new(Shared {
        index: RwLock::new(index),
        root: root.clone(),
        snapshot: args.snapshot,
        volume_uuid: uuid,
        last_event_id: AtomicU64::new(since),
        dirty: AtomicBool::new(false),
        indexing: AtomicBool::new(need_scan),
        progress: Mutex::new((0.0, None)),
        permission_denied: AtomicU64::new(0),
        save_lock: Mutex::new(()),
    });

    let (rx, overflow) = fsevents::start(&root, since);
    std::thread::Builder::new().name("writer".into()).spawn({
        let sh = sh.clone();
        move || writer(sh, rx, overflow)
    }).expect("spawn writer");
    if need_scan {
        let sh = sh.clone();
        std::thread::Builder::new().name("scan".into()).spawn(move || full_scan(&sh)).expect("spawn scan");
    }

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async move {
        let listener = match bind(&args.socket) {
            Ok(l) => l,
            Err(e) => {
                error!(socket = %args.socket.display(), "bind failed: {e}");
                std::process::exit(1);
            }
        };
        info!(socket = %args.socket.display(), "listening");
        let sh_save = sh.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(300));
            tick.tick().await;
            loop {
                tick.tick().await;
                if sh_save.dirty.load(Relaxed) && !sh_save.indexing.load(Relaxed) {
                    let sh = sh_save.clone();
                    let _ = tokio::task::spawn_blocking(move || save(&sh)).await;
                }
            }
        });
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");
        loop {
            tokio::select! {
                acc = listener.accept() => match acc {
                    Ok((stream, _)) => {
                        let sh = sh.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_conn(sh, stream).await {
                                warn!("connection: {e}");
                            }
                        });
                    }
                    Err(e) => warn!("accept: {e}"),
                },
                _ = tokio::signal::ctrl_c() => break,
                _ = term.recv() => break,
            }
        }
        info!("shutting down");
        if sh.dirty.load(Relaxed) && !sh.indexing.load(Relaxed) {
            let sh = sh.clone();
            let _ = tokio::task::spawn_blocking(move || save(&sh)).await;
        }
        let _ = std::fs::remove_file(&args.socket);
    });
    std::process::exit(0);
}
