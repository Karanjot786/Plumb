# Plumb

**Every other disk analyzer tells you what is big. This one tells you what deleting
would actually free.**

Those are not the same number, and the gap is not small. A hardlinked file is counted
once by the filesystem and twice by every tool that sums `st_size`. An APFS clone —
which is what `cp` does on a Mac now, and what Time Machine local snapshots are built
from — shows its full length while sharing every block with its original. A sparse
file reports terabytes and occupies megabytes. Delete any of them expecting the
advertised space back and you will not get it.

Plumb models all four — hardlinks, clones, sparse files, snapshots —
and reports **reclaimable bytes**: the blocks that would genuinely be returned to
the volume. Shared blocks are credited exactly once, at the lowest common ancestor of
everything that shares them, so a folder's number is honest no matter where you look
from. The volume's own free space is reconciled against the total, and whatever cannot
be attributed is shown as unaccounted rather than quietly absorbed.

```
~/Library/Caches
  logical      14.72 GB
  allocated    11.94 GB
  freeable     11.60 GB  <- what deleting actually frees
  contents     247850 files, 21103 folders
  scan time    1.891s
```

---

## Status, honestly

**v0.1.0. macOS is the tier-1 platform. Binaries are unsigned.**

There is no Apple Developer signature on the app or the CLI, because notarization
costs $99/yr and this project does not have it. On first launch macOS will refuse to
open the app. Right-click the app → **Open** → **Open** in the dialog that follows, once;
after that it launches normally. For the CLI, `xattr -d com.apple.quarantine ./plumb`.
If that trade is not acceptable to you — for a tool that moves files, it is a fair
objection — build from source, which is a supported path and produces no quarantine
flag at all.

| Platform | Engine + CLI | Desktop app | Applications tab |
| --- | --- | --- | --- |
| macOS | verified by running | verified by running | **yes** |
| Linux | verified by running | builds; not yet driven under a real session | no |
| Windows | builds and runs in CI on real NTFS | never launched | no |

The Applications tab is **macOS-only**. It reads `/Applications` bundles, their
`Info.plist` identifiers and the files they leave scattered across
`~/Library/{Application Support,Caches,Preferences,Logs,Saved Application State}`.
Windows application discovery is not written, so the tab does not appear there.

## Install

**From source** — the path that works on every platform today:

```bash
git clone https://github.com/Karanjot786/Plumb
cd Plumb
cargo install --path crates/plumb-cli    # installs `plumb`
```

**The desktop app**, from the same checkout:

```bash
cargo install tauri-cli --version "^2" --locked
cargo tauri build --config crates/plumb-ui/tauri.conf.json
```

produces a `.dmg` and `.app` on macOS, a `.deb` and `.AppImage` on Linux.

Linux build dependencies, the exact set CI settles on:

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf libsoup-3.0-dev
```

## Safety — read this part

This tool moves files. The design is one sentence:

> **Nothing is ever deleted except by `commit`, and everything `commit` can delete is
> described by a manifest that was written to disk before the first file moved.**

What that means in practice:

- **`clean` stages, it does not delete.** Selected paths are `rename(2)`d into a
  private staging directory on the *same volume*, so the move is atomic and costs no
  extra space. If a same-device staging directory cannot be created, the item is
  **refused** — there is no fallback that copies, and none that deletes.
- **The manifest is written first.** A crash at any point after that leaves something
  `plumb restore` can finish. The manifest records each item's original path, its staged
  path, its size, and its `(dev, ino, mtime)` identity.
- **Undo is one command.** `plumb restore <id>` puts everything back. Restore never
  clobbers: if something now occupies the original path, that item stays staged and
  stays listed rather than overwriting whatever is there.
- **Deletion is explicit and separate.** `plumb commit <id>` is the only function in the
  entire project that removes anything, and it refuses any path that is not inside a
  staging directory. That check is a runtime gate, not a debug assertion — it is
  compiled into release builds and it is the last thing standing between a path-handling
  bug and your home directory.
- **Identity is re-checked immediately before every move.** If the file changed between
  being inspected and being staged, it is skipped. Comparing path strings would not
  survive a rename in that window; `(dev, ino, mtime)` does.
- **A deny list guards the obvious catastrophes**, compared by inode rather than by
  string so a symlinked alias to `/System` is caught too. On top of it, structural rules
  refuse any path that is not absolute, contains `.` or `..`, has an empty component,
  sits fewer than two levels from a volume root, *is* your home directory, or contains
  it. A collapsed shell variable cannot turn `/Users/$USER/$LEAF` into `/Users`.
- **Mount points are never staged**, symlinks are moved as links and never followed out
  of the tree, and files a system package manager claims (`dpkg`, `rpm`, `pacman`) are
  refused.
- **Uninstall shows guesses; it never stages them.** Application leftovers matched by
  bundle identifier are staged. Matches made on the application's *name* are shown
  under "left alone", because two apps can share a name and one of them is not the one
  you are removing. `--guesses` widens what is displayed, never what is touched.

Staged items live for 30 days by default. `plumb staged` lists every pending manifest.

## The eight views

All eight are rasterized in Rust with `tiny-skia` and blitted to the canvas as a single
image, rather than drawn as DOM or Canvas 2D primitives. That is why a tree with a
quarter of a million nodes stays interactive, and why the frontend behaves the same on
WebKitGTK as it does on WKWebView.

| View | What it is for |
| --- | --- |
| **Treemap** | The default. Squarified, so tiles stay near-square and comparable by area. |
| **Folders** | Plain nested rectangles, one level at a time, when the treemap is too dense to read. |
| **Sunburst** | Radial. Depth reads as distance from the centre, so deep trees stay legible. |
| **Flame** | Icicle layout. Best for finding one deep expensive path. |
| **Bubbles** | Circle packing. Emphasises count and clustering over exact area. |
| **Mind map** | Radial tree. Structure rather than size. |
| **Top sizes** | A ranked list — here, files anywhere, or folders anywhere. |
| **Age map** | Coloured by last-modified age. Old and large is the best cleanup signal there is. |

Every view can be sized by **freeable** (the default), allocated, or logical bytes.
Switching between them is the fastest way to see the gap this project exists to
measure.

Alongside them: a volume donut reconciling the scan against real free space, quick
wins, duplicate detection with optional reflink deduplication, snapshot save/diff, and
a live Monitor that reports what is growing under a directory as it happens.

## CLI

```
plumb scan <path>                    what is here, and what deleting it would free
plumb top <path> [--files]           the biggest entries
plumb old <path> [--days 365]        the stalest entries
plumb dupes <path> [--dedupe]        byte-identical files; --dedupe shares extents instead
plumb clean <paths...> [--dry-run]   stage for removal. deletes nothing
plumb staged                         pending manifests
plumb restore <id>                   put a manifest back
plumb commit <id>                    permanently remove a manifest's contents
plumb apps [--leftovers]             installed applications (macOS)
plumb uninstall <app> [--yes]        review, then stage, an app and what it left behind
plumb watch <path>                   report changes as they happen
plumb snapshot save|list|diff        save and compare scans over time
```

`--json` on any command emits machine-readable output.

## How it works

`dua-core` walks the tree into a flat arena; per-platform syscalls
(`getattrlistbulk` and `F_LOG2PHYS` on macOS, `statx` on Linux,
`GetFileInformationByHandle` on Windows) supply the sharing facts the walk cannot see.
Blocks shared between paths are credited once, at their lowest common ancestor, which
is what makes a folder's reclaimable number correct rather than merely plausible.
Snapshots are `rkyv` archives, mapped rather than parsed, so a diff of two scans opens
instantly.

The design document, the verified-facts research file and the adversarial audit live in a
separate private repository, `Plumb_docs`. They are not published with the code.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). One rule is unusual and load-bearing: **this
project has no test files.** Correctness lives in `debug_assert!` inside the code, and
every change is verified by building a throwaway fixture and running the binary against
it. That is not laziness — every serious bug this project has had was found by running
it, including one where `freeable` exceeded the bytes physically present.

## Security

See [SECURITY.md](SECURITY.md).

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE). Apache rather than MIT for the
explicit patent grant: this tool issues syscalls (`clonefile`, `FIDEDUPERANGE`,
`FSCTL_QUERY_FILE_LAYOUT`) whose surrounding techniques are patented territory, and a
bare MIT grant says nothing about patents.
