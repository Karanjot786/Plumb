//! Zero-copy snapshots of a scanned arena.
//!
//! The arena is columns of plain integers, which is exactly what rkyv is good
//! at: writing is a memcpy per column and reading is an mmap plus a pointer
//! cast. Nothing is decoded on open.
//!
//! Four things here are load-bearing and cost nothing to keep right:
//!
//! * **`HEADER_LEN` is 32, and it must stay a multiple of 16.** `access` casts
//!   straight into `&map[HEADER_LEN..]`, so a header that is not a multiple of
//!   the maximum alignment leaves every archived pointer misaligned. In release
//!   that surfaces as an unhelpful validation failure, not a clear error.
//! * **Write to a temp path, then rename.** Rewriting a file another process
//!   has mapped raises `SIGBUS`, which no amount of validation catches.
//! * **`access`, not `access_unchecked`, on open.** Validation is O(1) for
//!   `ArchivedVec<ArchivedU64>` because integers have no invalid bit patterns.
//!   Once a mapping has been validated, later re-derives use the unchecked form.
//! * **The header records the format-control settings.** rkyv's endianness and
//!   pointer width are cargo features, so a transitive dependency flipping one
//!   would otherwise reinterpret old files rather than reject them.

use crate::arena::ArchivedTree;
use crate::clean::central;
use crate::Tree;
use memmap2::Mmap;
use rkyv::rancor;
use rkyv::ser::writer::IoWriter;
use std::fs::{self, File};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"SVSNAP01";
/// Multiple of 16. See the module note.
pub const HEADER_LEN: usize = 32;
const FORMAT_VERSION: u16 = 1;

/// Bit 0 little endian, bit 1 aligned. Both are compile-time rkyv features;
/// recording them turns a silent misread into a refusal.
const FORMAT_BITS: u16 = 0b11;
const PTR_WIDTH: u8 = 32;
const RKYV_MAJOR: u8 = 0;
const RKYV_MINOR: u8 = 8;

fn header(payload_len: u64) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[0..8].copy_from_slice(MAGIC);
    h[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    h[10..12].copy_from_slice(&FORMAT_BITS.to_le_bytes());
    h[12] = PTR_WIDTH;
    h[13] = RKYV_MAJOR;
    h[14] = RKYV_MINOR;
    h[16..24].copy_from_slice(&payload_len.to_le_bytes());
    h
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

pub fn snapshots_dir() -> io::Result<PathBuf> {
    let d = central()?.join("snapshots");
    fs::create_dir_all(&d)?;
    Ok(d)
}

/// Streams the arena straight to disk. `to_bytes_in` over an `IoWriter` avoids
/// building the whole archive in memory first, which on a 2M-node tree is the
/// difference between one 206 MB allocation and none.
pub fn save(t: &Tree, out: &Path) -> io::Result<()> {
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = out.with_extension("tmp");

    {
        let file = File::create(&tmp)?;
        let mut w = BufWriter::new(file);
        // Reserve the header so rkyv's position 0 lands on file offset 32.
        // Both are multiples of 16, so its alignment maths stays valid.
        w.write_all(&header(0))?;
        let writer = rkyv::api::high::to_bytes_in::<_, rancor::Error>(t, IoWriter::new(w))
            .map_err(|e| invalid(format!("rkyv serialize failed: {e}")))?;
        let mut w = writer.into_inner();
        w.flush()?;
        let mut file = w.into_inner().map_err(|e| io::Error::other(e.to_string()))?;

        let end = file.seek(SeekFrom::End(0))?;
        let payload_len = end - HEADER_LEN as u64;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header(payload_len))?;
        file.sync_all()?;
    }

    fs::rename(&tmp, out)?;
    if let Some(parent) = out.parent() {
        if let Ok(d) = File::open(parent) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// A mapped snapshot. Holding this alive is what keeps the archived tree valid.
pub struct Snapshot {
    map: Mmap,
    pub path: PathBuf,
}

impl Snapshot {
    pub fn open(path: &Path) -> io::Result<Snapshot> {
        let file = File::open(path)?;
        // SAFETY: the file is ours and is replaced by rename, never rewritten
        // in place, so the mapping cannot be pulled out from under us.
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER_LEN {
            return Err(invalid("snapshot is shorter than its header"));
        }
        if &map[0..8] != MAGIC {
            return Err(invalid("not a snapshot: bad magic"));
        }
        let version = u16::from_le_bytes([map[8], map[9]]);
        if version != FORMAT_VERSION {
            return Err(invalid(format!("snapshot format {version}, expected {FORMAT_VERSION}")));
        }
        let bits = u16::from_le_bytes([map[10], map[11]]);
        if bits != FORMAT_BITS || map[12] != PTR_WIDTH {
            return Err(invalid(
                "snapshot was written with different rkyv format settings (endianness, \
                 alignment or pointer width)",
            ));
        }
        if map[13] != RKYV_MAJOR || map[14] != RKYV_MINOR {
            return Err(invalid(format!(
                "snapshot written by rkyv {}.{}, this build uses {RKYV_MAJOR}.{RKYV_MINOR}",
                map[13], map[14]
            )));
        }
        let payload_len = u64::from_le_bytes(map[16..24].try_into().unwrap());
        if payload_len as usize != map.len() - HEADER_LEN {
            return Err(invalid("snapshot payload length disagrees with the file size"));
        }

        // Validate once, here. Cheap because every column is integers.
        rkyv::access::<ArchivedTree, rancor::Error>(&map[HEADER_LEN..])
            .map_err(|e| invalid(format!("snapshot failed validation: {e}")))?;

        Ok(Snapshot { map, path: path.to_path_buf() })
    }

    /// The archived arena. Unchecked because `open` already validated this
    /// exact mapping and the bytes cannot change underneath it.
    pub fn tree(&self) -> &ArchivedTree {
        unsafe { rkyv::access_unchecked::<ArchivedTree>(&self.map[HEADER_LEN..]) }
    }

    pub fn len(&self) -> usize {
        self.tree().parent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Root aggregates, native-endian. Used to prove a round trip against a
    /// live scan.
    pub fn root_totals(&self) -> Option<Totals> {
        let t = self.tree();
        if t.parent.is_empty() {
            return None;
        }
        Some(Totals {
            nodes: t.parent.len(),
            logical: t.sub_logical[0].to_native(),
            allocated: t.sub_blocks[0].to_native(),
            freeable: t.sub_excl[0].to_native(),
            files: t.sub_files[0].to_native(),
            dirs: t.sub_dirs[0].to_native(),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Totals {
    pub nodes: usize,
    pub logical: u64,
    pub allocated: u64,
    pub freeable: u64,
    pub files: u32,
    pub dirs: u32,
}

impl Totals {
    pub fn of(t: &Tree) -> Option<Totals> {
        if t.is_empty() {
            return None;
        }
        Some(Totals {
            nodes: t.len(),
            logical: t.sub_logical[0],
            allocated: t.sub_blocks[0],
            freeable: t.sub_excl[0],
            files: t.sub_files[0],
            dirs: t.sub_dirs[0],
        })
    }
}

pub struct Entry {
    pub path: PathBuf,
    pub bytes: u64,
    pub modified: u64,
}

pub fn list() -> io::Result<Vec<Entry>> {
    let dir = snapshots_dir()?;
    let mut out = Vec::new();
    for e in fs::read_dir(dir)? {
        let Ok(e) = e else { continue };
        let path = e.path();
        if path.extension().is_none_or(|x| x != "svsnap") {
            continue;
        }
        let Ok(m) = e.metadata() else { continue };
        let modified = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        out.push(Entry { path, bytes: m.len(), modified });
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.modified));
    Ok(out)
}
