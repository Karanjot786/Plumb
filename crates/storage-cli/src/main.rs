use clap::{Parser, Subcommand};
use std::path::PathBuf;
use storage_core::{aggregate, quick_wins, reconcile, scan, volume_of, Flags, NodeId, Tree};

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
    let mut t = scan(path, threads)?;
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
    }
    Ok(())
}
