//! Duplicate detection, cheapest filter first.
//!
//! 1. exact `logical` size, singletons discarded
//! 2. xxh3 of head 4 KiB + tail 4 KiB + size, singletons discarded
//! 3. xxh3 of the whole file, mmapped, in parallel
//! 4. blake3 **only** to break an xxh3 collision
//!
//! xxh3 runs at roughly 26x blake3 single-threaded on Apple Silicon, so blake3
//! is a tiebreaker rather than the primary digest. blake3's headline numbers
//! are AVX-512 on x86; `fclones` picks MetroHash-128 for the same reason.
//!
//! **The rule that makes this honest.** Clones and hardlinks already share
//! their blocks, so listing them as "duplicates to delete" is a lie: deleting
//! one frees nothing. Members are partitioned into share classes by
//! `clone_id`, else `(dev, ino)`; a class occupies one physical copy no matter
//! how many names point at it. Reclaimable bytes are therefore
//! `(distinct classes - 1) * size`, never `(members - 1) * size`.

use crate::{Flags, NodeId, Tree};
use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

/// Files below this are not worth the syscalls, and a filesystem full of tiny
/// identical files is usually a package manager's business, not ours.
pub const MIN_SIZE: u64 = 4096;
const EDGE: usize = 4096;

#[derive(Clone, Debug)]
pub struct Member {
    pub id: NodeId,
    pub path: PathBuf,
    /// True when this file already shares its blocks with another member of
    /// the same group, so removing it would free nothing.
    pub shared: bool,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub bytes_each: u64,
    pub members: Vec<Member>,
    /// Bytes actually returned to the filesystem by keeping one copy.
    pub reclaimable: u64,
    /// Physically distinct copies behind those members.
    pub copies: usize,
}

fn abs_of(root: &Path, t: &Tree, id: NodeId) -> Option<PathBuf> {
    let base = root.parent()?;
    let p = base.join(t.path(id));
    p.starts_with(root).then_some(p)
}

/// Identity of the physical extent behind a file: an APFS clone id when there
/// is one, otherwise the inode.
fn share_class(t: &Tree, id: NodeId) -> (u64, u64) {
    let i = id as usize;
    if t.clone_id[i] != 0 {
        (u64::MAX, t.clone_id[i])
    } else {
        (t.dev[i], t.ino[i])
    }
}

fn regroup<K>(groups: Vec<Vec<NodeId>>, key: impl Fn(NodeId) -> Option<K>) -> Vec<Vec<NodeId>>
where
    K: std::hash::Hash + Eq,
{
    let mut out = Vec::new();
    for g in groups {
        let mut by: HashMap<K, Vec<NodeId>> = HashMap::new();
        for id in g {
            if let Some(k) = key(id) {
                by.entry(k).or_default().push(id);
            }
        }
        out.extend(by.into_values().filter(|v| v.len() > 1));
    }
    out
}

fn head_tail_hash(path: &Path, size: u64) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = File::open(path).ok()?;
    let n = EDGE.min(size as usize);
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf).ok()?;
    let mut h = xxh3_64(&buf) ^ size;
    if size > EDGE as u64 * 2 {
        f.seek(SeekFrom::End(-(EDGE as i64))).ok()?;
        let mut tail = vec![0u8; EDGE];
        f.read_exact(&mut tail).ok()?;
        h ^= xxh3_64(&tail).rotate_left(17);
    }
    Some(h)
}

fn map(path: &Path) -> Option<Mmap> {
    let f = File::open(path).ok()?;
    // SAFETY: a file changing under the map yields a wrong hash rather than
    // unsoundness for this read-only use; callers re-verify before acting.
    unsafe { Mmap::map(&f).ok() }
}

fn full_xxh3(path: &Path) -> Option<u64> {
    map(path).map(|m| xxh3_64(&m))
}

fn full_blake3(path: &Path) -> Option<[u8; 32]> {
    map(path).map(|m| *blake3::hash(&m).as_bytes())
}

/// Groups of byte-identical files, ordered by what removing the extras would
/// actually free.
pub fn find(t: &Tree, root: &Path, min_size: u64) -> Vec<Group> {
    let root = crate::blocklist::canon_keep_link(root);
    let min_size = min_size.max(MIN_SIZE);

    // Tier 1: exact logical size.
    let mut by_size: HashMap<u64, Vec<NodeId>> = HashMap::new();
    for id in 0..t.len() as NodeId {
        let i = id as usize;
        if t.flags[i].is_dir() || t.flags[i].has(Flags::SYMLINK) || t.flags[i].has(Flags::DENIED) {
            continue;
        }
        let size = t.logical[i];
        if size < min_size {
            continue;
        }
        by_size.entry(size).or_default().push(id);
    }
    let groups: Vec<Vec<NodeId>> = by_size.into_values().filter(|v| v.len() > 1).collect();
    if groups.is_empty() {
        return Vec::new();
    }

    // Paths are resolved once and reused by every later tier.
    let paths: HashMap<NodeId, PathBuf> = groups
        .iter()
        .flatten()
        .filter_map(|&id| abs_of(&root, t, id).map(|p| (id, p)))
        .collect();
    let path_of = |id: NodeId| paths.get(&id).cloned();

    // Tier 2: head + tail + size.
    let groups = regroup(groups, |id| {
        let p = path_of(id)?;
        head_tail_hash(&p, t.logical[id as usize])
    });
    if groups.is_empty() {
        return Vec::new();
    }

    // Tier 3: full xxh3, in parallel. Precomputed rather than hashed inside the
    // grouping closure so rayon sees the whole workload at once.
    let ids: Vec<NodeId> = groups.iter().flatten().copied().collect();
    let full: HashMap<NodeId, u64> = ids
        .par_iter()
        .filter_map(|&id| paths.get(&id).and_then(|p| full_xxh3(p)).map(|h| (id, h)))
        .collect();
    let groups = regroup(groups, |id| full.get(&id).copied());
    if groups.is_empty() {
        return Vec::new();
    }

    // Tier 4: blake3, only over what xxh3 already called equal.
    let ids: Vec<NodeId> = groups.iter().flatten().copied().collect();
    let strong: HashMap<NodeId, [u8; 32]> = ids
        .par_iter()
        .filter_map(|&id| paths.get(&id).and_then(|p| full_blake3(p)).map(|h| (id, h)))
        .collect();
    let groups = regroup(groups, |id| strong.get(&id).copied());

    let mut out: Vec<Group> = groups
        .into_iter()
        .filter_map(|g| {
            let size = t.logical[g[0] as usize];
            // Distinct physical copies, not distinct names.
            let mut classes: HashMap<(u64, u64), usize> = HashMap::new();
            for &id in &g {
                *classes.entry(share_class(t, id)).or_default() += 1;
            }
            let copies = classes.len();
            let members: Vec<Member> = g
                .iter()
                .filter_map(|&id| {
                    Some(Member {
                        id,
                        path: path_of(id)?,
                        shared: classes.get(&share_class(t, id)).copied().unwrap_or(0) > 1,
                    })
                })
                .collect();
            if members.len() < 2 {
                return None;
            }
            let reclaimable = (copies as u64).saturating_sub(1) * size;
            Some(Group { bytes_each: size, members, reclaimable, copies })
        })
        .collect();

    debug_assert!(
        out.iter().all(|g| g.copies <= g.members.len()),
        "a group cannot hold more physical copies than members"
    );
    out.sort_by_key(|g| std::cmp::Reverse((g.reclaimable, g.bytes_each)));
    out
}

/// What the whole run would return to the filesystem.
pub fn total_reclaimable(groups: &[Group]) -> u64 {
    groups.iter().map(|g| g.reclaimable).sum()
}
