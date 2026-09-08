use std::ops::Range;

pub type NodeId = u32;
pub const NO_PARENT: NodeId = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags(pub u8);

impl Flags {
    pub const DIR: u8 = 1 << 0;
    pub const SYMLINK: u8 = 1 << 1;
    pub const DENIED: u8 = 1 << 2;
    pub const SHARED: u8 = 1 << 3;
    pub fn has(self, bit: u8) -> bool { self.0 & bit != 0 }
    pub fn is_dir(self) -> bool { self.has(Self::DIR) }
}

/// DFS pre-order columnar arena. The subtree of node `i` is the contiguous
/// range `[i, i + subtree_len[i])`, which makes aggregation one reverse loop
/// and "all descendants" a slice.
///
/// Every field is a plain integer on purpose: it is what makes the stage-5
/// snapshot a zero-copy read. Do not add String or enum-with-payload here.
#[derive(Default)]
pub struct Tree {
    pub parent: Vec<NodeId>,
    pub subtree_len: Vec<u32>,
    pub name_off: Vec<u32>,
    pub name_len: Vec<u16>,
    pub logical: Vec<u64>,
    pub blocks: Vec<u64>,
    pub mtime: Vec<u32>,
    pub flags: Vec<Flags>,
    pub dev: Vec<u64>,
    pub ino: Vec<u64>,
    pub nlink: Vec<u32>,
    pub clone_id: Vec<u64>,
    pub names: Vec<u8>,

    pub sub_logical: Vec<u64>,
    pub sub_blocks: Vec<u64>,
    pub sub_excl: Vec<u64>,
    pub sub_files: Vec<u32>,
    pub sub_dirs: Vec<u32>,
}

pub struct Row<'a> {
    pub name: &'a str,
    pub parent: NodeId,
    pub logical: u64,
    pub blocks: u64,
    pub mtime: u32,
    pub flags: Flags,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u32,
    pub clone_id: u64,
}

impl Tree {
    pub fn len(&self) -> usize { self.parent.len() }
    pub fn is_empty(&self) -> bool { self.parent.is_empty() }

    pub fn push(&mut self, r: Row<'_>) -> NodeId {
        let id = self.parent.len() as NodeId;
        self.name_off.push(self.names.len() as u32);
        self.name_len.push(r.name.len().min(u16::MAX as usize) as u16);
        self.names.extend_from_slice(r.name.as_bytes());
        self.parent.push(r.parent);
        self.subtree_len.push(1);
        self.logical.push(r.logical);
        self.blocks.push(r.blocks);
        self.mtime.push(r.mtime);
        self.flags.push(r.flags);
        self.dev.push(r.dev);
        self.ino.push(r.ino);
        self.nlink.push(r.nlink);
        self.clone_id.push(r.clone_id);
        id
    }

    pub fn name(&self, id: NodeId) -> &str {
        let s = self.name_off[id as usize] as usize;
        let e = s + self.name_len[id as usize] as usize;
        std::str::from_utf8(&self.names[s..e]).unwrap_or("<invalid utf8>")
    }

    pub fn path(&self, id: NodeId) -> String {
        let mut parts = Vec::new();
        let mut cur = id;
        loop {
            parts.push(self.name(cur));
            let p = self.parent[cur as usize];
            if p == NO_PARENT { break; }
            cur = p;
        }
        parts.reverse();
        parts.join("/")
    }

    pub fn descendants(&self, id: NodeId) -> Range<NodeId> {
        (id + 1)..(id + self.subtree_len[id as usize])
    }

    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let end = id + self.subtree_len[id as usize];
        let mut cur = id + 1;
        std::iter::from_fn(move || {
            if cur >= end { return None; }
            let out = cur;
            cur += self.subtree_len[out as usize];
            Some(out)
        })
    }

    /// Structural invariants. Compiled out in release.
    pub fn check(&self) {
        debug_assert_eq!(self.parent.len(), self.subtree_len.len());
        debug_assert_eq!(self.parent.len(), self.logical.len());
        for i in 0..self.len() {
            let id = i as NodeId;
            debug_assert!((id + self.subtree_len[i]) as usize <= self.len(),
                "subtree of {i} overruns arena");
            for c in self.children(id) {
                debug_assert_eq!(self.parent[c as usize], id, "child {c} disagrees on parent");
            }
            if !self.flags[i].is_dir() {
                debug_assert_eq!(self.subtree_len[i], 1, "non-dir {i} has children");
            }
        }
    }
}
