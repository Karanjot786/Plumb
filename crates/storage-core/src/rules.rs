use crate::{NodeId, Tree};

pub struct Win { pub label: &'static str, pub id: NodeId, pub bytes: u64, pub items: u32 }

/// Directory basenames worth surfacing. TOML-driven rules with [verification]
/// blocks arrive in stage 3; this is the smallest useful version.
/// WinSxS is deliberately absent: report-only, must never be staged.
const RULES: &[(&str, &str)] = &[
    ("node_modules", "node_modules"),
    ("DerivedData", "Xcode DerivedData"),
    ("Caches", "Caches"),
    (".cache", "User cache"),
    ("Downloads", "Downloads"),
    ("overlay2", "Docker layers"),
    ("target", "Rust build artifacts"),
];

pub fn quick_wins(t: &Tree) -> Vec<Win> {
    let mut out = Vec::new();
    let mut i: NodeId = 0;
    while (i as usize) < t.len() {
        let idx = i as usize;
        if t.flags[idx].is_dir() {
            if let Some((_, label)) = RULES.iter().find(|(n, _)| *n == t.name(i)) {
                out.push(Win { label, id: i, bytes: t.sub_excl[idx], items: t.sub_files[idx] });
                i += t.subtree_len[idx];
                continue;
            }
        }
        i += 1;
    }
    out.sort_unstable_by_key(|w| std::cmp::Reverse(w.bytes));
    out
}
