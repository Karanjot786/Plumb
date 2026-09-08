use clap::{Parser, Subcommand};
use std::path::PathBuf;
use storage_core::snapshot;
use storage_core::{
    commit, list_staged, plan, quick_wins, reconcile, restore, scan, stage, volume_of,
    Flags, NodeId, Plan, Tree,
};

#[derive(Parser)]
#[command(name = "sv", version, about = "Storage visualizer engine")]
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
    /// List installed applications.
    Apps {
        #[arg(long, default_value_t = 30)] limit: usize,
        /// Show the files each application left around the system.
        #[arg(long)] leftovers: bool,
        /// Restrict to applications whose name or bundle id contains this.
        #[arg(long)] app: Option<String>,
        /// Include name-keyed matches. These are guesses, not proof.
        #[arg(long)] guesses: bool,
    },
    /// Review, and with --yes stage, an application and everything it left behind.
    Uninstall {
        /// Name or bundle id substring. Must select exactly one application.
        app: String,
        /// Actually stage. Without it this prints the review and touches nothing.
        #[arg(long)] yes: bool,
        /// Also show name-keyed matches. They are guesses, not proof, so they
        /// are listed under "left alone" and are never staged.
        #[arg(long)] guesses: bool,
    },
    /// Find byte-identical files and report what deleting them would free.
    Dupes {
        path: PathBuf,
        #[arg(long, default_value_t = 0)] min_size: u64,
        #[arg(long, default_value_t = 20)] limit: usize,
        /// Share extents between identical copies instead of listing them.
        #[arg(long)] dedupe: bool,
        /// With --dedupe, run every check and change nothing.
        #[arg(long)] dry_run: bool,
    },
    /// Follow a directory and report what changes under it.
    Watch {
        path: PathBuf,
        /// Stop after this long. 0 runs until interrupted.
        #[arg(long, default_value_t = 0)] seconds: u64,
        /// Milliseconds between drains.
        #[arg(long, default_value_t = 250)] tick: u64,
        /// Force rescan-and-diff instead of the native backend. This is what
        /// Linux always does, and it is the only way to exercise it here.
        #[arg(long)] poll: bool,
        /// Seconds between rescans, with --poll.
        #[arg(long, default_value_t = 30)] every: u64,
    },
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
    let private = storage_core::scan::private_sizes(&t, path);
    storage_core::aggregate::aggregate_with_private(&mut t, &private);
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
        Cmd::Apps { limit, leftovers, app, guesses } => {
            let apps = storage_core::apps::list_apps()?;
            if apps.is_empty() {
                println!("no applications found");
                return Ok(());
            }
            if *leftovers {
                return show_leftovers(&apps, app.as_deref(), *guesses, *limit);
            }
            let mut sized: Vec<(u64, usize, &storage_core::apps::App)> = apps
                .iter()
                .enumerate()
                .map(|(i, a)| (storage_core::apps::dir_bytes(&a.path), i, a))
                .collect();
            sized.sort_by_key(|(b, _, _)| std::cmp::Reverse(*b));
            let mut contested = 0usize;
            for (bytes, i, a) in sized.iter().take(*limit) {
                let id = a.bundle_id.as_deref().unwrap_or("(no bundle id)");
                let with = storage_core::apps::contesting_ids(&apps, *i);
                let shared = if with.is_empty() { "" } else { "  [id shared]" };
                if !with.is_empty() {
                    contested += 1;
                }
                println!("{:>10}  {:<34} {}{}", human(*bytes), a.name, id, shared);
                if let Some(v) = &a.version {
                    println!("            v{v}  {}", a.path.display());
                }
            }
            println!("\n  {} application(s)", apps.len());
            if contested > 0 {
                println!("  {contested} shown application(s) share a bundle id with another");
            }
        }
        Cmd::Watch { path, seconds, tick, poll, every } => {
            return watch(path, *seconds, *tick, *poll, *every);
        }
        Cmd::Uninstall { app, yes, guesses } => {
            return uninstall(app, *yes, *guesses, cli.threads);
        }
        Cmd::Dupes { path, min_size, limit, dedupe, dry_run } => {
            let t = load(path, cli.threads)?;
            let root = storage_core::blocklist::canon_keep_link(path);
            let started = std::time::Instant::now();
            let groups = storage_core::dupes::find(&t, &root, *min_size);
            let took = started.elapsed();
            if groups.is_empty() {
                println!("no duplicates found in {:?}", took);
                return Ok(());
            }
            for g in groups.iter().take(*limit) {
                println!("{:>10} each x{} names, {} physical cop{}  ->  {} reclaimable",
                    human(g.bytes_each), g.members.len(), g.copies,
                    if g.copies == 1 { "y" } else { "ies" },
                    human(g.reclaimable));
                for m in &g.members {
                    println!("     {} {}", if m.shared { "[shares blocks]" } else { "               " },
                        m.path.display());
                }
            }
            let total = storage_core::dupes::total_reclaimable(&groups);
            println!("\n  {} group(s) in {:?}", groups.len(), took);
            println!("  {} reclaimable  (already-shared copies excluded)", human(total));

            if !dedupe {
                if storage_core::reflink::supported(&root) {
                    println!("  this mount supports reflinks: sv dupes {} --dedupe --dry-run",
                        path.display());
                }
                return Ok(());
            }
            if !storage_core::reflink::supported(&root) {
                println!("\n  this mount does not support reflinks; nothing to do");
                return Ok(());
            }
            let r = storage_core::reflink::dedupe_groups(&groups, *dry_run);
            println!("\n{}", if *dry_run { "dry run, nothing changed:" } else { "deduped:" });
            for d in &r.done {
                println!("  {:>10}  {}\n              shares with {}",
                    human(d.bytes), d.replaced.display(), d.kept.display());
            }
            for f in &r.refused {
                println!("  refused    {}  ({})", f.path.display(), f.why);
            }
            println!("\n  {} file(s), {} {}", r.done.len(), human(r.freed),
                if *dry_run { "would be freed" } else { "freed" });
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

/// Report what each application left behind. Read-only in every branch: this
/// prints and nothing else.
fn show_leftovers(
    apps: &[storage_core::apps::App],
    filter: Option<&str>,
    guesses: bool,
    limit: usize,
) -> std::io::Result<()> {
    use storage_core::apps::{self, LeftoverOpts};

    let needle = filter.map(|f| f.to_lowercase());
    // Carry the slot: the sibling guard excludes an app by position, not by
    // id value, so two apps declaring the same id still guard each other.
    let picked: Vec<(usize, &apps::App)> = apps
        .iter()
        .enumerate()
        .filter(|(_, a)| match &needle {
            None => true,
            Some(n) => {
                a.name.to_lowercase().contains(n)
                    || a.bundle_id.as_deref().unwrap_or("").to_lowercase().contains(n)
            }
        })
        .collect();

    if picked.is_empty() {
        println!("no application matched");
        return Ok(());
    }

    let opts = LeftoverOpts { include_name_matches: guesses };
    for (i, a) in picked.iter().take(limit) {
        let others = apps::other_ids(apps, *i);
        let items = apps::associated_for(a, &others, opts);
        let removable = apps::stageable(&items);
        let id = a.bundle_id.as_deref().unwrap_or("(no bundle id)");
        let total: u64 = items.iter().map(|i| i.bytes).sum();
        let free: u64 = removable.iter().map(|r| r.item().bytes).sum();

        println!("\n{}  [{}]", a.name, id);
        println!("  {} in {} item(s); {} removable here", human(total), items.len(), human(free));
        let with = apps::contesting_ids(apps, *i);
        if !with.is_empty() {
            println!("  sibling guard: also claimed by {}", with.join(", "));
        }
        for i in &items {
            let why = apps::exclusion_reason(i);
            let mark = if why.is_some() { "  --" } else { "  ok" };
            println!(
                "{mark} {:>10}  {:<24} {}",
                human(i.bytes),
                i.category.label(),
                i.path.display()
            );
            println!("            {}{}", i.evidence.label(), why.map(|w| format!("; {w}")).unwrap_or_default());
        }
    }
    Ok(())
}

/// Review an uninstall, and with `--yes` route it through staging.
///
/// Nothing is deleted here or anywhere downstream: staging is a rename into
/// an app-owned directory that `sv restore` reverses and only `sv commit`
/// makes permanent.
fn uninstall(needle: &str, yes: bool, guesses: bool, threads: usize) -> std::io::Result<()> {
    use storage_core::apps::{self, AppProvider, LeftoverOpts};

    let mut local = apps::Local::new()?;
    local.opts = LeftoverOpts { include_name_matches: guesses };
    let n = needle.to_lowercase();
    let all = local.list()?;
    let picked: Vec<&apps::App> = all
        .iter()
        .filter(|a| {
            a.name.to_lowercase().contains(&n)
                || a.bundle_id.as_deref().unwrap_or("").to_lowercase().contains(&n)
        })
        .collect();

    // An exact name or id wins outright. Substring alone left any application
    // whose name or bundle id is a prefix of another's permanently
    // unselectable - `sv uninstall Codex` listed Codex and ChatGPT and refused
    // to act, with nothing the user could type to break the tie.
    let exact: Vec<&apps::App> = picked
        .iter()
        .copied()
        .filter(|a| {
            a.name.to_lowercase() == n
                || a.bundle_id.as_deref().unwrap_or("").to_lowercase() == n
        })
        .collect();
    let picked = if exact.len() == 1 { exact } else { picked };

    match picked.len() {
        0 => {
            println!("no application matched {needle:?}");
            return Ok(());
        }
        1 => {}
        _ => {
            println!("{needle:?} matched {} applications; name one exactly:", picked.len());
            for a in picked {
                println!("  {:<34} {}", a.name, a.bundle_id.as_deref().unwrap_or("-"));
            }
            return Ok(());
        }
    }

    let plan = local.uninstall_plan(picked[0])?;
    println!("Uninstall {}  [{}]", plan.app, plan.bundle_id.as_deref().unwrap_or("no bundle id"));
    println!("\n  will be staged - {} in {} item(s)", human(plan.bytes), plan.items.len());
    for r in &plan.items {
        println!("    {:>10}  {:<24} {}", human(r.item().bytes), r.item().category.label(), r.path().display());
    }
    if !plan.excluded.is_empty() {
        println!("\n  left alone - {} item(s)", plan.excluded.len());
        for (path, why) in &plan.excluded {
            println!("    {}\n      {why}", path.display());
        }
    }
    if !plan.unload.is_empty() {
        println!("\n  will be stopped first - {} job(s)", plan.unload.len());
        for u in &plan.unload {
            println!("    {}", u.label());
        }
    }

    if !yes {
        println!("\n  review only: nothing was moved. Re-run with --yes to stage.");
        return Ok(());
    }
    if plan.is_empty() {
        println!("\n  nothing to stage");
        return Ok(());
    }

    // Plan everything first. A refusal of the bundle itself is fatal, and
    // finding that out after booting out the app's helpers would leave a
    // stopped application that is still installed.
    let prepared = storage_core::apps::prepare_uninstall(&plan, threads)?;
    for (path, why) in &prepared.refused {
        println!("  refused  {}  ({why})", path.display());
    }

    // Only now stop the background jobs, so a running helper cannot recreate
    // what is about to move.
    for (label, why) in storage_core::apps::perform_unload(&plan) {
        println!("  {label}: {why}");
    }

    let planned = prepared.count();
    let m = storage_core::apps::stage_prepared(&prepared, &format!("uninstall {}", plan.app))?;
    println!("\n  staged {} items, {} -> manifest {}", m.items.len(), human(m.total_bytes), m.id);
    // Only the gap between planning and moving is a TOCTOU skip. Anything the
    // planner declined was already printed above with its real reason.
    let changed = planned - m.items.len();
    if changed > 0 {
        println!("  {changed} skipped: changed between planning and staging");
    }
    println!("  undo with: sv restore {}", m.id);
    Ok(())
}

/// Follow a directory. Prints one line per coalesced event, never one per raw
/// event, which is the difference between usable and unusable during a build.
fn watch(path: &PathBuf, seconds: u64, tick: u64, poll: bool, every: u64) -> std::io::Result<()> {
    use storage_core::watch::{watcher, Feed, Poll, Watcher};

    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut w: Box<dyn Watcher> = if poll {
        let interval = std::time::Duration::from_secs(every.max(1));
        println!("rescan-and-diff every {}s", interval.as_secs());
        Box::new(Poll::new(path, threads, interval)?)
    } else {
        watcher(path, threads)?
    };
    let mut feed = Feed::new(500);
    println!("watching {} - ctrl-c to stop", path.display());

    let start = std::time::Instant::now();
    let tick = std::time::Duration::from_millis(tick.max(10));
    let mut printed = 0usize;
    let mut worst = std::time::Duration::ZERO;
    loop {
        if seconds > 0 && start.elapsed().as_secs() >= seconds {
            break;
        }
        let t0 = std::time::Instant::now();
        let batch = w.events()?;
        let n = batch.len();
        feed.absorb(batch);
        let drain = t0.elapsed();
        if drain > worst {
            worst = drain;
        }
        if n > 0 {
            // Oldest first, so the log reads in the order things happened.
            let fresh: Vec<String> = feed
                .events()
                .take(n)
                .map(|e| {
                    let d = e.bytes;
                    let sign = if d < 0 { "-" } else { "+" };
                    format!(
                        "  {:<9} {sign}{:>10}  {}",
                        e.kind.label(),
                        human(d.unsigned_abs()),
                        e.path.display()
                    )
                })
                .collect();
            for line in fresh.iter().rev() {
                println!("{line}");
            }
            printed += n;
            println!(
                "  -- {n} path(s) this drain in {:.1}ms, {} raw event(s) so far, {} stale dir(s)",
                drain.as_secs_f64() * 1000.0,
                feed.seen,
                feed.stale().len()
            );
        }
        std::thread::sleep(tick);
    }
    println!(
        "\n  {printed} coalesced event(s) from {} raw; worst drain {:.1}ms",
        feed.seen,
        worst.as_secs_f64() * 1000.0
    );
    Ok(())
}
