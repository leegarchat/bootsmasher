//! newc (`070701`) cpio parsing, building, splitting and verification.
//!
//! The parser is intentionally strict about bounds but tolerant about
//! content: it walks entries by the header-declared sizes and only
//! interprets the pathname for the dlkm split decision.

use crate::common::error::{Error, Result};

const HDR_LEN: usize = 110;
const TRAILER: &[u8] = b"TRAILER!!!";

#[derive(Debug, Clone)]
pub struct Entry {
    pub header: [u8; HDR_LEN],
    pub name: Vec<u8>,
    pub data: Vec<u8>,
}

fn hex8(b: &[u8]) -> Result<usize> {
    if b.len() != 8 {
        return Err(Error::Parse("cpio header field truncated".to_string()));
    }
    let s = std::str::from_utf8(b).map_err(|_| Error::Parse("cpio header not ASCII".to_string()))?;
    usize::from_str_radix(s, 16).map_err(|_| Error::Parse("cpio header not hex".to_string()))
}

fn pad4(n: usize) -> usize {
    (4 - (n % 4)) % 4
}

fn align512(n: usize) -> usize {
    (n + 511) & !511
}

/// Parse one concatenated cpio blob into entries (TRAILER entries kept).
/// GNU cpio pads each archive to a 512-byte boundary with zeros; such
/// padding (between concatenated archives and at the very end) is skipped.
pub fn parse(blob: &[u8]) -> Result<Vec<Entry>> {
    let mut out = Vec::new();
    let mut pos = 0usize; // absolute offset, for 512-block padding
    let mut rest = blob;
    loop {
        if rest.is_empty() {
            break;
        }
        if rest[0] == 0 {
            // Zero padding up to the next 512-byte boundary.
            let aligned = align512(pos);
            let skip = aligned.saturating_sub(pos);
            if skip == 0 || skip > rest.len() {
                // No alignment progress possible: only valid if all zeros.
                if rest.iter().all(|&b| b == 0) {
                    break;
                }
                return Err(Error::Parse("cpio has corrupt zero gap".to_string()));
            }
            if rest[..skip].iter().any(|&b| b != 0) {
                return Err(Error::Parse("cpio entry without 070701 magic".to_string()));
            }
            rest = &rest[skip..];
            pos = aligned;
            continue;
        }
        if rest.len() < HDR_LEN {
            return Err(Error::Parse("cpio truncated inside entry header".to_string()));
        }
        if &rest[0..6] != b"070701" {
            return Err(Error::Parse("cpio entry without 070701 magic".to_string()));
        }
        let filesize = hex8(&rest[54..62])?;
        let namesize = hex8(&rest[94..102])?;
        if namesize == 0 || namesize > 4096 {
            return Err(Error::Parse("cpio has insane namesize".to_string()));
        }
        let name_end = HDR_LEN + namesize;
        if rest.len() < name_end {
            return Err(Error::Parse("cpio truncated inside entry name".to_string()));
        }
        let name = rest[HDR_LEN..name_end].to_vec();
        let data_start = name_end + pad4(name_end);
        let data_end = data_start + filesize;
        if rest.len() < data_end {
            return Err(Error::Parse("cpio truncated inside entry data".to_string()));
        }
        let data = rest[data_start..data_end].to_vec();
        let mut header = [0u8; HDR_LEN];
        header.copy_from_slice(&rest[0..HDR_LEN]);
        let is_trailer = name.split(|&b| b == 0).next().unwrap_or(&[]) == TRAILER;
        out.push(Entry { header, name, data });
        let next = data_end + pad4(data_end);
        if next > rest.len() {
            return Err(Error::Parse("cpio truncated at entry padding".to_string()));
        }
        rest = &rest[next..];
        pos += next;
        if is_trailer {
            // A concatenated archive may follow; keep walking.
            continue;
        }
        if out.len() > 2_000_000 {
            return Err(Error::Parse("cpio has too many entries".to_string()));
        }
    }
    Ok(out)
}

pub fn name_str(e: &Entry) -> String {
    let end = e.name.iter().position(|&b| b == 0).unwrap_or(e.name.len());
    String::from_utf8_lossy(&e.name[..end]).into_owned()
}

/// Drop TRAILER entries (mid-stream trailers would truncate extraction —
/// both GNU cpio and the kernel stop at the first one).
pub fn drop_trailers(entries: &[Entry]) -> Vec<Entry> {
    entries.iter().filter(|e| name_str(e) != "TRAILER!!!").cloned().collect()
}

/// A dlkm entry is everything under `lib/` (matches LOS stock layout
/// where the dlkm fragment is exactly the `lib` subtree).
pub fn is_dlkm_path(name: &str) -> bool {
    name == "lib" || name.starts_with("lib/")
}

/// A recovery entry: an explicit `recovery/` subtree when the ramdisk has
/// one, plus `debug_ramdisk/` (on Pixel ramdisks that is the only
/// recovery-ish dir: usually a single empty-dir entry).
pub fn is_recovery_path(name: &str) -> bool {
    name == "recovery"
        || name.starts_with("recovery/")
        || name == "debug_ramdisk"
        || name.starts_with("debug_ramdisk/")
}

/// A first-stage entry: the `first_stage_ramdisk` dir itself or anything
/// under it. This is the unit the vboot `--drop first-stage` selector and
/// the inbuild-vs-cpio first-stage rule operate on (root-level `init`
/// is ordinary platform payload, not first-stage).
pub fn is_first_stage_path(name: &str) -> bool {
    name == "first_stage_ramdisk" || name.starts_with("first_stage_ramdisk/")
}

/// newc mode field lives at header bytes 14..22 (after 6 magic + 8 ino).
pub(crate) fn entry_mode(e: &Entry) -> Option<u32> {
    if e.header.len() < 22 {
        return None;
    }
    std::str::from_utf8(&e.header[14..22]).ok().and_then(|s| u32::from_str_radix(s, 16).ok())
}

fn is_dir(e: &Entry) -> bool {
    entry_mode(e).is_some_and(|m| m & 0o170000 == 0o040000)
}

/// True when the set carries real content (not just directory entries).
/// Split-derived dlkm/recovery fragments are emitted only in that case —
/// a lone `lib` or `debug_ramdisk` dir entry is not worth a fragment.
pub fn has_payload(entries: &[Entry]) -> bool {
    entries.iter().any(|e| !is_dir(e))
}

/// Serialize entries as one archive with a single TRAILER.
pub fn build(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        out.extend_from_slice(&e.header);
        out.extend_from_slice(&e.name);
        out.extend_from_slice(&vec![0u8; pad4(HDR_LEN + e.name.len())]);
        out.extend_from_slice(&e.data);
        out.extend_from_slice(&vec![0u8; pad4(e.data.len())]);
    }
    // Canonical TRAILER!!! (dir, mtime 0).
    let trailer = format!(
        "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
        0, 0x41ed, 0, 0, 1, 0, 0, 0, 0, 0, 0, 11, 0
    );
    let mut th = [0u8; HDR_LEN];
    th.copy_from_slice(&trailer.as_bytes()[..HDR_LEN]);
    out.extend_from_slice(&th);
    out.extend_from_slice(b"TRAILER!!!\0");
    out.extend_from_slice(&vec![0u8; pad4(HDR_LEN + 11)]);
    out
}

/// Partition entries into (platform, recovery, dlkm) by pathname.
/// TRAILER entries are dropped (build() writes exactly one).
/// Platform = first_stage_ramdisk/** plus everything not claimed above.
pub fn partition(entries: &[Entry]) -> (Vec<Entry>, Vec<Entry>, Vec<Entry>) {
    let mut plat = Vec::new();
    let mut rec = Vec::new();
    let mut dlkm = Vec::new();
    for e in entries {
        let n = name_str(e);
        if n == "TRAILER!!!" {
            continue;
        }
        if is_dlkm_path(&n) {
            dlkm.push(e.clone());
        } else if is_recovery_path(&n) {
            rec.push(e.clone());
        } else {
            plat.push(e.clone());
        }
    }
    (plat, rec, dlkm)
}

/// Verify a decompressed ramdisk: non-empty, every entry parses,
/// at least one TRAILER present.
pub fn verify_blob(decompressed: &[u8]) -> Result<usize> {
    let entries = parse(decompressed)?;
    if entries.is_empty() {
        return Err(Error::Verify("cpio is empty".to_string()));
    }
    let trailers = entries.iter().filter(|e| name_str(e) == "TRAILER!!!").count();
    if trailers == 0 {
        return Err(Error::Verify("cpio has no TRAILER!!!".to_string()));
    }
    Ok(entries.len())
}
