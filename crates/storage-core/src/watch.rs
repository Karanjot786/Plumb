//! Live filesystem monitoring.
//!
//! Three platforms, two strategies, one trait.
//!
//! macOS gets FSEvents and Windows gets `ReadDirectoryChangesW`, both through
//! `notify`. **Linux deliberately does not.** `notify`'s Linux backend is
//! inotify, which needs one watch per directory; a 240k-directory tree blows
//! straight past `max_user_watches` and the watcher fails in a way the user
//! cannot fix without editing a sysctl. Linux therefore polls and diffs, which
//! is a decent answer rather than a consolation: a scan is fast and `diff` is a
//! single lockstep walk, so a 30-second poll is cheap and exactly correct.
//! `fanotify` with `FAN_REPORT_FID` is the real answer and is not attempted
//! here.
//!
//! # Coalescing is the whole design
//!
//! A build or an installer emits thousands of events a second. Rendering one
//! row per event is both useless and slow, so `events()` returns at most one
//! entry per path per call, with the byte movements summed. The caller renders
//! once per drain, not once per event.
//!
//! Nothing in this module removes anything.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventKind {
    Created,
    Modified,
    Removed,
}

impl EventKind {
    pub fn label(self) -> &'static str {
        match self {
            EventKind::Created => "created",
            EventKind::Modified => "modified",
            EventKind::Removed => "removed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct FsEvent {
    pub path: PathBuf,
    pub kind: EventKind,
    /// Signed byte movement since we last saw this path. Best effort: a path
    /// we have never seen before contributes its whole size.
    pub bytes: i64,
    pub at: u64,
}

pub trait Watcher {
    fn events(&mut self) -> io::Result<Vec<FsEvent>>;
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn size_of(p: &Path) -> Option<u64> {
    let m = std::fs::symlink_metadata(p).ok()?;
    if m.is_dir() {
        // A directory's own entry size says nothing useful, and walking it on
        // every event would be the exact per-event cost this module exists to
        // avoid. The tree rescan reports the real number.
        return Some(0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(m.blocks() * 512)
    }
    #[cfg(not(unix))]
    {
        Some(m.len())
    }
}

/// Last known size per path, so an event can carry a delta rather than an
/// absolute. Bounded, because a long-running monitor over a busy tree would
/// otherwise grow without limit.
struct Sizes {
    /// path -> (last seen size, sequence number of that sighting)
    seen: HashMap<PathBuf, (u64, u64)>,
    cap: usize,
    tick: u64,
}

impl Sizes {
    fn new(cap: usize) -> Sizes {
        Sizes { seen: HashMap::new(), cap, tick: 0 }
    }

    /// Drop the coldest half once the cap is reached.
    ///
    /// Clearing the whole map was cheaper but made the next event for *every*
    /// path report its full size as the delta rather than the change. Halving
    /// keeps the hot paths — which is what a monitor is watching — and pays an
    /// O(n) sort only once per `cap/2` insertions.
    fn evict(&mut self) {
        let mut seqs: Vec<u64> = self.seen.values().map(|&(_, s)| s).collect();
        let keep = seqs.len() / 2;
        if keep == 0 {
            self.seen.clear();
            return;
        }
        seqs.sort_unstable();
        let cutoff = seqs[seqs.len() - keep];
        self.seen.retain(|_, &mut (_, s)| s >= cutoff);
    }

    /// Delta for `path`, and the kind the delta implies.
    fn delta(&mut self, path: &Path) -> (EventKind, i64) {
        if self.seen.len() >= self.cap {
            self.evict();
            debug_assert!(
                self.seen.len() < self.cap,
                "eviction left the map at or above its cap"
            );
        }
        self.tick += 1;
        let tick = self.tick;
        match size_of(path) {
            None => {
                let old = self.seen.remove(path).map(|(b, _)| b).unwrap_or(0);
                (EventKind::Removed, -(old as i64))
            }
            Some(new) => {
                let prev = self.seen.insert(path.to_path_buf(), (new, tick));
                match prev {
                    None => (EventKind::Created, new as i64),
                    Some((old, _)) => (EventKind::Modified, new as i64 - old as i64),
                }
            }
        }
    }
}

/// Fold a burst down to one entry per path.
fn coalesce(raw: Vec<FsEvent>) -> Vec<FsEvent> {
    let mut order: Vec<PathBuf> = Vec::new();
    let mut by_path: HashMap<PathBuf, FsEvent> = HashMap::new();
    for e in raw {
        match by_path.get_mut(&e.path) {
            Some(acc) => {
                acc.bytes += e.bytes;
                acc.at = e.at;
                // A removal is the last word; a creation followed by writes is
                // still a creation.
                acc.kind = match (acc.kind, e.kind) {
                    (_, EventKind::Removed) => EventKind::Removed,
                    (EventKind::Created, _) => EventKind::Created,
                    (_, k) => k,
                };
            }
            None => {
                order.push(e.path.clone());
                by_path.insert(e.path.clone(), e);
            }
        }
    }
    order.into_iter().filter_map(|p| by_path.remove(&p)).collect()
}

// ------------------------------------------------------------- native

/// FSEvents on macOS, `ReadDirectoryChangesW` on Windows.
#[cfg(any(target_os = "macos", windows))]
pub struct Native {
    rx: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    sizes: Sizes,
    // Held for its lifetime: dropping the backend stops the stream.
    _backend: notify::RecommendedWatcher,
}

#[cfg(any(target_os = "macos", windows))]
impl Native {
    /// At most this many raw events are drained per call. A burst larger than
    /// this is not lost, only deferred to the next drain, which keeps one call
    /// bounded no matter what the filesystem is doing.
    const DRAIN_CAP: usize = 20_000;

    pub fn new(root: &Path) -> io::Result<Native> {
        use notify::Watcher as _;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut backend = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .map_err(io::Error::other)?;
        backend.watch(root, notify::RecursiveMode::Recursive).map_err(io::Error::other)?;
        Ok(Native { rx, sizes: Sizes::new(200_000), _backend: backend })
    }
}

#[cfg(any(target_os = "macos", windows))]
impl Watcher for Native {
    fn events(&mut self) -> io::Result<Vec<FsEvent>> {
        let at = now();
        let mut raw = Vec::new();
        for _ in 0..Self::DRAIN_CAP {
            match self.rx.try_recv() {
                Ok(Ok(ev)) => {
                    for path in ev.paths {
                        let (kind, bytes) = self.sizes.delta(&path);
                        raw.push(FsEvent { path, kind, bytes, at });
                    }
                }
                // A dropped or failed event is not worth killing the monitor
                // over; the periodic rescan reconciles anything missed.
                Ok(Err(_)) => continue,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::other("watcher backend stopped"))
                }
            }
        }
        Ok(coalesce(raw))
    }
}

// --------------------------------------------------------------- polling

/// Periodic rescan and diff. The Linux path, and the fallback anywhere the
/// native backend will not start.
pub struct Poll {
    root: PathBuf,
    threads: usize,
    interval: Duration,
    last: Instant,
    tree: Option<crate::Tree>,
}

impl Poll {
    pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

    pub fn new(root: &Path, threads: usize, interval: Duration) -> io::Result<Poll> {
        let mut p = Poll {
            root: root.to_path_buf(),
            threads: threads.max(1),
            interval,
            // Force the first `events()` call to do nothing but establish a
            // baseline, rather than reporting the whole tree as new.
            last: Instant::now(),
            tree: None,
        };
        p.tree = Some(p.scan()?);
        Ok(p)
    }

    /// `diff` reads `sub_blocks`, which a bare scan leaves empty, so the
    /// aggregate is part of taking a baseline rather than an optional extra.
    fn scan(&self) -> io::Result<crate::Tree> {
        let mut t = crate::scan(&self.root, self.threads, |_| {})?;
        crate::aggregate(&mut t);
        debug_assert_eq!(t.sub_blocks.len(), t.len(), "poll baseline was not aggregated");
        Ok(t)
    }

    /// Seconds until the next rescan is due, for a UI that wants to say so.
    pub fn due_in(&self) -> Duration {
        self.interval.saturating_sub(self.last.elapsed())
    }
}

impl Watcher for Poll {
    fn events(&mut self) -> io::Result<Vec<FsEvent>> {
        if self.last.elapsed() < self.interval {
            return Ok(Vec::new());
        }
        self.last = Instant::now();

        // A root that is momentarily unreadable scans to just itself - every
        // child vanishes - so diff reports them all Removed, the stripped tree
        // replaces the baseline, and the next poll reports them all Added. Two
        // storms for a tree that never changed.
        //
        // The shape cannot be used to detect this: a genuine `rm -rf` of the
        // contents leaves the same root-only tree and *should* report. So ask
        // the filesystem whether the root is readable, and if it is not, keep
        // the baseline and say nothing. A failed read is not a deletion.
        if std::fs::read_dir(&self.root).is_err() {
            return Ok(Vec::new());
        }

        let new = self.scan()?;
        let at = now();
        let base = self.root.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let out = match self.tree.take() {
            None => Vec::new(),
            Some(old) => crate::diff::diff(&old, &new)
                .into_iter()
                .map(|c| FsEvent {
                    path: base.join(&c.path),
                    kind: match c.kind {
                        crate::diff::Kind::Added => EventKind::Created,
                        crate::diff::Kind::Removed => EventKind::Removed,
                        _ => EventKind::Modified,
                    },
                    bytes: c.delta().clamp(i64::MIN as i128, i64::MAX as i128) as i64,
                    at,
                })
                .collect(),
        };
        self.tree = Some(new);
        Ok(coalesce(out))
    }
}

/// The right watcher for this machine: native where it exists and starts,
/// polling everywhere else.
pub fn watcher(root: &Path, threads: usize) -> io::Result<Box<dyn Watcher + Send>> {
    #[cfg(any(target_os = "macos", windows))]
    if let Ok(n) = Native::new(root) {
        return Ok(Box::new(n));
    }
    Ok(Box::new(Poll::new(root, threads, Poll::DEFAULT_INTERVAL)?))
}

// ------------------------------------------------------------------ feed

/// The rolling list a UI renders, plus the set of directories whose sizes are
/// now wrong.
///
/// Marking stale is the point: a rescan per event would cost more than the
/// events are worth, so the monitor records *what* to re-measure and leaves
/// *when* to the caller.
pub struct Feed {
    events: VecDeque<FsEvent>,
    stale: HashSet<PathBuf>,
    pub cap: usize,
    /// While paused the feed still drains its watcher, so a burst does not
    /// back up in the channel; it simply keeps nothing.
    pub paused: bool,
    pub seen: u64,
}

impl Feed {
    pub fn new(cap: usize) -> Feed {
        Feed { events: VecDeque::new(), stale: HashSet::new(), cap, paused: false, seen: 0 }
    }

    pub fn absorb(&mut self, batch: Vec<FsEvent>) {
        self.seen += batch.len() as u64;
        if self.paused {
            return;
        }
        for e in batch {
            // The containing directory is what needs re-measuring, not the
            // file, and its ancestors follow from it.
            if let Some(parent) = e.path.parent() {
                self.stale.insert(parent.to_path_buf());
            }
            self.events.push_front(e);
        }
        while self.events.len() > self.cap {
            self.events.pop_back();
        }
        debug_assert!(self.events.len() <= self.cap, "feed grew past its cap");
    }

    /// Newest first.
    pub fn events(&self) -> impl Iterator<Item = &FsEvent> {
        self.events.iter()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn stale(&self) -> &HashSet<PathBuf> {
        &self.stale
    }

    /// Call after re-measuring.
    pub fn clear_stale(&mut self) {
        self.stale.clear();
    }

    pub fn clear(&mut self) {
        self.events.clear();
        self.stale.clear();
    }
}
