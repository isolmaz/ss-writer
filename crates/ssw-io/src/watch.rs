//! Folder watching (spec §5): one `ReadDirectoryChangesW` overlapped reader thread per root,
//! rename pairing and 50 ms debouncing in a pure [`Coalescer`], own-write suppression and
//! external-change classification for open documents.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::core::{Owned, PCWSTR};

use crate::fs::{FileSystem, FsError, io, wide, win};
use crate::save::SaveStamp;

/// Quiet period after the last raw change before events are released.
pub const DEBOUNCE: Duration = Duration::from_millis(50);
/// Upper bound on how long a continuous stream of changes can delay delivery.
pub const MAX_DELAY: Duration = Duration::from_millis(500);

/// Size of the `ReadDirectoryChangesW` buffer in bytes.
const BUFFER_BYTES: usize = 64 * 1024;

/// A coalesced change below a watched root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsEvent {
    /// Absolute path of the affected item (the new path for renames).
    pub path: PathBuf,
    /// What happened.
    pub kind: FsEventKind,
}

/// Kind of a coalesced [`FsEvent`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsEventKind {
    /// Created (or moved in from outside the root).
    Created,
    /// Content or attributes changed.
    Modified,
    /// Deleted (or moved out of the root).
    Removed,
    /// Renamed or moved inside the root.
    Renamed {
        /// Previous absolute path.
        from: PathBuf,
    },
    /// Changes were lost (buffer overflow) or the watch ended: re-read the tree.
    Rescan,
}

/// Action of a raw `FILE_NOTIFY_INFORMATION` record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawAction {
    /// `FILE_ACTION_ADDED`.
    Added,
    /// `FILE_ACTION_REMOVED`.
    Removed,
    /// `FILE_ACTION_MODIFIED`.
    Modified,
    /// `FILE_ACTION_RENAMED_OLD_NAME`.
    RenamedOld,
    /// `FILE_ACTION_RENAMED_NEW_NAME`.
    RenamedNew,
    /// The kernel buffer overflowed; `path` is the watched root.
    Overflow,
}

/// One raw change as reported by the OS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawChange {
    /// Action.
    pub action: RawAction,
    /// Absolute path.
    pub path: PathBuf,
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

/// Parses a `FILE_NOTIFY_INFORMATION` chain of `bytes` valid bytes in `buffer`. Zero bytes means
/// the kernel buffer overflowed and yields [`RawAction::Overflow`] for `root`. Truncated records
/// end the parse; unknown actions are skipped.
#[must_use]
pub fn parse_notifications(buffer: &[u8], bytes: usize, root: &Path) -> Vec<RawChange> {
    if bytes == 0 {
        return vec![RawChange {
            action: RawAction::Overflow,
            path: root.to_path_buf(),
        }];
    }
    let data = &buffer[..bytes.min(buffer.len())];
    let mut changes = Vec::new();
    let mut offset = 0usize;
    while let (Some(next), Some(action), Some(length)) = (
        read_u32(data, offset),
        read_u32(data, offset + 4),
        read_u32(data, offset + 8),
    ) {
        let start = offset + 12;
        let Some(name) = data.get(start..start + length as usize) else {
            break;
        };
        let units: Vec<u16> = name
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        let action = match action {
            1 => Some(RawAction::Added),
            2 => Some(RawAction::Removed),
            3 => Some(RawAction::Modified),
            4 => Some(RawAction::RenamedOld),
            5 => Some(RawAction::RenamedNew),
            _ => None,
        };
        if let Some(action) = action {
            changes.push(RawChange {
                action,
                path: root.join(String::from_utf16_lossy(&units)),
            });
        }
        if next == 0 {
            break;
        }
        offset += next as usize;
    }
    changes
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Incoming {
    Created,
    Modified,
    Removed,
}

/// Merges raw changes into one event per path: Added+Modified → Created, Added+Removed →
/// nothing, Removed+Added → Modified, rename old/new pairs → Renamed. Pure and clock-driven so
/// it can be tested without the OS.
#[derive(Debug, Default)]
pub struct Coalescer {
    entries: Vec<Option<FsEvent>>,
    index: HashMap<String, usize>,
    pending_rename: Option<PathBuf>,
    first_push: Option<Instant>,
    last_push: Option<Instant>,
}

impl Coalescer {
    /// An empty coalescer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one raw change observed at `now`.
    pub fn push(&mut self, change: RawChange, now: Instant) {
        self.first_push.get_or_insert(now);
        self.last_push = Some(now);
        match change.action {
            RawAction::Added => self.merge(change.path, Incoming::Created),
            RawAction::Modified => self.merge(change.path, Incoming::Modified),
            RawAction::Removed => self.merge(change.path, Incoming::Removed),
            RawAction::RenamedOld => {
                // An earlier unpaired old name was moved out of the tree.
                if let Some(previous) = self.pending_rename.replace(change.path) {
                    self.merge(previous, Incoming::Removed);
                }
            }
            RawAction::RenamedNew => match self.pending_rename.take() {
                Some(from) => self.rename(from, change.path),
                None => self.merge(change.path, Incoming::Created),
            },
            RawAction::Overflow => self.insert(change.path, FsEventKind::Rescan),
        }
    }

    fn insert(&mut self, path: PathBuf, kind: FsEventKind) {
        let key = path_key(&path);
        if let Some(&old) = self.index.get(&key) {
            self.entries[old] = None;
        }
        self.index.insert(key, self.entries.len());
        self.entries.push(Some(FsEvent { path, kind }));
    }

    fn cancel(&mut self, path: &Path) -> Option<FsEvent> {
        let slot = self.index.remove(&path_key(path))?;
        self.entries[slot].take()
    }

    fn merge(&mut self, path: PathBuf, incoming: Incoming) {
        let existing = self
            .index
            .get(&path_key(&path))
            .and_then(|&slot| self.entries[slot].as_ref())
            .map(|event| event.kind.clone());
        let Some(existing) = existing else {
            let kind = match incoming {
                Incoming::Created => FsEventKind::Created,
                Incoming::Modified => FsEventKind::Modified,
                Incoming::Removed => FsEventKind::Removed,
            };
            self.insert(path, kind);
            return;
        };
        match (existing, incoming) {
            (FsEventKind::Created, Incoming::Removed) => {
                // Created and deleted inside one window: nothing happened.
                self.cancel(&path);
            }
            (FsEventKind::Renamed { from }, Incoming::Removed) => {
                self.insert(path, FsEventKind::Removed);
                self.merge(from, Incoming::Removed);
            }
            (FsEventKind::Removed, Incoming::Created) => {
                self.insert(path, FsEventKind::Modified);
            }
            (FsEventKind::Modified, Incoming::Removed) => {
                self.insert(path, FsEventKind::Removed);
            }
            // Created+Modified, Modified+Created/Modified, Renamed+Created/Modified,
            // Removed+Modified/Removed, Rescan+anything keep the existing event.
            _ => {}
        }
    }

    fn rename(&mut self, from: PathBuf, to: PathBuf) {
        // A file created and renamed within the window simply appears under the new name.
        let kind = match self.cancel(&from) {
            Some(FsEvent {
                kind: FsEventKind::Created,
                ..
            }) => FsEventKind::Created,
            _ => FsEventKind::Renamed { from },
        };
        self.insert(to, kind);
    }

    fn deadline(&self) -> Option<Instant> {
        let first = self.first_push?;
        let last = self.last_push?;
        Some((last + DEBOUNCE).min(first + MAX_DELAY))
    }

    /// How long to wait for more input before [`Self::take_ready`] releases events; `None` when
    /// nothing is pending.
    #[must_use]
    pub fn timeout(&self, now: Instant) -> Option<Duration> {
        self.deadline()
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    /// Releases the pending events when the debounce window has passed.
    pub fn take_ready(&mut self, now: Instant) -> Vec<FsEvent> {
        match self.deadline() {
            Some(deadline) if now >= deadline => self.take_all(),
            _ => Vec::new(),
        }
    }

    /// Releases everything immediately.
    pub fn take_all(&mut self) -> Vec<FsEvent> {
        if let Some(from) = self.pending_rename.take() {
            // Old name without a new name: the item left the watched tree.
            self.merge(from, Incoming::Removed);
        }
        self.index.clear();
        self.first_push = None;
        self.last_push = None;
        self.entries.drain(..).flatten().collect()
    }
}

/// Case-insensitive, separator-normalized key for comparing Windows paths.
#[must_use]
pub fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    text.trim_end_matches('\\').to_lowercase()
}

/// Result of waiting on a [`ChangeSource`].
#[derive(Debug)]
pub enum Poll {
    /// Raw changes arrived.
    Changes(Vec<RawChange>),
    /// The timeout elapsed without changes.
    Timeout,
    /// The watcher was asked to stop.
    Stop,
    /// The watch broke (root deleted, handle invalidated).
    Lost,
}

/// Where raw changes come from: [`ReadDirectoryChangesW`] in production, scripted in tests.
pub trait ChangeSource {
    /// Waits up to `timeout` (`None` = forever) for the next batch of changes.
    fn poll(&mut self, timeout: Option<Duration>) -> Poll;
}

/// Drives `source`: coalesces its changes and hands released batches to `emit`. Returns after
/// [`Poll::Stop`] or [`Poll::Lost`] (the latter ends with a [`FsEventKind::Rescan`] for `root`).
pub fn pump(
    source: &mut dyn ChangeSource,
    root: &Path,
    now: &mut dyn FnMut() -> Instant,
    emit: &mut dyn FnMut(Vec<FsEvent>),
) {
    let mut coalescer = Coalescer::new();
    loop {
        let timeout = coalescer.timeout(now());
        let polled = source.poll(timeout);
        let stamp = now();
        let mut finished = true;
        match polled {
            Poll::Changes(changes) => {
                finished = false;
                for change in changes {
                    coalescer.push(change, stamp);
                }
            }
            Poll::Timeout => finished = false,
            Poll::Stop => {}
            Poll::Lost => coalescer.push(
                RawChange {
                    action: RawAction::Overflow,
                    path: root.to_path_buf(),
                },
                stamp,
            ),
        }
        let ready = if finished {
            coalescer.take_all()
        } else {
            coalescer.take_ready(stamp)
        };
        if !ready.is_empty() {
            emit(ready);
        }
        if finished {
            return;
        }
    }
}

/// Events must reach the reader thread's owner; Win32 handles are plain kernel object ids.
struct SendBox<T>(T);

// SAFETY: the wrapped Win32 handles refer to kernel objects that may be used from any thread;
// the wrapper only moves them to the reader thread, which becomes their sole user.
unsafe impl<T> Send for SendBox<T> {}

impl<T> SendBox<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

struct StopEvent(Owned<HANDLE>);

// SAFETY: an event handle may be signalled and waited on from any thread.
unsafe impl Send for StopEvent {}
// SAFETY: see above; `SetEvent`/`WaitForMultipleObjects` are thread-safe.
unsafe impl Sync for StopEvent {}

fn create_event(path: &Path) -> Result<Owned<HANDLE>, FsError> {
    // SAFETY: no security attributes, manual-reset, initially non-signalled, unnamed.
    let handle = win(
        unsafe { CreateEventW(None, true, false, PCWSTR::null()) },
        path,
    )?;
    // SAFETY: `handle` is a freshly created, valid handle that nothing else owns.
    Ok(unsafe { Owned::new(handle) })
}

/// Overlapped `ReadDirectoryChangesW` on one directory handle.
struct RdcwSource {
    root: PathBuf,
    dir: Owned<HANDLE>,
    io_event: Owned<HANDLE>,
    stop: Arc<StopEvent>,
    overlapped: Box<OVERLAPPED>,
    buffer: Vec<u32>,
    pending: bool,
}

impl RdcwSource {
    fn issue(&mut self) -> windows::core::Result<()> {
        let filter = FILE_NOTIFY_CHANGE_FILE_NAME
            | FILE_NOTIFY_CHANGE_DIR_NAME
            | FILE_NOTIFY_CHANGE_ATTRIBUTES
            | FILE_NOTIFY_CHANGE_SIZE
            | FILE_NOTIFY_CHANGE_LAST_WRITE
            | FILE_NOTIFY_CHANGE_CREATION;
        // SAFETY: `buffer` and `overlapped` are heap allocations owned by `self`; they stay at
        // a fixed address until the request completes or `Drop` cancels it and waits for the
        // cancellation. The directory handle was opened with FILE_FLAG_OVERLAPPED.
        unsafe {
            ReadDirectoryChangesW(
                *self.dir,
                self.buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
                true,
                filter,
                None,
                Some(&raw mut *self.overlapped),
                None,
            )
        }?;
        self.pending = true;
        Ok(())
    }

    fn poll_inner(&mut self, timeout: Option<Duration>) -> windows::core::Result<Poll> {
        if !self.pending {
            self.issue()?;
        }
        let millis = timeout.map_or(INFINITE, |wait| {
            u32::try_from(wait.as_micros().div_ceil(1000)).unwrap_or(INFINITE - 1)
        });
        let handles = [*self.io_event, *self.stop.0];
        // SAFETY: both handles are valid events owned by this source and the stop event is
        // kept alive by the `Arc`.
        let woke = unsafe { WaitForMultipleObjects(&handles, false, millis) };
        match woke.0.wrapping_sub(WAIT_OBJECT_0.0) {
            0 => {
                self.pending = false;
                let mut transferred = 0u32;
                // SAFETY: the request was issued on `dir` with this `overlapped` and has
                // completed (its event is signalled), so the call does not block.
                unsafe {
                    GetOverlappedResult(
                        *self.dir,
                        &raw const *self.overlapped,
                        &raw mut transferred,
                        false,
                    )
                }?;
                // SAFETY: the kernel wrote `transferred` bytes into `buffer`, which is at least
                // that long and `u8` has no alignment requirement.
                let bytes = unsafe {
                    std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), BUFFER_BYTES)
                };
                Ok(Poll::Changes(parse_notifications(
                    bytes,
                    transferred as usize,
                    &self.root,
                )))
            }
            1 => Ok(Poll::Stop),
            // Only WAIT_TIMEOUT is possible with two valid handles.
            _ => Ok(Poll::Timeout),
        }
    }
}

impl ChangeSource for RdcwSource {
    fn poll(&mut self, timeout: Option<Duration>) -> Poll {
        match self.poll_inner(timeout) {
            Ok(polled) => polled,
            Err(_) => Poll::Lost,
        }
    }
}

impl Drop for RdcwSource {
    fn drop(&mut self) {
        if self.pending {
            let mut transferred = 0u32;
            // SAFETY: cancels the one outstanding request on `dir` and waits until the kernel
            // is done with `buffer` and `overlapped`; failures mean it already completed.
            unsafe {
                let _ = CancelIoEx(*self.dir, Some(&raw const *self.overlapped));
                let _ = GetOverlappedResult(
                    *self.dir,
                    &raw const *self.overlapped,
                    &raw mut transferred,
                    true,
                );
            }
        }
    }
}

/// Watches a folder tree recursively. Dropping the watcher stops the reader thread.
#[derive(Debug)]
pub struct Watcher {
    root: PathBuf,
    stop: Arc<StopEvent>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for StopEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StopEvent")
    }
}

impl Watcher {
    /// Starts watching `root` recursively. Coalesced events arrive on the returned channel;
    /// `wake` runs after each delivered batch (e.g. to post a window message). When the watch
    /// breaks the channel receives [`FsEventKind::Rescan`] and then disconnects.
    ///
    /// # Errors
    /// [`FsError::NotFound`] when `root` is not a folder, other [`FsError`]s from the OS.
    pub fn start(
        root: &Path,
        wake: impl Fn() + Send + 'static,
    ) -> Result<(Self, Receiver<FsEvent>), FsError> {
        let name = wide(root);
        // SAFETY: `name` is NUL-terminated and outlives the call; the handle is opened for
        // overlapped directory listing with full sharing so the folder can still be renamed.
        let dir = win(
            unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    FILE_LIST_DIRECTORY.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    None,
                )
            },
            root,
        )?;
        // SAFETY: `dir` is a freshly opened handle that nothing else owns.
        let dir = unsafe { Owned::new(dir) };
        let stop = Arc::new(StopEvent(create_event(root)?));
        let mut source = RdcwSource {
            root: root.to_path_buf(),
            dir,
            io_event: create_event(root)?,
            stop: Arc::clone(&stop),
            overlapped: Box::default(),
            buffer: vec![0u32; BUFFER_BYTES / 4],
            pending: false,
        };
        source.overlapped.hEvent = *source.io_event;
        let (sender, receiver) = channel();
        let (armed_sender, armed) = channel::<()>();
        let owned_root = root.to_path_buf();
        let moved = SendBox(source);
        let thread = io(
            std::thread::Builder::new()
                .name("ssw-watch".to_owned())
                .spawn(move || {
                    let mut source = moved.into_inner();
                    // The first request must be pending before `start` returns, or changes made
                    // right after it would be missed. It is issued on this thread because
                    // overlapped requests are cancelled when their issuing thread ends. A
                    // failure here is not reported separately: `pump` retries the request on its
                    // first poll and ends with a rescan when that fails as well.
                    let _ = source.issue();
                    let _ = armed_sender.send(());
                    pump(&mut source, &owned_root, &mut Instant::now, &mut |events| {
                        for event in events {
                            let _ = sender.send(event);
                        }
                        wake();
                    });
                }),
            root,
        )?;
        // Blocks until the reader thread has issued the first request. A disconnect means the
        // thread died first; the receiver then simply never yields events.
        let _ = armed.recv();
        Ok((
            Self {
                root: root.to_path_buf(),
                stop,
                thread: Some(thread),
            },
            receiver,
        ))
    }

    /// The watched root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: the stop event is a valid handle kept alive by the `Arc`. If signalling
        // fails the reader already ended (watch lost) and `join` returns immediately.
        let _ = unsafe { SetEvent(*self.stop.0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What happened to an open document on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocChange {
    /// Content identical to what this process wrote or loaded.
    Unchanged,
    /// Modified by someone else.
    Changed,
    /// Deleted.
    Deleted,
    /// Moved or renamed to `to`.
    Renamed {
        /// New path.
        to: PathBuf,
    },
}

/// Tracks the last known stamp of each open document and decides whether a watcher event is an
/// external change: own writes (same size and mtime, or same content hash) are suppressed.
#[derive(Debug)]
pub struct ExternalChanges {
    fs: Arc<dyn FileSystem>,
    docs: HashMap<String, (PathBuf, SaveStamp)>,
}

impl ExternalChanges {
    /// A tracker that inspects the disk through `fs`.
    #[must_use]
    pub fn new(fs: Arc<dyn FileSystem>) -> Self {
        Self {
            fs,
            docs: HashMap::new(),
        }
    }

    /// Starts (or updates) tracking `path` with the stamp of its current content. Call after
    /// loading the file and after every own save.
    pub fn track(&mut self, path: &Path, stamp: SaveStamp) {
        self.docs
            .insert(path_key(path), (path.to_path_buf(), stamp));
    }

    /// Stops tracking `path`.
    pub fn untrack(&mut self, path: &Path) {
        self.docs.remove(&path_key(path));
    }

    /// The stamp stored for `path`.
    #[must_use]
    pub fn stamp(&self, path: &Path) -> Option<SaveStamp> {
        self.docs.get(&path_key(path)).map(|(_, stamp)| *stamp)
    }

    /// Compares the file on disk with the stored stamp. Untracked paths are `Unchanged`.
    #[must_use]
    pub fn check(&self, path: &Path) -> DocChange {
        let Some((_, stamp)) = self.docs.get(&path_key(path)) else {
            return DocChange::Unchanged;
        };
        match self.fs.metadata(path) {
            Err(FsError::NotFound(_)) => DocChange::Deleted,
            // Cannot verify: assume someone else changed it.
            Err(_) => DocChange::Changed,
            Ok(meta) if stamp.matches_meta(&meta) => DocChange::Unchanged,
            Ok(meta) if meta.size == stamp.size => match self.fs.read(path) {
                Ok(content) if stamp.matches_content(&content) => DocChange::Unchanged,
                _ => DocChange::Changed,
            },
            Ok(_) => DocChange::Changed,
        }
    }

    /// Classifies watcher events for the tracked documents. The result holds one entry per
    /// document (the last decision wins) and never contains [`DocChange::Unchanged`].
    #[must_use]
    pub fn classify(&self, events: &[FsEvent]) -> Vec<(PathBuf, DocChange)> {
        let mut decisions: Vec<(String, PathBuf, DocChange)> = Vec::new();
        let mut decide = |path: &Path, change: DocChange| {
            let key = path_key(path);
            decisions.retain(|(existing, _, _)| *existing != key);
            decisions.push((key, path.to_path_buf(), change));
        };
        for event in events {
            match &event.kind {
                FsEventKind::Rescan => {
                    for (path, _) in self.docs.values() {
                        decide(path, self.check(path));
                    }
                }
                FsEventKind::Renamed { from } if self.docs.contains_key(&path_key(from)) => {
                    decide(
                        from,
                        DocChange::Renamed {
                            to: event.path.clone(),
                        },
                    );
                }
                _ => decide(&event.path, self.check(&event.path)),
            }
        }
        decisions
            .into_iter()
            .filter(|(_, _, change)| *change != DocChange::Unchanged)
            .map(|(_, path, change)| (path, change))
            .collect()
    }
}
