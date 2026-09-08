# Contributing

## The one rule that will surprise you

**This project has no test files.** No `#[test]`, no test framework, no `tests/`
directory. Please do not add one.

Correctness lives in two places instead:

1. **`debug_assert!` inside the code**, stating invariants at the point where they must
   hold. They compile out of release builds, so they cost shipping users nothing and
   catch developers immediately.
2. **Running the binary against a throwaway fixture.** Build a directory with the
   property you care about — a hardlink, a clone, a sparse file, a symlink pointing out
   of the tree — and run `plumb` at it.

This is not a preference dressed up as a policy. Every serious defect this project has
had was found by running it, not by reasoning about it: `freeable` exceeding the bytes
physically present on any tree containing a hardlink; a `sub_blocks` panic in the
Linux watch loop; a failed read being reported as a deletion. A unit test suite over
the pure functions would have caught none of them, because all three lived in the
seam between the code and a real filesystem.

So: **reproduce before you fix.** Observe the wrong behaviour first. A fix for a defect
you never reproduced is a guess, and in a tool that moves files a guess is expensive.

## Getting set up

```bash
cargo build                       # workspace
cargo run -p plumb-cli -- scan ~/Downloads
```

For the desktop app:

```bash
cargo install tauri-cli --version "^2" --locked
cargo tauri dev --config crates/plumb-ui/tauri.conf.json
```

On Linux you will need the WebKitGTK development packages first:

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf libsoup-3.0-dev
```

## Making a fixture

Never point a destructive command at real data during development. Fixtures only.
Reading real state — listing `/Applications`, scanning a directory — is fine and
encouraged; a real machine makes a better test than anything synthetic.

```bash
F=$(mktemp -d)
mkdir -p "$F/a" "$F/b"
dd if=/dev/zero of="$F/a/big.bin" bs=1m count=50
ln "$F/a/big.bin" "$F/b/hard.bin"          # the whole thesis in one line
cargo run -p plumb-cli -- scan "$F"      # freeable must be below logical
```

## Where things live

| Crate | What it is |
| --- | --- |
| `plumb-core` | The engine. Scanning, accounting, layout, rasterizing, staging, apps, watch. |
| `plumb-cli` | `plumb`. A thin shell over the engine; every feature is reachable here first. |
| `plumb-ui` | The Tauri desktop app. Commands only — no logic that is not in the core. |

Read `HANDOFF.md` before anything else. It records what is verified by having been run
versus what merely compiles, which is a distinction this project takes seriously. The
design document under `docs/superpowers/specs/` is §-numbered and is the reference for
why things are the way they are.

## House style

- **Vendor or depend, never rewrite.** If working code exists, reuse it. `dua-core`
  replaced a hand-written scanner; a d3-derived squarify replaced a hand-rolled
  treemap. Both were improvements.
- **Licence hygiene.** The best prior art in this space is GPL — WinDirStat, rmlint,
  qdirstat, BleachBit. Take concepts from prose and issue threads; **never read their
  source.** Their path lists and rule files are the copyrightable asset. This project is
  Apache-2.0 and intends to stay unencumbered.
- **Refusing beats guessing.** Anywhere a safety check cannot fully verify something,
  it says no. Refusing costs the user a click; guessing costs them a file.
- **Comments explain why, not what.** The code says what it does.

## Commits and pull requests

One commit per logical change, with a message that explains **why** — the reasoning is
the part that cannot be recovered from the diff. CI builds *and runs* the CLI on
Windows, Linux and macOS against a fixture containing a hardlink; it must stay green.

By contributing you agree that your contributions are licensed under Apache-2.0, per
section 5 of the licence.
