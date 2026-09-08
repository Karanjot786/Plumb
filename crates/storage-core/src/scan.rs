use crate::{Flags, NodeId, Row, Tree, NO_PARENT};
use dua_core::{Entry, Options, Order};
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// The one place platform differences live. dua-core's Metadata exposes
/// different methods per OS, so normalise here and nowhere else.
#[derive(Clone, Copy)]
struct Meta {
    logical: u64,
    blocks: u64,
    mtime: u32,
    dev: u64,
    ino: u64,
    /// 0 means "link count unknown" (Windows), not "zero links".
    nlink: u32,
    clone_id: u64,
}

const EMPTY: Meta = Meta { logical: 0, blocks: 0, mtime: 0, dev: 0, ino: 0, nlink: 1, clone_id: 0 };

fn mtime_secs(t: Option<SystemTime>) -> u32 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
}

#[cfg(target_os = "macos")]
fn meta_of(m: &dua_core::Metadata) -> Meta {
    Meta {
        logical: m.len(),
        blocks: m.blocks() * 512,
        mtime: mtime_secs(m.modified().ok()),
        dev: m.dev(),
        ino: m.ino(),
        nlink: m.nlink() as u32,
        clone_id: m.clone_id().map_or(0, |c| c.get()),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn meta_of(m: &dua_core::Metadata) -> Meta {
    use std::os::unix::fs::MetadataExt;
    Meta {
        logical: m.len(),
        blocks: m.blocks() * 512,
        mtime: mtime_secs(m.modified().ok()),
        dev: m.dev(),
        ino: m.ino(),
        nlink: m.nlink() as u32,
        clone_id: 0,
    }
}

// Windows has no nlink, but it does have identity: hard_link_id() returns
// (volume_serial, file_id), free from FileIdBothDirectoryInfo enumeration.
// nlink = 0 means "unknown", which aggregate() reads as "maybe shared".
#[cfg(windows)]
fn meta_of(m: &dua_core::Metadata) -> Meta {
    let (dev, ino) = m.hard_link_id().unwrap_or((0, 0));
    Meta {
        logical: m.len(),
        blocks: m.allocated_size(),
        mtime: mtime_secs(m.modified().ok()),
        dev,
        ino,
        nlink: 0,
        clone_id: 0,
    }
}

struct Raw {
    name: String,
    meta: Meta,
    is_dir: bool,
    is_link: bool,
    denied: bool,
    dir_idx: Option<usize>,
}

enum Step { Visit(usize, NodeId), Close(NodeId) }

/// dua-core's Order::ParentFirst guarantees only that a parent precedes its
/// descendants; its own docs say "sibling order is unspecified in both modes".
/// So collect, then emit in explicit DFS order to get contiguous subtrees.
pub fn scan(root: &Path, threads: usize, mut progress: impl FnMut(u64) + Send) -> io::Result<Tree> {
    let opts = Options {
        skip_metadata: false,
        #[cfg(target_os = "macos")]
        apfs_clone_metadata: true,
    };

    let mut raw: Vec<Raw> = Vec::new();
    let mut children_of: Vec<Vec<usize>> = Vec::new();
    let mut root_raw: Option<usize> = None;

    for item in dua_core::walk(root, threads, Order::ParentFirst, opts, |_| true) {
        let e: Entry = match item { Ok(e) => e, Err(_) => continue };
        let (meta, denied) = match &e.metadata {
            Some(Ok(m)) => (meta_of(m), false),
            Some(Err(_)) => (EMPTY, true),
            None => (EMPTY, false),
        };
        let idx = raw.len();
        // Every 4096 entries, not every entry: the callback crosses an IPC
        // boundary in the UI and would otherwise cost more than the walk.
        if idx % 4096 == 0 {
            progress(idx as u64);
        }
        let dir_idx = e.directory_id.map(|d| d.index());
        if let Some(d) = dir_idx {
            if children_of.len() <= d { children_of.resize(d + 1, Vec::new()); }
        }
        match e.parent_directory_id.map(|d| d.index()) {
            Some(p) => {
                if children_of.len() <= p { children_of.resize(p + 1, Vec::new()); }
                children_of[p].push(idx);
            }
            None => root_raw = Some(idx),
        }
        raw.push(Raw {
            name: e.file_name.to_string_lossy().into_owned(),
            meta,
            is_dir: e.file_type.is_dir(),
            is_link: e.file_type.is_symlink(),
            denied,
            dir_idx,
        });
    }

    // Canonical sibling order. A snapshot diff joins on tree position and
    // breaks ties on the sibling ordinal, so the emit order has to be a pure
    // function of the names rather than of how the walker happened to schedule
    // its threads. Raw bytes, not a locale collation: the comparison must mean
    // the same thing on every machine that reads the snapshot.
    for kids in &mut children_of {
        kids.sort_by(|&a, &b| raw[a].name.as_bytes().cmp(raw[b].name.as_bytes()));
    }

    let mut tree = Tree::default();
    let Some(start) = root_raw else { return Ok(tree) };

    let mut stack = vec![Step::Visit(start, NO_PARENT)];
    while let Some(step) = stack.pop() {
        match step {
            Step::Close(id) => {
                tree.subtree_len[id as usize] = tree.len() as u32 - id;
            }
            Step::Visit(ri, parent) => {
                let r = &raw[ri];
                let mut f = 0u8;
                if r.is_dir { f |= Flags::DIR; }
                if r.is_link { f |= Flags::SYMLINK; }
                if r.denied { f |= Flags::DENIED; }
                let id = tree.push(Row {
                    name: &r.name,
                    parent,
                    logical: r.meta.logical,
                    blocks: r.meta.blocks,
                    mtime: r.meta.mtime,
                    flags: Flags(f),
                    dev: r.meta.dev,
                    ino: r.meta.ino,
                    nlink: r.meta.nlink,
                    clone_id: r.meta.clone_id,
                });
                stack.push(Step::Close(id));
                if let Some(d) = r.dir_idx {
                    if let Some(kids) = children_of.get(d) {
                        for &k in kids.iter().rev() { stack.push(Step::Visit(k, id)); }
                    }
                }
            }
        }
    }
    tree.check();
    check_sibling_order(&tree);
    Ok(tree)
}

/// Siblings must be emitted in raw-name-byte order. `dua-cli` enforces the same
/// invariant on read and refuses a snapshot that violates it; we assert it on
/// write so a bad tree never reaches a snapshot in the first place.
fn check_sibling_order(t: &Tree) {
    #[cfg(debug_assertions)]
    for i in 0..t.len() as NodeId {
        let mut prev: Option<&str> = None;
        for c in t.children(i) {
            let name = t.name(c);
            if let Some(p) = prev {
                debug_assert!(
                    p.as_bytes() <= name.as_bytes(),
                    "siblings out of order under node {i}: {p:?} precedes {name:?}"
                );
            }
            prev = Some(name);
        }
    }
    let _ = t;
}
