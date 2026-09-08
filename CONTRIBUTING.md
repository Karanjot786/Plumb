# Contributing

## The rule likely to surprise you

This project has no test files. No `#[test]`, no test framework, no `tests/` directory.
Do not add one.

Correctness lives in two places instead.

- `debug_assert!` inside the code, stating invariants where they must hold. They compile
  out of release builds, so shipping users pay nothing and developers see failures
  immediately.
- Running the binary against a throwaway fixture. Build a directory with the property you
  care about, such as a hardlink, a clone, a sparse file, or a symlink pointing out of the
  tree, then run `plumb` at it.

The rule comes from evidence, not preference. Every serious defect in this project was
found by running the code, never by reasoning about it:

- `freeable` exceeded the bytes physically present on any tree holding a hardlink.
- A `sub_blocks` panic in the Linux watch loop.
- A failed read reported as a deletion.

A unit test suite over the pure functions catches none of the three. All three live in the
seam between the code and a real filesystem.

Reproduce before you fix. Observe the wrong behaviour first. A fix for a defect you never
reproduced is a guess, and guesses cost files in a tool moving them.

## Getting set up

```bash
cargo build
cargo run -p plumb-cli -- scan ~/Downloads
```

The desktop app:

```bash
cargo install tauri-cli --version "^2" --locked
cd crates/plumb-ui
cargo tauri dev
```

Linux needs the WebKitGTK development packages first:

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf libsoup-3.0-dev
```

## Making a fixture

Never point a destructive command at real data during development. Use fixtures. Reading
real state stays fine and useful. Listing `/Applications` or scanning a directory tells
you more than anything synthetic.

```bash
F=$(mktemp -d)
mkdir -p "$F/a" "$F/b"
dd if=/dev/zero of="$F/a/big.bin" bs=1m count=50
ln "$F/a/big.bin" "$F/b/hard.bin"          # the thesis in one line
cargo run -p plumb-cli -- scan "$F"        # freeable must land below logical
```

## Where things live

| Crate | Contents |
| --- | --- |
| `plumb-core` | The engine. Scanning, accounting, layout, rasterizing, staging, apps, watch. |
| `plumb-cli` | `plumb`. A thin shell over the engine. Every feature lands here first. |
| `plumb-ui` | The Tauri desktop app. Commands only, no logic missing from the core. |

The handoff notes and the design document live in a separate private repository,
`Plumb_docs`. The handoff records what has been verified by running versus what only
compiles, a distinction this project takes seriously. The design document is
section-numbered and explains why things work the way they do. Neither ships with the
code.

## House style

- Vendor or depend, never rewrite. Reuse working code. `dua-core` replaced a hand-written
  scanner. A d3-derived squarify replaced a hand-rolled treemap. Both were improvements.
- Licence hygiene. The strongest prior art here carries the GPL: WinDirStat, rmlint,
  qdirstat, BleachBit. Take concepts from prose and issue threads. Never read their
  source. Their path lists and rule files are the copyrightable asset. This project stays
  Apache-2.0 and unencumbered.
- Refusing beats guessing. Where a safety check cannot fully verify something, refuse.
  Refusing costs a click. Guessing costs a file.
- Comments explain why. The code already says what.

## Commits and pull requests

One commit per logical change, with a message explaining why. The reasoning is the part
no one recovers from the diff. Continuous integration builds and runs the CLI on Windows,
Linux and macOS against a fixture containing a hardlink. Keep it green.

Contributing means licensing your contribution under Apache-2.0, per section 5 of the
licence.
