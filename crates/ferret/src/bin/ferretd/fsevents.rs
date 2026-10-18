//! One FSEvents stream on the root, run on its own CFRunLoop thread, feeding a bounded channel.

use objc2_core_foundation::{kCFRunLoopDefaultMode, CFArray, CFRunLoop, CFString};
use objc2_core_services as fs;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::ffi::{c_char, c_void, CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

pub const MUST_SCAN_SUB_DIRS: u32 = fs::kFSEventStreamEventFlagMustScanSubDirs;
pub const USER_DROPPED: u32 = fs::kFSEventStreamEventFlagUserDropped;
pub const KERNEL_DROPPED: u32 = fs::kFSEventStreamEventFlagKernelDropped;

pub struct Event {
    pub path: PathBuf,
    pub flags: u32,
    pub id: u64,
}

/// Parent dirs of events dropped because the channel was full, and their max id.
/// Past OVERFLOW_CAP dirs, the root itself is recorded and the writer rescans everything.
pub type Overflow = Arc<Mutex<Option<(HashSet<PathBuf>, u64)>>>;
pub const OVERFLOW_CAP: usize = 100_000;

struct Ctx {
    tx: SyncSender<Event>,
    overflow: Overflow,
}

pub fn current_event_id() -> u64 {
    unsafe { fs::FSEventsGetCurrentEventId() }
}

unsafe extern "C-unwind" fn callback(
    _stream: fs::ConstFSEventStreamRef,
    info: *mut c_void,
    n: usize,
    paths: NonNull<c_void>,
    flags: NonNull<fs::FSEventStreamEventFlags>,
    ids: NonNull<fs::FSEventStreamEventId>,
) {
    // SAFETY: info is the leaked Ctx; without UseCFTypes, paths is a C array of n C strings.
    let ctx = unsafe { &*(info as *const Ctx) };
    let paths = paths.as_ptr() as *const *const c_char;
    let (flags, ids) = (flags.as_ptr() as *const u32, ids.as_ptr() as *const u64);
    for i in 0..n {
        let (path, flags, id) = unsafe {
            let p = CStr::from_ptr(*paths.add(i));
            (
                PathBuf::from(OsStr::from_bytes(p.to_bytes())),
                *flags.add(i),
                *ids.add(i),
            )
        };
        // Never block the run loop: on a full channel remember only where to rescan.
        if let Err(TrySendError::Full(ev)) = ctx.tx.try_send(Event { path, flags, id }) {
            let mut o = ctx.overflow.lock();
            let (dirs, max) = o.get_or_insert_with(Default::default);
            *max = (*max).max(ev.id);
            if dirs.len() < OVERFLOW_CAP {
                dirs.insert(ev.path.parent().map_or(ev.path.clone(), Path::to_path_buf));
            } else {
                dirs.insert(PathBuf::from("/"));
            }
        }
    }
}

/// Start watching `root` from event id `since` on a dedicated thread.
pub fn start(root: &Path, since: u64) -> (Receiver<Event>, Overflow) {
    let (tx, rx) = sync_channel(65_536);
    let overflow: Overflow = Arc::default();
    let ctx = Ctx {
        tx,
        overflow: overflow.clone(),
    };
    let root = root.to_string_lossy().into_owned();
    std::thread::Builder::new()
        .name("fsevents".into())
        .spawn(move || unsafe {
            let ctx = Box::into_raw(Box::new(ctx)); // lives for the process
            let root_cf = CFString::from_str(&root);
            let paths = CFArray::from_objects(&[&*root_cf]);
            let mut context = fs::FSEventStreamContext {
                version: 0,
                info: ctx as *mut c_void,
                retain: None,
                release: None,
                copyDescription: None,
            };
            let flags = fs::kFSEventStreamCreateFlagFileEvents
                | fs::kFSEventStreamCreateFlagNoDefer
                | fs::kFSEventStreamCreateFlagWatchRoot;
            let stream = fs::FSEventStreamCreate(
                None,
                Some(callback),
                &mut context,
                paths.as_opaque(),
                since,
                0.5,
                flags,
            );
            let (Some(rl), Some(mode)) = (CFRunLoop::current(), kCFRunLoopDefaultMode) else {
                return;
            };
            if stream.is_null() {
                tracing::error!("FSEventStreamCreate failed; live updates disabled");
                return;
            }
            #[allow(deprecated)] // a plain CFRunLoop thread avoids pulling in dispatch2
            fs::FSEventStreamScheduleWithRunLoop(stream, &rl, mode);
            if !fs::FSEventStreamStart(stream) {
                tracing::error!("FSEventStreamStart failed; live updates disabled");
                return;
            }
            tracing::info!(root, since, "watching FSEvents");
            CFRunLoop::run();
        })
        .expect("spawn fsevents thread");
    (rx, overflow)
}
