use crate::{Flags, NodeId, Tree, NO_PARENT};
use std::collections::HashMap;

fn depths(t: &Tree) -> Vec<u32> {
    let mut d = vec![0u32; t.len()];
    for i in 1..t.len() {
        let p = t.parent[i];
        debug_assert_ne!(p, NO_PARENT, "only node 0 may be rootless");
        d[i] = d[p as usize] + 1;
    }
    d
}

fn lca(t: &Tree, depth: &[u32], mut a: NodeId, mut b: NodeId) -> NodeId {
    while depth[a as usize] > depth[b as usize] { a = t.parent[a as usize]; }
    while depth[b as usize] > depth[a as usize] { b = t.parent[b as usize]; }
    while a != b { a = t.parent[a as usize]; b = t.parent[b as usize]; }
    a
}

pub fn aggregate(t: &mut Tree) {
    let n = t.len();
    if n == 0 { return; }
    let depth = depths(t);

    // Group shared-block families.
    //   nlink == 1  definitely not shared, skip (keeps the map tiny on Unix)
    //   nlink >  1  definitely shared with something
    //   nlink == 0  unknown (Windows has identity but no link count) - insert
    //               and let single-member families be discarded below.
    let mut fam: HashMap<(u64, u64), Vec<NodeId>> = HashMap::new();
    for i in 0..n {
        if t.flags[i].is_dir() { continue; }
        let key = if t.clone_id[i] != 0 {
            (u64::MAX, t.clone_id[i])
        } else if t.ino[i] != 0 && t.nlink[i] != 1 {
            (t.dev[i], t.ino[i])
        } else {
            continue;
        };
        fam.entry(key).or_default().push(i as NodeId);
    }

    // Each family's blocks are credited exactly once, at its LCA.
    let mut credit_at: Vec<u64> = vec![0; n];
    for members in fam.values() {
        if members.len() < 2 { continue; }
        let bytes = t.blocks[members[0] as usize];
        let mut anc = members[0];
        for &m in &members[1..] {
            anc = lca(t, &depth, anc, m);
            t.flags[m as usize].0 |= Flags::SHARED;
        }
        t.flags[members[0] as usize].0 |= Flags::SHARED;
        credit_at[anc as usize] += bytes;
    }

    t.sub_logical = vec![0; n];
    t.sub_blocks = vec![0; n];
    t.sub_excl = vec![0; n];
    t.sub_files = vec![0; n];
    t.sub_dirs = vec![0; n];

    for i in (0..n).rev() {
        let shared = t.flags[i].has(Flags::SHARED);
        t.sub_logical[i] += t.logical[i];
        t.sub_blocks[i] += t.blocks[i];
        if !shared { t.sub_excl[i] += t.blocks[i]; }
        t.sub_excl[i] += credit_at[i];
        if t.flags[i].is_dir() { t.sub_dirs[i] += 1; } else { t.sub_files[i] += 1; }

        let p = t.parent[i];
        if p != NO_PARENT {
            let p = p as usize;
            t.sub_logical[p] += t.sub_logical[i];
            t.sub_blocks[p] += t.sub_blocks[i];
            t.sub_excl[p] += t.sub_excl[i];
            t.sub_files[p] += t.sub_files[i];
            t.sub_dirs[p] += t.sub_dirs[i];
        }
    }

    // The only correctness net in stage 1. Deliberately strict; compiled out
    // in release.
    #[cfg(debug_assertions)]
    {
        let credited: u64 = credit_at.iter().sum();
        let families: u64 = fam.values().filter(|m| m.len() >= 2)
            .map(|m| t.blocks[m[0] as usize]).sum();
        debug_assert_eq!(credited, families, "family bytes credited more or less than once");

        for i in 0..n {
            debug_assert!(t.sub_excl[i] <= t.sub_blocks[i],
                "node {i}: freeable {} exceeds allocated {}", t.sub_excl[i], t.sub_blocks[i]);
            let kids: u64 = t.children(i as NodeId).map(|c| t.sub_blocks[c as usize]).sum();
            debug_assert_eq!(t.sub_blocks[i], kids + t.blocks[i],
                "node {i}: children do not sum to parent");
        }
    }
}
