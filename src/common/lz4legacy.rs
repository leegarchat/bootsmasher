//! LZ4-legacy framing used by Android vendor ramdisks.
//!
//! Stream layout: magic `02 21 4c 18`, then blocks
//! `u32 LE block_len | block bytes`, terminated by a zero `u32`.
//! If the top bit of `block_len` is set, the block is stored literally
//! (`len & 0x7FFF_FFFF` bytes); otherwise it is an LZ4 block frame.
//! Uncompressed blocks are at most 8 MiB. Pure-Rust via `lz4_flex`.

use crate::common::error::{Error, Result};

pub const LEGACY_MAGIC: [u8; 4] = [0x02, 0x21, 0x4C, 0x18];
pub const MAX_BLOCK_OUT: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobKind {
    Lz4Legacy,
    Cpio,
    Gzip,
    Xz,
    Lzma,
    Lz4Frame,
    Unknown,
}

impl BlobKind {
    /// Short format name for reports and spec.toml.
    pub fn name(self) -> &'static str {
        match self {
            BlobKind::Lz4Legacy => "lz4_legacy",
            BlobKind::Cpio => "cpio",
            BlobKind::Gzip => "gzip",
            BlobKind::Xz => "xz",
            BlobKind::Lzma => "lzma",
            BlobKind::Lz4Frame => "lz4",
            BlobKind::Unknown => "unknown",
        }
    }
}

/// Sniff the blob: legacy magic, newc cpio magic (`070701`), else unknown.
pub fn sniff(blob: &[u8]) -> BlobKind {
    if blob.len() >= 4 && blob[0..4] == LEGACY_MAGIC {
        BlobKind::Lz4Legacy
    } else if blob.len() >= 6 && blob[0..6] == *b"070701" {
        BlobKind::Cpio
    } else {
        BlobKind::Unknown
    }
}

/// Decompress a full legacy stream. A zero block marker terminates the
/// stream when present (single-stream blobs end with one); fragments
/// packed back-to-back carry no marker, so running out of input exactly
/// on a block boundary is also a clean end. Anything else is an error
/// naming the failing block index instead of guessing.
pub fn decompress_legacy(blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 4 || blob[0..4] != LEGACY_MAGIC {
        return Err(Error::Parse("not an LZ4-legacy stream (bad magic)".to_string()));
    }
    let mut out = Vec::new();
    let mut off = 4usize;
    let mut index = 0u32;
    loop {
        if off == blob.len() {
            break;
        }
        if off + 4 > blob.len() {
            return Err(Error::Parse(format!("legacy stream truncated at block {index} header")));
        }
        let raw = u32::from_le_bytes(blob[off..off + 4].try_into().unwrap());
        off += 4;
        if raw == 0 {
            if off != blob.len() {
                return Err(Error::Parse(format!(
                    "legacy stream has {} trailing bytes after end marker",
                    blob.len() - off
                )));
            }
            break;
        }
        let len = (raw & 0x7FFF_FFFF) as usize;
        let stored = (raw & 0x8000_0000) != 0;
        if off + len > blob.len() {
            return Err(Error::Parse(format!(
                "legacy stream truncated inside block {index}: need {len} bytes, have {}",
                blob.len() - off
            )));
        }
        let data = &blob[off..off + len];
        off += len;
        if stored {
            out.extend_from_slice(data);
        } else {
            let dec = lz4_flex::block::decompress(data, MAX_BLOCK_OUT).map_err(|e| {
                Error::Parse(format!("legacy block {index} does not decompress: {e:?}"))
            })?;
            out.extend_from_slice(&dec);
        }
        index += 1;
        if index > 4096 {
            return Err(Error::Parse("legacy stream has too many blocks".to_string()));
        }
    }
    Ok(out)
}

/// Compress raw bytes into a legacy stream (8 MiB blocks).
/// No end marker is written: kernel-produced ramdisk fragments butt
/// directly against each other (or blob end), and the `lz4` CLI treats
/// a zero word as corruption. The decoder still tolerates a marker for
/// robustness against foreign packers.
///
/// Every block is stored as a valid LZ4 block stream, even when that is
/// larger than the input: the 0x80000000 "stored raw" form is never
/// emitted. Minimal legacy decoders (the `lz4` CLI legacy mode, LK/aboot
/// on Pixel bootloaders) reject raw blocks and abort the whole image —
/// instant reboot at the logo — while accepting plain compressed blocks
/// of any size. Verified: a stream with one 8 MiB raw block fails
/// `lz4 -d`, the same bytes as plain blocks decode fine.
pub fn compress_legacy(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() / 2 + 16);
    out.extend_from_slice(&LEGACY_MAGIC);
    for chunk in raw.chunks(MAX_BLOCK_OUT) {
        let comp = lz4_flex::block::compress(chunk);
        out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
        out.extend_from_slice(&comp);
    }
    out
}

/// Walk the block headers without decompressing. Returns
/// `(blocks, end_offset)` where `end_offset` is just past the zero marker.
pub fn walk_blocks(blob: &[u8]) -> Result<(u32, usize)> {
    if blob.len() < 4 || blob[0..4] != LEGACY_MAGIC {
        return Err(Error::Parse("not an LZ4-legacy stream (bad magic)".to_string()));
    }
    let mut off = 4usize;
    let mut n = 0u32;
    loop {
        if off == blob.len() {
            break;
        }
        if off + 4 > blob.len() {
            return Err(Error::Parse(format!("legacy stream truncated at block {n} header")));
        }
        let raw = u32::from_le_bytes(blob[off..off + 4].try_into().unwrap());
        off += 4;
        if raw == 0 {
            if off != blob.len() {
                return Err(Error::Parse(format!(
                    "legacy stream has {} trailing bytes after end marker",
                    blob.len() - off
                )));
            }
            break;
        }
        let len = (raw & 0x7FFF_FFFF) as usize;
        if off + len > blob.len() {
            return Err(Error::Parse(format!(
                "legacy stream truncated inside block {n}: need {len} bytes"
            )));
        }
        off += len;
        n += 1;
        if n > 4096 {
            return Err(Error::Parse("legacy stream has too many blocks".to_string()));
        }
    }
    Ok((n, off))
}
