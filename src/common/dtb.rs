//! Flattened Device Tree validation.
//!
//! A vendor_boot dtb area is one or more FDTs packed back-to-back
//! (Pixel 6: two, e.g. raven + oriole). Each FDT starts with
//! `d0 0d fe ed` and declares its own `totalsize`; the next FDT
//! starts exactly at `offset + totalsize` — no page padding between them.

use crate::common::error::{Error, Result};

const FDT_MAGIC: [u8; 4] = [0xD0, 0x0D, 0xFE, 0xED];

fn u32be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Walk concatenated FDTs. Returns the count; errors on bad magic,
/// insane totalsize, or trailing garbage.
pub fn verify(dtb: &[u8], expected: usize) -> Result<usize> {
    // dtb_size 0 is legal: the image simply carries no DTB.
    if expected == 0 {
        if !dtb.is_empty() {
            return Err(Error::Verify(format!(
                "dtb has {} bytes but header dtb_size is 0",
                dtb.len()
            )));
        }
        return Ok(0);
    }
    if dtb.len() != expected {
        return Err(Error::Verify(format!(
            "dtb length {} != header dtb_size {expected}",
            dtb.len()
        )));
    }
    if dtb.len() < 8 {
        return Err(Error::Verify("dtb too small for FDT header".to_string()));
    }
    let mut off = 0usize;
    let mut n = 0u32;
    while off < dtb.len() {
        if off + 8 > dtb.len() {
            return Err(Error::Verify(format!("dtb truncated at FDT {n} header")));
        }
        if dtb[off..off + 4] != FDT_MAGIC {
            return Err(Error::Verify(format!(
                "dtb: FDT {n} without magic at offset {:#x}",
                off
            )));
        }
        let total = u32be(&dtb[off + 4..off + 8]) as usize;
        if total < 8 || off + total > dtb.len() {
            return Err(Error::Verify(format!(
                "dtb: FDT {n} has insane totalsize {total:#x} at offset {off:#x}"
            )));
        }
        off += total;
        n += 1;
        if n > 64 {
            return Err(Error::Verify("dtb has too many FDTs".to_string()));
        }
    }
    if n == 0 {
        return Err(Error::Verify("dtb contains no FDT".to_string()));
    }
    Ok(n as usize)
}
