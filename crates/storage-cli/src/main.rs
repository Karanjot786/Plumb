use clap::{Parser, Subcommand};
use std::path::PathBuf;
use storage_core::snapshot;
use storage_core::{
    aggregate, commit, list_staged, plan, quick_wins, reconcile, restore, scan, stage, volume_of,
    Flags, NodeId, Plan, Tree,
};

#[derive(Parser)]
#[command(name = "sv", about = "Storage visualizer engine")]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true, default_value_t = 0)]
    threads: usize,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Scan { path: PathBuf },
    Top {
        path: PathBuf,
        #[arg(long, default_value_t = 20)] limit: usize,
        #[arg(long)] files: bool,
    },
    Old {
        path: PathBuf,
        #[arg(long, default_value_t = 365)] days: u32,
        #[arg(long, default_value_t = 20)] limit: usize,
    },
    /// Stage paths for removal. Nothing is deleted; use `commit` for that.
    Clean {
        paths: Vec<PathBuf>,
        /// Print the manifest that would be written and touch nothing.
        #[arg(long)] dry_run: bool,
        #[arg(long, default_value = "cleanup")] label: String,
    },
    /// List pending manifests.
    Staged,
    /// Move everything in a manifest back where it came from.
    Restore { id: u64 },
    /// Permanently remove everything in a manifest.
    Commit { id: u64 },
    /// Save, list and compare snapshots of a scanned tree.
    Snapshot {
        #[command(subcommand)]
        cmd: SnapCmd,
    },
}

#[derive(Subcommand)]
enum SnapCmd {
    /// Scan a path and write a snapshot.
    Save {
        path: PathBuf,
        #[arg(long)] out: Option<PathBuf>,
    },
    /// List saved snapshots, newest first.
    List,
    /// Compare two snapshots.
    Diff {
        old: PathBuf,
        new: PathBuf,
        #[arg(long, default_value_t = 40)] limit: usize,
    },
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 4 { v /= 1024.0; i += 1; }
    if i == 0 { format!("{b} B") } else { format!("{v:.2} {}", U[i]) }
}

fn load(path: &PathBuf, threads: usize) -> std::io::Result<Tree> {
    let threads = if threads == 0 {
        std::thread::available_parallelism().map_or(4, |n| n.get())
    } else { threads };
    let mut t = scan(path, threads, |_| {})?;
    aggregate(&mut t);
    Ok(t)
}

fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Scan { path } => {
            let started = std::time::Instant::now();
            let t = load(path, cli.threads)?;
            let elapsed = started.elapsed();
            if t.is_empty() { println!("nothing scanned"); return Ok(()); }
            let vol = volume_of(path).ok();
            let denied = (0..t.len()).filter(|&i| t.flags[i].has(Flags::DENIED)).count();
            let shared = (0..t.len()).filter(|&i| t.flags[i].has(Flags::SHARED)).count();

            if cli.json {
                let mut o = serde_json::json!({
                    "path": path,
                    "files": t.sub_files[0], "dirs": t.sub_dirs[0],
                    "logical": t.sub_logical[0], "allocated": t.sub_blocks[0],
                    "freeable": t.sub_excl[0],
                    "denied": denied, "shared": shared,
                    "elapsed_ms": elapsed.as_millis() as u64,
                });
                if let Some(v) = &vol {
                    let r = reconcile(&t, v);
                    o["volume_used"] = r.used.into();
                    o["unaccounted"] = r.unaccounted.into();
                }
                println!("{}", serde_json::to_string_pretty(&o).unwrap());
            } else {
                println!("{}", path.display());
                println!("  {:<12} {}", "logical", human(t.sub_logical[0]));
                println!("  {:<12} {}", "allocated", human(t.sub_blocks[0]));
                println!("  {:<12} {}  <- what deleting actually frees", "freeable", human(t.sub_excl[0]));
                println!("  {:<12} {} files, {} folders", "contents", t.sub_files[0], t.sub_dirs[0]);
                println!("  {:<12} {:?}", "scan time", elapsed);
                if shared > 0 {
                    println!("  {:<12} {shared} entries share blocks with another path", "shared");
                }
                if denied > 0 {
                    println!("  {:<12} {denied} entries unreadable (grant Full Disk Access to include them)", "denied");
                }
                if let Some(v) = &vol {
                    let r = reconcile(&t, v);
                    println!("\n  volume used {}", human(r.used));
                    println!("    scanned      {}", human(r.scanned));
                    println!("    unaccounted  {}  (outside root, snapshots, purgeable)", human(r.unaccounted));
                }
                let wins = quick_wins(&t);
                if !wins.is_empty() {
                    println!("\n  quick wins");
                    for w in wins.iter().take(8) {
                        println!("    {:>10}  {:<22} {} ({} files)",
                            human(w.bytes), w.label, t.path(w.id), w.items);
                    }
                }
            }
        }
        Cmd::Top { path, limit, files } => {
            let t = load(path, cli.threads)?;
            let mut ids: Vec<NodeId> = (0..t.len() as NodeId)
                .filter(|&i| t.flags[i as usize].is_dir() != *files)
                .collect();
            ids.sort_unstable_by_key(|&i| std::cmp::Reverse(t.sub_blocks[i as usize]));
            for (n, &i) in ids.iter().take(*limit).enumerate() {
                println!("{:>3}  {:>10}  {}", n + 1, human(t.sub_blocks[i as usize]), t.path(i));
            }
        }
        Cmd::Old { path, days, limit } => {
            let t = load(path, cli.threads)?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()) as u32;
            let cutoff = now.saturating_sub(days.saturating_mul(86_400));
            let mut ids: Vec<NodeId> = (0..t.len() as NodeId)
                .filter(|&i| {
                    let i = i as usize;
                    !t.flags[i].is_dir() && t.mtime[i] != 0 && t.mtime[i] < cutoff
                })
                .collect();
            ids.sort_unstable_by_key(|&i| std::cmp::Reverse(t.blocks[i as usize]));
            for (n, &i) in ids.iter().take(*limit).enumerate() {
                let age = (now - t.mtime[i as usize]) / 86_400;
                println!("{:>3}  {:>10}  {:>5}d  {}", n + 1, human(t.blocks[i as usize]), age, t.path(i));
            }
        }
        Cmd::Clean { paths, dry_run, label } => {
            if paths.is_empty() {
                eprintln!("nothing to clean: give at least one path");
                return Ok(());
            }
            // One command produces one manifest, so a single `restore` undoes
            // the whole thing.
            let mut all: Option<Plan> = None;
            let mut blocked = Vec::new();
            for p in paths {
                // Check before scanning: a blocked path should cost nothing,
                // and walking /System to be told no is absurd.
                let c = storage_core::blocklist::canon_keep_link(p);
                let why = storage_core::blocklist::shape_problem(&c)
                    .or_else(|| storage_core::blocklist::denied(&c));
                if let Some(why) = why {
                    println!("refused  {}  (blocked: {why})", p.display());
                    blocked.push(p.clone());
                    continue;
                }
                let t = load(p, cli.threads)?;
                if t.is_empty() {
                    eprintln!("skipping {}: nothing readable", p.display());
                    continue;
                }
                let root = storage_core::blocklist::canon_keep_link(p);
                let one = plan(&t, &root, &[0]);
                match &mut all {
                    Some(a) => a.absorb(one),
                    None => all = Some(one),
                }
            }
            let Some(all) = all else {
                if !blocked.is_empty() {
                    println!("\n  {} path(s) refused, nothing staged", blocked.len());
                }
                return Ok(());
            };

            for (path, why) in &all.refused {
                println!("refused  {}  ({why})", path.display());
            }
            if cli.json {
                let m = all.manifest(label);
                println!("{}", serde_json::to_string_pretty(&m).unwrap());
            } else {
                for i in &all.staged {
                    println!("{:>10}  {}", human(i.bytes), i.original.display());
                }
                println!("\n  {} items, {}", all.staged.len(), human(all.total_bytes));
            }
            if *dry_run {
                println!("\n  dry run: nothing was moved");
                return Ok(());
            }
            if all.staged.is_empty() {
                println!("\n  nothing to stage");
                return Ok(());
            }
            let m = stage(&all, label)?;
            let skipped = all.staged.len() - m.items.len();
            println!("\n  staged {} items, {} -> manifest {}", m.items.len(), human(m.total_bytes), m.id);
            if skipped > 0 {
                println!("  {skipped} skipped: changed between planning and staging");
            }
            println!("  undo with: sv restore {}", m.id);
        }
        Cmd::Staged => {
            let now = storage_core::clean::now_secs();
            let list = list_staged()?;
            if list.is_empty() {
                println!("nothing staged");
                return Ok(());
            }
            let mut total = 0u64;
            for m in &list {
                let days = (m.expires_at.saturating_sub(now)) / 86_400;
                let when = if m.is_expired(now) {
                    "expired".to_string()
                } else {
                    format!("{days}d left")
                };
                println!("{:>12}  {:>10}  {:<16} {} items  {when}",
                    m.id, human(m.total_bytes), m.label, m.items.len());
                total += m.total_bytes;
            }
            println!("\n  {} pending, {}", list.len(), human(total));
        }
        Cmd::Restore { id } => {
            let n = restore(*id)?;
            println!("restored {n} items from manifest {id}");
            let left = storage_core::clean::read_manifest(*id)
                .map(|m| m.items.len())
                .unwrap_or(0);
            if left > 0 {
                println!("  {left} left staged: something already occupies their original path");
            }
        }
        Cmd::Commit { id } => {
            let r = commit(*id)?;
            println!("manifest {id}: {} freed", human(r.freed));
            for (path, why) in &r.skipped {
                println!("  skipped  {}  ({why})", path.display());
            }
            if !r.skipped.is_empty() {
                println!("  {} item(s) left staged; the manifest still lists them", r.skipped.len());
            }
            println!("  entries sharing blocks with another path free less than their listed size");
        }
        Cmd::Snapshot { cmd } => match cmd {
            SnapCmd::Save { path, out } => {
                let t = load(path, cli.threads)?;
                if t.is_empty() {
                    println!("nothing scanned");
                    return Ok(());
                }
                let out = match out {
                    Some(o) => o.clone(),
                    None => {
                        let stem = path
                            .file_name()
                            .map(|s| s.to_string_lossy().replace(['/', ' '], "_"))
                            .unwrap_or_else(|| "root".into());
                        snapshot::snapshots_dir()?
                            .join(format!("{stem}-{}.svsnap", storage_core::clean::now_secs()))
                    }
                };
                let started = std::time::Instant::now();
                snapshot::save(&t, &out)?;
                let wrote = started.elapsed();

                // Prove the round trip here rather than trusting it: reopen the
                // file we just wrote and compare against the live arena.
                let opened = std::time::Instant::now();
                let snap = snapshot::Snapshot::open(&out)?;
                let read = opened.elapsed();
                let live = snapshot::Totals::of(&t);
                let back = snap.root_totals();
                println!("{}", out.display());
                println!("  {:<12} {} nodes", "scanned", t.len());
                println!("  {:<12} {} ({:?} to write, {:?} to open+validate)",
                    "snapshot", human(std::fs::metadata(&out)?.len()), wrote, read);
                match (&live, &back) {
                    (Some(a), Some(b)) if a == b => {
                        println!("  {:<12} identical to the live scan", "round trip");
                        println!("    nodes {}  logical {}  allocated {}  freeable {}  files {}  dirs {}",
                            b.nodes, b.logical, b.allocated, b.freeable, b.files, b.dirs);
                    }
                    _ => {
                        println!("  {:<12} MISMATCH", "round trip");
                        println!("    live     {live:?}");
                        println!("    reopened {back:?}");
                        std::process::exit(1);
                    }
                }
            }
            SnapCmd::Diff { old, new, limit } => {
                let a = snapshot::Snapshot::open(old)?;
                let b = snapshot::Snapshot::open(new)?;
                let started = std::time::Instant::now();
                let changes = storage_core::diff::diff(a.tree(), b.tree());
                let took = started.elapsed();
                if changes.is_empty() {
                    println!("no changes ({} vs {} nodes)", a.len(), b.len());
                    return Ok(());
                }
                let mut net: i128 = 0;
                for c in changes.iter().take(*limit) {
                    let d = c.delta();
                    let sign = if d >= 0 { '+' } else { '-' };
                    println!("{:<8} {sign}{:>10}  {}{}", c.kind.label(),
                        human(d.unsigned_abs() as u64), c.path,
                        if c.is_dir { "/" } else { "" });
                }
                for c in &changes { net += c.delta(); }
                println!("\n  {} change(s) in {:?}", changes.len(), took);
                let sign = if net >= 0 { '+' } else { '-' };
                println!("  net {sign}{}", human(net.unsigned_abs() as u64));
            }
            SnapCmd::List => {
                let list = snapshot::list()?;
                if list.is_empty() {
                    println!("no snapshots");
                    return Ok(());
                }
                for e in &list {
                    let nodes = snapshot::Snapshot::open(&e.path)
                        .map(|s| s.len().to_string())
                        .unwrap_or_else(|err| format!("unreadable: {err}"));
                    println!("{:>10}  {:>10} nodes  {}", human(e.bytes), nodes,
                        e.path.file_name().unwrap_or_default().to_string_lossy());
                }
                println!("\n  {} snapshot(s) in {}", list.len(), snapshot::snapshots_dir()?.display());
            }
        },
    }
    Ok(())
}
