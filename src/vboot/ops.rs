//! Smart vendor_boot analyzer + repacker (Pixel 6 / gs101 specialization).
//!
//! Pipeline:
//!   load (bounds-checked sectioning) -> analyze (per-fragment validity,
//!   stale-table detection) -> build new fragment set -> assemble ->
//!   verify rebuilt image in memory -> emit (file or stdout, no chatter).

use crate::common::error::{Error, Result};
use crate::common::codec::{self, Format};
use crate::common::cpio::{self, Entry};
use crate::common::dtb;
use crate::common::vendor::{align_up, type_name, Header, RamdiskEntry, HEADER_LEN};
use crate::common::vendor::{TYPE_DLKM, TYPE_PLATFORM, TYPE_RECOVERY};
use crate::common::lz4legacy::{self, BlobKind};

/// Repack layout selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Keep the original fragment layout (normalize a stale table).
    Keep,
    /// Partition content: first_stage_ramdisk/** + rest -> platform,
    /// recovery|debug_ramdisk/** -> recovery, lib/** -> dlkm.
    Split,
    /// Glue everything into a single platform fragment.
    Merge,
}

/// Repack options: layout mode, `--drop` selectors and header overrides.
///
/// `drop` holds raw selectors: fragment type/name matchers (`platform`,
/// `dlkm`, `recovery`, `none`, `16K`, ...) plus the special `first-stage`
/// (also accepted as `first_stage`), which inverts the first-stage rule
/// (see [`apply_first_stage`]).
///
/// `sets` holds `--set k=v` header overrides, applied to the output header
/// after everything else (same keys as the repack subprogram's vendor
/// path: cmdline, name, page_size, kernel_addr, ramdisk_addr, tags_addr,
/// dtb_addr).
///
/// `recovery` holds the `--recovery <file>` payload (label, bytes): the
/// recovery-install layout (see [`build_recovery`]). Conflicts with a
/// platform file and with Split/Merge (enforced by the CLI, double-checked
/// in [`repack_with_opts`]).
///
/// `drop_footer` (`--drop-footer`): omit the trailing vbmeta/AVB/padding
/// tail instead of carrying it verbatim. Needed when the new content no
/// longer fits the partition next to the old footer (recovery install
/// grows the ramdisk); like `repack --drop-footer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackOpts {
    pub mode: Mode,
    pub drop: Vec<String>,
    pub sets: Vec<(String, String)>,
    pub recovery: Option<(String, Vec<u8>)>,
    pub drop_footer: bool,
}

/// Parsed `--drop` list.
struct DropSet {
    /// Fragment selectors (table type or name).
    frags: Vec<String>,
    /// Drop the inbuild first-stage so the passed cpio's wins; without it
    /// the cpio's first-stage entries are dropped and inbuild ones kept.
    first_stage: bool,
}

fn parse_drop(drop: &[String]) -> DropSet {
    let mut out = DropSet { frags: Vec::new(), first_stage: false };
    for s in drop {
        if s == "first-stage" || s == "first_stage" {
            out.first_stage = true;
        } else {
            out.frags.push(s.clone());
        }
    }
    out
}

/// True when `--drop` selector `sel` addresses table entry `e`: by
/// fragment type (`platform`, `dlkm`, `recovery`, `none`) or by the table
/// name (`16K`, `recovery`, ...). Matching is case-sensitive.
fn frag_selector_matches(sel: &str, e: &RamdiskEntry) -> bool {
    sel == type_name(e.entry_type) || *sel == e.name_str()
}

fn frag_dropped(set: &DropSet, e: &RamdiskEntry) -> bool {
    set.frags.iter().any(|s| frag_selector_matches(s, e))
}

/// A fully sectioned vendor_boot image.
pub struct Image {
    pub hdr: Header,
    pub ramdisk_blob: Vec<u8>,
    pub dtb: Vec<u8>,
    pub table: Vec<RamdiskEntry>,
    pub bootconfig: Vec<u8>,
    /// Trailing bytes after bootconfig (vbmeta + AVB footer on partition
    /// dumps, zeros on padded dumps): carried through verbatim.
    pub footer: Vec<u8>,
}

/// Per-fragment analysis result.
#[derive(Debug)]
pub struct FragInfo {
    pub index: usize,
    pub name: String,
    pub etype: u32,
    pub size: u32,
    pub offset: u32,
    pub kind: BlobKind,
    pub valid: bool,
    pub detail: String,
}

#[derive(Debug)]
pub struct Analysis {
    pub header_version: u32,
    pub page_size: usize,
    pub header_ramdisk_size: u32,
    pub table_sum: u64,
    pub offsets_chain_ok: bool,
    pub frags: Vec<FragInfo>,
    pub dtb_fdts: usize,
    pub bootconfig_len: usize,
    /// Table disagrees with the blob while the blob itself is one stream.
    pub stale_table: bool,
}

impl Image {
    pub fn load(bytes: &[u8]) -> Result<Image> {
        let hdr = Header::parse(bytes)?;
        if hdr.header_version != 3 && hdr.header_version != 4 {
            return Err(Error::Parse(format!(
                "unsupported vendor_boot header version {} (0.1.0 handles v3/v4, Pixel 6 uses v4)",
                hdr.header_version
            )));
        }
        let page = hdr.page_size as usize;
        if page == 0 || page > 65536 || (page & (page - 1)) != 0 {
            return Err(Error::Parse(format!("insane page_size {}", hdr.page_size)));
        }
        if hdr.table_entry_size != 0 && hdr.table_entry_size != 108 {
            return Err(Error::Parse(format!(
                "unsupported ramdisk table entry size {}",
                hdr.table_entry_size
            )));
        }
        let rs = hdr.ramdisk_start()?;
        let ramdisk_end = rs.checked_add(hdr.ramdisk_size as usize).ok_or_else(|| {
            Error::Parse("ramdisk blob end overflows".to_string())
        })?;
        let dtb_start = align_up(ramdisk_end, page);
        let dtb_end = dtb_start.checked_add(hdr.dtb_size as usize).ok_or_else(|| {
            Error::Parse("dtb end overflows".to_string())
        })?;
        let table_off = align_up(dtb_end, page);
        let table_end = table_off.checked_add(hdr.table_size as usize).ok_or_else(|| {
            Error::Parse("ramdisk table end overflows".to_string())
        })?;
        let bootconfig_off = align_up(table_end, page);
        let bootconfig_end = bootconfig_off
            .checked_add(hdr.bootconfig_size as usize)
            .ok_or_else(|| Error::Parse("bootconfig end overflows".to_string()))?;
        if bytes.len() < bootconfig_end {
            return Err(Error::Parse(format!(
                "image truncated: need {} bytes for bootconfig, have {}",
                bootconfig_end,
                bytes.len()
            )));
        }
        if bytes.len() < rs || bytes.len() < ramdisk_end || bytes.len() < dtb_end || bytes.len() < table_end
        {
            return Err(Error::Parse("image truncated inside sections".to_string()));
        }
        let n = hdr.table_entry_num as usize;
        if n > 32 {
            return Err(Error::Parse(format!("insane table entry count {n}")));
        }
        if hdr.table_size != (n as u32).checked_mul(hdr.table_entry_size.max(108)).unwrap_or(u32::MAX) && hdr.table_size != 0
        {
            // Tolerate zero-table images, otherwise require consistency.
            if !(n == 0 && hdr.table_size == 0) {
                return Err(Error::Parse(format!(
                    "table_size {} != entry_num {} * entry_size {}",
                    hdr.table_size, hdr.table_entry_num, hdr.table_entry_size
                )));
            }
        }
        let mut table = Vec::with_capacity(n);
        for i in 0..n {
            let off = table_off + i * 108;
            table.push(RamdiskEntry::parse(&bytes[off..off + 108])?);
        }
        Ok(Image {
            hdr,
            ramdisk_blob: bytes[rs..ramdisk_end].to_vec(),
            dtb: bytes[dtb_start..dtb_end].to_vec(),
            table,
            bootconfig: bytes[bootconfig_off..bootconfig_end].to_vec(),
            // Footer starts at the padded image end (assemble re-pads, so
            // slicing from bootconfig_end would duplicate the pad bytes).
            footer: bytes[align_up(bootconfig_end, page).min(bytes.len())..].to_vec(),
        })
    }

    /// Raw slice of table-claimed fragment `i` inside the blob.
    pub fn frag_bytes(&self, i: usize) -> Result<&[u8]> {
        let e = self.table.get(i).ok_or_else(|| Error::Parse("fragment index out of range".to_string()))?;
        let s = e.offset as usize;
        let en = s.checked_add(e.size as usize).ok_or_else(|| Error::Parse("fragment end overflows".to_string()))?;
        if en > self.ramdisk_blob.len() {
            return Err(Error::Parse(format!(
                "fragment {i} ({} bytes @ {s}) overruns ramdisk blob ({} bytes)",
                e.size,
                self.ramdisk_blob.len()
            )));
        }
        Ok(&self.ramdisk_blob[s..en])
    }
}

/// Tolerant vendor_boot diagnosis for `unpack`: never fails outright,
/// explains every section (what is fine, what is broken and WHY).
#[derive(Debug)]
pub struct FragDiag {
    pub index: usize,
    pub name: String,
    pub etype: u32,
    pub declared_size: u32,
    pub declared_offset: u32,
    pub available: usize,
    pub valid: bool,
    pub stored_format: String,
    pub entries: usize,
    pub why: String,
}

#[derive(Debug)]
pub struct Diagnosis {
    pub header: Option<Header>,
    pub header_note: String,
    pub frags: Vec<FragDiag>,
    pub table_entries_read: usize,
    pub table_declared: usize,
    pub table_why: String,
    pub dtb_declared: u32,
    pub dtb_available: usize,
    pub dtb_fdts: usize,
    pub dtb_why: String,
    pub bootconfig_declared: u32,
    pub bootconfig_available: usize,
    pub whole_blob_single_stream: bool,
    pub whole_blob_entries: usize,
    pub overall_ok: bool,
}

pub fn diagnose(bytes: &[u8]) -> Diagnosis {
    // Fast path: a fully consistent image maps 1:1 from analyze().
    if let Ok(a) = analyze(bytes) {
        let im = Image::load(bytes).unwrap();
        let broken = a.stale_table
            || a.table_sum != a.header_ramdisk_size as u64
            || !a.offsets_chain_ok
            || a.frags.iter().any(|f| !f.valid);
        let mut frags = Vec::new();
        for f in &a.frags {
            let entries = if f.valid {
                match im.frag_bytes(f.index).and_then(blob_entries) {
                    Ok(en) => en.iter().filter(|e| cpio::name_str(e) != "TRAILER!!!").count(),
                    Err(_) => 0,
                }
            } else {
                0
            };
            frags.push(FragDiag {
                index: f.index,
                name: f.name.clone(),
                etype: f.etype,
                declared_size: f.size,
                declared_offset: f.offset,
                available: f.size as usize,
                valid: f.valid,
                stored_format: f.kind.name().to_string(),
                entries,
                why: if f.valid {
                    "ok".to_string()
                } else {
                    f.detail.clone()
                },
            });
        }
        let whole = match lz4legacy::decompress_legacy(&im.ramdisk_blob) {
            Ok(dec) => cpio::parse(&dec).map(|en| en.len()).unwrap_or(0),
            Err(_) => 0,
        };
        return Diagnosis {
            header: Some(im.hdr.clone()),
            header_note: "ok".to_string(),
            frags,
            table_entries_read: im.table.len(),
            table_declared: im.table.len(),
            table_why: if a.stale_table {
                format!(
                    "STALE: table sum {} != header ramdisk_size {} (diff {}); whole blob is one valid stream",
                    a.table_sum,
                    a.header_ramdisk_size,
                    (a.header_ramdisk_size as i64 - a.table_sum as i64).abs()
                )
            } else {
                "ok".to_string()
            },
            dtb_declared: im.hdr.dtb_size,
            dtb_available: im.hdr.dtb_size as usize,
            dtb_fdts: a.dtb_fdts,
            dtb_why: if im.hdr.dtb_size == 0 {
                "absent (dtb_size 0)".to_string()
            } else {
                "ok".to_string()
            },
            bootconfig_declared: im.hdr.bootconfig_size,
            bootconfig_available: im.bootconfig.len(),
            whole_blob_single_stream: a.stale_table || whole > 0,
            whole_blob_entries: whole,
            overall_ok: !broken,
        };
    }

    // Slow path: manual walk over whatever bytes exist.
    let mut d = Diagnosis {
        header: None,
        header_note: "unreadable".to_string(),
        frags: Vec::new(),
        table_entries_read: 0,
        table_declared: 0,
        table_why: "unread".to_string(),
        dtb_declared: 0,
        dtb_available: 0,
        dtb_fdts: 0,
        dtb_why: "unread".to_string(),
        bootconfig_declared: 0,
        bootconfig_available: 0,
        whole_blob_single_stream: false,
        whole_blob_entries: 0,
        overall_ok: false,
    };
    if bytes.len() < 8 || &bytes[0..8] != b"VNDRBOOT" {
        d.header_note = format!(
            "no VNDRBOOT magic (found {:02x?}, need a vendor_boot image)",
            &bytes[..bytes.len().min(8)]
        );
        return d;
    }
    if bytes.len() < HEADER_LEN {
        d.header_note = format!(
            "header truncated: have {} bytes, need {HEADER_LEN}",
            bytes.len()
        );
        return d;
    }
    let hdr = match Header::parse(bytes) {
        Ok(h) => h,
        Err(e) => {
            d.header_note = format!("header corrupt: {e}");
            return d;
        }
    };
    if hdr.header_version != 3 && hdr.header_version != 4 {
        d.header_note = format!("unsupported header version {}", hdr.header_version);
        d.header = Some(hdr);
        return d;
    }
    d.header = Some(hdr.clone());
    d.header_note = "ok".to_string();
    let page = hdr.page_size.max(1) as usize;
    let rs = align_up(hdr.header_size as usize, page);
    let ram_end = rs + hdr.ramdisk_size as usize;
    let dtb_start = align_up(ram_end, page);
    let dtb_end = dtb_start + hdr.dtb_size as usize;
    let table_off = align_up(dtb_end, page);
    let table_end = table_off + hdr.table_size as usize;
    let bc_off = align_up(table_end, page);
    let bc_end = bc_off + hdr.bootconfig_size as usize;

    // Table entries that fully fit.
    let n_decl = hdr.table_entry_num as usize;
    d.table_declared = n_decl;
    let avail_table_bytes = table_end.min(bytes.len()).saturating_sub(table_off.min(bytes.len()));
    let n_fit = (avail_table_bytes / 108).min(n_decl);
    d.table_entries_read = n_fit;
    if table_off > bytes.len() {
        d.table_why = format!("table starts at {table_off:#x}, file ends at {:#x}", bytes.len());
    } else if n_fit < n_decl {
        d.table_why = format!(
            "table truncated: {n_fit}/{n_decl} entries fit (file ends at {:#x})",
            bytes.len()
        );
    } else {
        d.table_why = "ok".to_string();
    }
    let mut table = Vec::new();
    for i in 0..n_fit {
        match RamdiskEntry::parse(&bytes[table_off + i * 108..table_off + i * 108 + 108]) {
            Ok(e) => table.push(e),
            Err(e) => {
                d.table_why = format!("table entry {i} corrupt: {e}");
                break;
            }
        }
    }
    // Fragments against available blob bytes.
    let blob_avail = ram_end.min(bytes.len()).saturating_sub(rs.min(bytes.len()));
    for (i, e) in table.iter().enumerate() {
        let s = e.offset as usize;
        let want = e.size as usize;
        let have = blob_avail.saturating_sub(s.min(blob_avail));
        let have = have.min(want);
        let slice = if s < blob_avail {
            &bytes[rs + s..rs + s + have]
        } else {
            &[][..]
        };
        let (kind, valid, detail) = match check_fragment(slice) {
            (k, v, det, _) => (k, v, det),
        };
        let entries = if valid {
            blob_entries(slice).map(|en| en.len()).unwrap_or(0)
        } else {
            0
        };
        let mut why = if valid && have == want {
            "ok".to_string()
        } else if s >= blob_avail {
            format!("no bytes: offset {s} at/past available blob end {blob_avail}")
        } else if have < want {
            format!("TRUNCATED: have {have}/{want} bytes; {detail}")
        } else {
            detail.clone()
        };
        if (e.offset as u64) + (e.size as u64) > hdr.ramdisk_size as u64 {
            why = format!("declared range exceeds ramdisk_size {}: {why}", hdr.ramdisk_size);
        }
        d.frags.push(FragDiag {
            index: i,
            name: e.name_str(),
            etype: e.entry_type,
            declared_size: e.size,
            declared_offset: e.offset,
            available: have,
            valid: valid && have == want,
            stored_format: kind.name().to_string(),
            entries,
            why,
        });
    }
    if ram_end > bytes.len() {
        d.frags.push(FragDiag {
            index: usize::MAX,
            name: String::new(),
            etype: 0,
            declared_size: 0,
            declared_offset: 0,
            available: 0,
            valid: false,
            stored_format: String::new(),
            entries: 0,
            why: format!(
                "ramdisk blob truncated: header ends it at {ram_end:#x}, file ends at {:#x}",
                bytes.len()
            ),
        });
    }
    // DTB on available bytes (size 0 = absent, legal).
    d.dtb_declared = hdr.dtb_size;
    d.dtb_available = dtb_end.min(bytes.len()).saturating_sub(dtb_start.min(bytes.len()));
    if hdr.dtb_size == 0 {
        d.dtb_fdts = 0;
        d.dtb_why = "absent (dtb_size 0)".to_string();
    } else if dtb_start >= bytes.len() {
        d.dtb_why = "no bytes: dtb starts past file end".to_string();
    } else if d.dtb_available < hdr.dtb_size as usize {
        d.dtb_why = format!(
            "TRUNCATED: have {}/{:?} bytes",
            d.dtb_available, hdr.dtb_size
        );
    } else {
        match dtb::verify(&bytes[dtb_start..dtb_end], hdr.dtb_size as usize) {
            Ok(n) => {
                d.dtb_fdts = n;
                d.dtb_why = "ok".to_string();
            }
            Err(e) => d.dtb_why = format!("INVALID: {e}"),
        }
    }
    d.bootconfig_declared = hdr.bootconfig_size;
    d.bootconfig_available = bc_end.min(bytes.len()).saturating_sub(bc_off.min(bytes.len()));
    // Whole-blob rescue probe (only meaningful when the full blob is here).
    if ram_end <= bytes.len() {
        let blob = &bytes[rs..ram_end];
        if let Ok(dec) = lz4legacy::decompress_legacy(blob) {
            if let Ok(en) = cpio::parse(&dec) {
                d.whole_blob_single_stream = true;
                d.whole_blob_entries = en.len();
            }
        }
    }
    let dtb_ok = d.dtb_why == "ok" || d.dtb_why.starts_with("absent");
    d.overall_ok = d.frags.iter().all(|f| f.valid)
        && d.table_why == "ok"
        && dtb_ok
        && d.bootconfig_available == d.bootconfig_declared as usize
        && ram_end <= bytes.len()
        && bc_end <= bytes.len();
    d
}

fn check_fragment(blob: &[u8]) -> (BlobKind, bool, String, Option<Vec<u8>>) {
    match lz4legacy::sniff(blob) {
        BlobKind::Lz4Legacy => match lz4legacy::decompress_legacy(blob) {
            Ok(dec) => match cpio::verify_blob(&dec) {
                Ok(n) => {
                    let blocks = lz4legacy::walk_blocks(blob).map(|(b, _)| b).unwrap_or(0);
                    (
                        BlobKind::Lz4Legacy,
                        true,
                        format!("lz4_legacy ({blocks} blocks) decompresses to cpio with {n} entries"),
                        Some(dec),
                    )
                }
                Err(e) => (BlobKind::Lz4Legacy, false, format!("lz4 ok but cpio bad: {e}"), None),
            },
            Err(e) => (BlobKind::Lz4Legacy, false, format!("lz4 decompress fails: {e}"), None),
        },
        BlobKind::Cpio => match cpio::verify_blob(blob) {
            Ok(n) => (BlobKind::Cpio, true, format!("raw cpio with {n} entries"), None),
            Err(e) => (BlobKind::Cpio, false, format!("raw cpio bad: {e}"), None),
        },
        // Anything else: try the shared codecs (gzip/xz/lzma/lz4-frame).
        _ => {
            let kind = match codec::sniff(blob) {
                Format::Gzip => BlobKind::Gzip,
                Format::Xz => BlobKind::Xz,
                Format::Lzma => BlobKind::Lzma,
                Format::Lz4Frame => BlobKind::Lz4Frame,
                _ => return (BlobKind::Unknown, false, "unknown fragment format".to_string(), None),
            };
            match codec::decompress(codec::sniff(blob), blob) {
                Ok(dec) => match cpio::verify_blob(&dec) {
                    Ok(n) => (
                        kind,
                        true,
                        format!("{} decompresses to cpio with {n} entries", kind.name()),
                        Some(dec),
                    ),
                    Err(e) => (kind, false, format!("{} ok but cpio bad: {e}", kind.name()), None),
                },
                Err(e) => (kind, false, format!("{} decompress fails: {e}", kind.name()), None),
            }
        }
    }
}

pub fn analyze(img: &[u8]) -> Result<Analysis> {
    let im = Image::load(img)?;
    let mut frags = Vec::new();
    let mut sum: u64 = 0;
    let mut chain_ok = true;
    let mut running: u64 = 0;
    for (i, e) in im.table.iter().enumerate() {
        sum += e.size as u64;
        if e.offset as u64 != running {
            chain_ok = false;
        }
        running += e.size as u64;
        let (kind, valid, detail) = match im.frag_bytes(i) {
            Ok(b) => {
                let (k, v, d, _) = check_fragment(b);
                (k, v, d)
            }
            Err(e) => (BlobKind::Unknown, false, format!("slice error: {e}")),
        };
        frags.push(FragInfo {
            index: i,
            name: e.name_str(),
            etype: e.entry_type,
            size: e.size,
            offset: e.offset,
            kind,
            valid,
            detail,
        });
    }
    let fdts = dtb::verify(&im.dtb, im.hdr.dtb_size as usize)?;
    // Single-stream test on the whole blob (broken-maintainer case).
    let single = match lz4legacy::decompress_legacy(&im.ramdisk_blob) {
        Ok(dec) => cpio::verify_blob(&dec).is_ok(),
        Err(_) => false,
    };
    let stale = single
        && (sum != im.hdr.ramdisk_size as u64 || frags.iter().any(|f| !f.valid));
    Ok(Analysis {
        header_version: im.hdr.header_version,
        page_size: im.hdr.page_size as usize,
        header_ramdisk_size: im.hdr.ramdisk_size,
        table_sum: sum,
        offsets_chain_ok: chain_ok,
        frags,
        dtb_fdts: fdts,
        bootconfig_len: im.bootconfig.len(),
        stale_table: stale,
    })
}

/// Normalize a user-supplied platform file into raw fragment bytes:
/// already-compressed blobs (lz4-legacy, gzip, xz, lzma, lz4-frame) stay
/// verbatim after validation, raw cpio gets compressed to lz4-legacy.
fn normalize_input(path: &str, data: &[u8]) -> Result<Vec<u8>> {
    match lz4legacy::sniff(data) {
        BlobKind::Lz4Legacy => {
            let dec = lz4legacy::decompress_legacy(data).map_err(|e| {
                Error::Parse(format!("platform file {path} is broken lz4: {e}"))
            })?;
            cpio::verify_blob(&dec).map_err(|e| {
                Error::Parse(format!("platform file {path} decompresses but cpio is bad: {e}"))
            })?;
            Ok(data.to_vec())
        }
        BlobKind::Cpio => {
            cpio::verify_blob(data).map_err(|e| {
                Error::Parse(format!("platform file {path} is a bad cpio: {e}"))
            })?;
            Ok(lz4legacy::compress_legacy(data))
        }
        _ => match codec::sniff(data) {
            Format::Gzip | Format::Xz | Format::Lzma | Format::Lz4Frame => {
                let fmt = codec::sniff(data);
                let dec = codec::decompress(fmt, data).map_err(|e| {
                    Error::Parse(format!("platform file {path} is broken {}: {e}", fmt.name()))
                })?;
                cpio::verify_blob(&dec).map_err(|e| {
                    Error::Parse(format!("platform file {path} decompresses but cpio is bad: {e}"))
                })?;
                Ok(data.to_vec())
            }
            _ => Err(Error::Parse(format!(
                "platform file {path}: neither lz4-legacy, cpio, gzip, xz, lzma nor lz4 (need platform.cpio or a compressed ramdisk)"
            ))),
        },
    }
}

/// Assemble an image from a header template + fragments + dtb + bootconfig
/// + footer (footer goes last, unpadded, exactly like partition dumps).
pub(crate) fn assemble(
    hdr: &Header,
    frags: &[Vec<u8>],
    entries: &[RamdiskEntry],
    dtb: &[u8],
    bootconfig: &[u8],
    footer: &[u8],
) -> Vec<u8> {
    let page = hdr.page_size as usize;
    let mut out = Vec::new();
    let h = hdr.serialize();
    out.extend_from_slice(&h);
    out.extend_from_slice(&vec![0u8; align_up(HEADER_LEN, page) - HEADER_LEN]);
    for f in frags {
        out.extend_from_slice(f);
    }
    out.extend_from_slice(&vec![0u8; (align_up(out.len(), page) - out.len()) % page.max(1)]);
    out.extend_from_slice(dtb);
    out.extend_from_slice(&vec![0u8; (align_up(out.len(), page) - out.len()) % page.max(1)]);
    for e in entries {
        out.extend_from_slice(&e.serialize());
    }
    out.extend_from_slice(&vec![0u8; (align_up(out.len(), page) - out.len()) % page.max(1)]);
    out.extend_from_slice(bootconfig);
    out.extend_from_slice(&vec![0u8; (align_up(out.len(), page) - out.len()) % page.max(1)]);
    out.extend_from_slice(footer);
    out
}

/// Verify a freshly assembled image fully in memory before emitting.
pub fn verify_image(bytes: &[u8]) -> Result<()> {
    let im = Image::load(bytes).map_err(|e| Error::Verify(format!("rebuilt image does not parse: {e}")))?;
    if im.table.is_empty() {
        return Err(Error::Verify(
            "rebuilt image has no ramdisk fragments, refusing to emit an empty image".to_string(),
        ));
    }
    let mut sum: u64 = 0;
    let mut running: u64 = 0;
    for (i, e) in im.table.iter().enumerate() {
        sum += e.size as u64;
        if e.offset as u64 != running {
            return Err(Error::Verify(format!("rebuilt table offsets do not chain at fragment {i}")));
        }
        running += e.size as u64;
        let b = im.frag_bytes(i).map_err(|e| Error::Verify(format!("rebuilt fragment {i} out of range: {e}")))?;
        let (_, valid, detail, _) = check_fragment(b);
        if !valid {
            return Err(Error::Verify(format!("rebuilt fragment {i} invalid: {detail}")));
        }
        let _ = type_name(e.entry_type);
    }
    if sum != im.hdr.ramdisk_size as u64 {
        return Err(Error::Verify(format!(
            "rebuilt header ramdisk_size {} != table sum {sum}",
            im.hdr.ramdisk_size
        )));
    }
    dtb::verify(&im.dtb, im.hdr.dtb_size as usize)?;
    if im.bootconfig.len() != im.hdr.bootconfig_size as usize {
        return Err(Error::Verify("rebuilt bootconfig length mismatch".to_string()));
    }
    Ok(())
}

/// Decompress one fragment blob (lz4-legacy, raw cpio or another supported
/// codec) into entries.
fn blob_entries(blob: &[u8]) -> Result<Vec<Entry>> {
    match lz4legacy::sniff(blob) {
        BlobKind::Lz4Legacy => {
            let dec = lz4legacy::decompress_legacy(blob)?;
            cpio::parse(&dec)
        }
        BlobKind::Cpio => cpio::parse(blob),
        _ => match codec::sniff(blob) {
            Format::Gzip | Format::Xz | Format::Lzma | Format::Lz4Frame => {
                let dec = codec::decompress(codec::sniff(blob), blob)?;
                cpio::parse(&dec)
            }
            _ => Err(Error::Parse(
                "fragment is in an unsupported format (need lz4-legacy, cpio, gzip, xz, lzma or lz4)"
                    .to_string(),
            )),
        },
    }
}

/// Entries of every kept original fragment, plus a flag telling whether
/// the kept set is usable per-fragment. With no `--drop` selector this is
/// exactly the old behavior (chained absolute offsets, matching sum);
/// with dropped fragments the survivors are judged on their own (each
/// readable — offsets still address the original blob, so chaining
/// against removed neighbors is meaningless).
struct OrigContent {
    frags: Vec<(RamdiskEntry, Vec<Entry>)>,
    table_ok: bool,
}

fn orig_content(im: &Image, kept: &[bool]) -> OrigContent {
    let full = kept.len() == im.table.len() && kept.iter().all(|&k| k);
    if !full {
        let mut frags = Vec::new();
        let mut ok = false;
        for (i, e) in im.table.iter().enumerate() {
            if !kept.get(i).copied().unwrap_or(false) {
                continue;
            }
            match im.frag_bytes(i).and_then(blob_entries) {
                Ok(en) => {
                    ok = true;
                    frags.push((e.clone(), en));
                }
                Err(_) => {
                    // Mirror the all-or-nothing rule below: one unreadable
                    // survivor poisons the per-fragment view (the rescue
                    // path warns that fragment selectors do not apply).
                    frags.clear();
                    ok = false;
                    break;
                }
            }
        }
        if !kept.iter().any(|&k| k) {
            ok = false;
        }
        return OrigContent { frags, table_ok: ok };
    }
    let mut frags = Vec::new();
    let mut ok = !im.table.is_empty();
    let mut run: u64 = 0;
    let mut sum: u64 = 0;
    for (i, e) in im.table.iter().enumerate() {
        sum += e.size as u64;
        if e.offset as u64 != run {
            ok = false;
        }
        run += e.size as u64;
        match im.frag_bytes(i).and_then(blob_entries) {
            Ok(en) => frags.push((e.clone(), en)),
            Err(_) => ok = false,
        }
    }
    ok &= sum == im.hdr.ramdisk_size as u64;
    // If any fragment failed, the per-fragment view is unusable as a set
    // (the stale-table rescue below may still apply to the whole blob).
    if frags.len() != im.table.len() {
        frags.clear();
    }
    OrigContent { frags, table_ok: ok }
}

/// Whole-blob rescue for the stale-table case: the blob is one legacy
/// stream even though the table disagrees.
fn whole_blob_entries(im: &Image) -> Result<Vec<Entry>> {
    blob_entries(&im.ramdisk_blob)
        .map_err(|e| Error::Parse(format!("content unusable per-fragment and not one stream: {e}")))
}

/// Inbuild first-stage entries: `first_stage_ramdisk/**` from the readable
/// original platform fragments (whole-blob rescue when the table is
/// stale). Dropped fragments never contribute.
fn base_first_stage(im: &Image, kept: &[bool], use_frags: bool) -> Vec<Entry> {
    let mut pool = Vec::new();
    if use_frags {
        for (i, e) in im.table.iter().enumerate() {
            if !kept.get(i).copied().unwrap_or(false) || e.entry_type != TYPE_PLATFORM {
                continue;
            }
            if let Ok(en) = im.frag_bytes(i).and_then(blob_entries) {
                pool.extend(en);
            }
        }
    } else if let Ok(en) = whole_blob_entries(im) {
        pool = en;
    }
    pool.into_iter()
        .filter(|e| cpio::is_first_stage_path(&cpio::name_str(e)))
        .collect()
}

/// Split entries into (non-first-stage, dropped-first-stage-count).
fn strip_first_stage(entries: Vec<Entry>) -> (Vec<Entry>, usize) {
    let before = entries.len();
    let kept: Vec<Entry> = entries
        .into_iter()
        .filter(|e| !cpio::is_first_stage_path(&cpio::name_str(e)))
        .collect();
    let n = before - kept.len();
    (kept, n)
}

/// First-stage rule for a passed platform cpio (`new_entries`).
/// Default: the base image's own first-stage (inbuild) wins — those
/// entries go first, the cpio's first-stage entries are dropped along the
/// way. With `first-stage` in `--drop` the rule inverts: the cpio is used
/// as-is and the inbuild first-stage is dropped.
/// Returns the entries plus whether they were rebuilt (the caller must
/// re-encode instead of reusing the passed bytes verbatim).
fn apply_first_stage(
    im: &Image,
    kept: &[bool],
    use_frags: bool,
    drops: &DropSet,
    label: &str,
    new_entries: Vec<Entry>,
) -> (Vec<Entry>, bool) {
    let new_fs =
        new_entries.iter().filter(|e| cpio::is_first_stage_path(&cpio::name_str(e))).count();
    if drops.first_stage {
        if new_fs == 0 {
            eprintln!(
                "warning: inbuild first-stage dropped (--drop first-stage) but {label} has none; result platform has no first-stage"
            );
        }
        return (new_entries, false);
    }
    let base_fs = base_first_stage(im, kept, use_frags);
    if base_fs.is_empty() {
        eprintln!("warning: no readable inbuild first-stage, using {label} as-is");
        return (new_entries, false);
    }
    let (stripped, n) = strip_first_stage(new_entries);
    if n > 0 {
        eprintln!(
            "kept inbuild first-stage ({} entries), dropped {n} first-stage entries from {label}",
            base_fs.len()
        );
    } else {
        eprintln!("kept inbuild first-stage ({} entries), {label} has none", base_fs.len());
    }
    let mut out = base_fs;
    out.extend(stripped);
    (out, true)
}

fn parse_u32(v: &str, key: &str) -> Result<u32> {
    let t = v.trim();
    let (radix, digits) = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (16, h)
    } else {
        (10, t)
    };
    u32::from_str_radix(digits, radix)
        .map_err(|_| Error::Usage(format!("--set {key} needs an integer, got '{v}'")))
}

fn parse_u64(v: &str, key: &str) -> Result<u64> {
    let t = v.trim();
    let (radix, digits) = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (16, h)
    } else {
        (10, t)
    };
    u64::from_str_radix(digits, radix)
        .map_err(|_| Error::Usage(format!("--set {key} needs an integer, got '{v}'")))
}

fn fixed_bytes(b: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let n = b.len().min(len);
    out[..n].copy_from_slice(&b[..n]);
    out
}

/// Apply `--set k=v` header overrides (vendor_boot key set, same as the
/// repack subprogram). Runs after the layout is decided, so it cannot
/// break offsets — except page_size, which re-lays the whole image out
/// (assemble aligns by it, verify confirms the result).
fn apply_sets(hdr: &mut Header, sets: &[(String, String)]) -> Result<()> {
    for (k, v) in sets {
        match k.as_str() {
            "cmdline" => hdr.cmdline = fixed_bytes(v.as_bytes(), 2048).try_into().unwrap(),
            "name" => hdr.name = fixed_bytes(v.as_bytes(), 16).try_into().unwrap(),
            "page_size" => hdr.page_size = parse_u32(v, k)?,
            "kernel_addr" => hdr.kernel_addr = parse_u32(v, k)?,
            "ramdisk_addr" => hdr.ramdisk_addr = parse_u32(v, k)?,
            "tags_addr" => hdr.tags_addr = parse_u32(v, k)?,
            "dtb_addr" => hdr.dtb_addr = parse_u64(v, k)?,
            "os_version" | "os_patch_level" => {
                return Err(Error::Usage(format!("--set {k} is boot-only (vendor_boot has no os_version)")))
            }
            _ => return Err(Error::Usage(format!("--set: unknown key '{k}'"))),
        }
    }
    Ok(())
}

/// Rechain offsets over a fragment set (needed when `--drop` removed
/// entries: survivors keep their bytes, offsets close ranks).
fn rechain(frags: Vec<Vec<u8>>, mut table: Vec<RamdiskEntry>) -> (Vec<Vec<u8>>, Vec<RamdiskEntry>) {
    let mut off: u64 = 0;
    for (raw, e) in frags.iter().zip(table.iter_mut()) {
        e.offset = off as u32;
        e.size = raw.len() as u32;
        off += raw.len() as u64;
    }
    (frags, table)
}

/// Drop first-stage entries out of platform fragments, re-encoding them
/// marker-free. Non-platform fragments pass through verbatim.
fn strip_platforms(
    frags: Vec<Vec<u8>>,
    table: Vec<RamdiskEntry>,
) -> Result<(Vec<Vec<u8>>, Vec<RamdiskEntry>)> {
    let mut f = Vec::with_capacity(frags.len());
    let mut t = Vec::with_capacity(table.len());
    for (raw, e) in frags.into_iter().zip(table) {
        if e.entry_type == TYPE_PLATFORM {
            let (stripped, n) = strip_first_stage(blob_entries(&raw)?);
            if n > 0 {
                eprintln!("dropped {n} first-stage entries from a platform fragment (--drop first-stage)");
            }
            f.push(lz4legacy::compress_legacy(&cpio::build(&stripped)));
        } else {
            f.push(raw);
        }
        t.push(e);
    }
    Ok(rechain(f, t))
}

/// board_id of the first kept original fragment of `etype` (zeros when
/// none): freshly encoded fragments inherit it so re-encoded tables keep
/// the device binding instead of zeroing it out.
fn board_id_of(im: &Image, kept: &[bool], etype: u32) -> [u8; 64] {
    im.table
        .iter()
        .enumerate()
        .find(|(i, e)| e.entry_type == etype && kept.get(*i).copied().unwrap_or(false))
        .map(|(_, e)| e.board_id)
        .unwrap_or([0u8; 64])
}

/// Raw bytes + table entry for one freshly encoded fragment.
fn encode_fragment(
    entries: &[Entry],
    etype: u32,
    offset: u32,
    board_id: [u8; 64],
) -> (Vec<u8>, RamdiskEntry) {
    let raw = lz4legacy::compress_legacy(&cpio::build(entries));
    let mut entry = match etype {
        TYPE_DLKM => RamdiskEntry::dlkm(0, 0),
        TYPE_RECOVERY => {
            let mut n = [0u8; 32];
            n[0..8].copy_from_slice(b"recovery");
            RamdiskEntry { size: 0, offset: 0, entry_type: TYPE_RECOVERY, name: n, board_id: [0u8; 64] }
        }
        _ => RamdiskEntry::platform(0),
    };
    entry.size = raw.len() as u32;
    entry.offset = offset;
    entry.board_id = board_id;
    (raw, entry)
}

/// Build platform/recovery/dlkm fragments from an entry pool.
/// `verbatim_dlkm`: valid original dlkm (entry + bytes), used only when
/// the pool yields no dlkm payload of its own. Fresh entries inherit the
/// board_id of the kept originals of the same type.
fn split_entries(
    entries: Vec<Entry>,
    verbatim_dlkm: Option<(RamdiskEntry, Vec<u8>)>,
    im: &Image,
    kept: &[bool],
) -> Result<(Vec<Vec<u8>>, Vec<RamdiskEntry>)> {
    let (plat, rec, dlkm) = cpio::partition(&entries);
    if plat.is_empty() {
        return Err(Error::Parse(
            "split leaves platform empty (content is only lib/recovery?)".to_string(),
        ));
    }
    let mut frags = Vec::new();
    let mut table = Vec::new();
    let (plat_raw, e0) = encode_fragment(&plat, TYPE_PLATFORM, 0, board_id_of(im, kept, TYPE_PLATFORM));
    let mut off = plat_raw.len() as u32;
    frags.push(plat_raw);
    table.push(e0);
    if cpio::has_payload(&rec) {
        let (rec_raw, e) = encode_fragment(&rec, TYPE_RECOVERY, off, board_id_of(im, kept, TYPE_RECOVERY));
        off += rec_raw.len() as u32;
        frags.push(rec_raw);
        table.push(e);
    }
    if cpio::has_payload(&dlkm) {
        let (dlkm_raw, e) = encode_fragment(&dlkm, TYPE_DLKM, off, board_id_of(im, kept, TYPE_DLKM));
        frags.push(dlkm_raw);
        table.push(e);
    } else if let Some((mut e, raw)) = verbatim_dlkm {
        e.offset = off;
        e.size = raw.len() as u32;
        frags.push(raw);
        table.push(e);
    }
    Ok((frags, table))
}

/// Valid original fragments to carry over verbatim when the platform is
/// replaced: everything that is not platform (it gets replaced) and not
/// already produced by the new layout (tracked as (type, name) pairs).
/// This is what keeps foreign fragments like shiba's "16K" (type none,
/// 16K-page kernel modules) or a recovery fragment alive across a
/// platform swap instead of silently dropping 17 MB of modules.
/// Fragments named in `--drop` are skipped; unreadable originals are
/// skipped with a stderr note, never propagated.
fn carryovers(im: &Image, produced: &[(u32, String)], kept: &[bool]) -> Vec<(RamdiskEntry, Vec<u8>)> {
    let mut out = Vec::new();
    for (i, e) in im.table.iter().enumerate() {
        if e.entry_type == TYPE_PLATFORM {
            continue;
        }
        if !kept.get(i).copied().unwrap_or(false) {
            continue;
        }
        if produced.iter().any(|(t, n)| *t == e.entry_type && *n == e.name_str()) {
            continue;
        }
        match im.frag_bytes(i) {
            Ok(b) if check_fragment(b).1 => {
                let mut ne = e.clone();
                ne.size = b.len() as u32;
                ne.offset = 0; // rechained by the caller
                out.push((ne, b.to_vec()));
            }
            _ => eprintln!(
                "warning: original {} fragment {:?} unreadable, not carried over",
                type_name(e.entry_type),
                e.name_str()
            ),
        }
    }
    out
}

/// Append carryovers to a (frags, table) pair, rechaining offsets.
fn append_carryovers(
    frags: &mut Vec<Vec<u8>>,
    table: &mut Vec<RamdiskEntry>,
    im: &Image,
    produced: &[(u32, String)],
    kept: &[bool],
) {
    let mut off: u32 = frags.iter().map(|f| f.len() as u32).sum();
    for (mut e, raw) in carryovers(im, produced, kept) {
        e.offset = off;
        off += raw.len() as u32;
        eprintln!(
            "carried over {} fragment {:?} verbatim ({} bytes)",
            type_name(e.entry_type),
            e.name_str(),
            raw.len()
        );
        frags.push(raw);
        table.push(e);
    }
}

/// Recovery-install layout for `--recovery <fox>`: detach the vendor blobs
/// from recovery and install the passed ramdisk as the recovery fragment.
///
/// Output fragments, in order:
/// - platform = native `first_stage_ramdisk/**` entries ONLY, harvested
///   from the whole kept pool (wherever they live). The harvest happens
///   BEFORE the old recovery is dropped, so an old recovery holding
///   first-stage can never brick the device when it is replaced.
/// - recovery = the passed file minus first-stage duplicates (verbatim
///   bytes when it has none, re-encoded otherwise).
/// - dlkm = a valid original dlkm verbatim; else `lib/**` pulled out of
///   the pool into a fresh dlkm; else nothing.
/// - any other valid original non-platform fragment (16K, ...) carries
///   over verbatim. Old recovery-type originals are always replaced,
///   never carried over.
///
/// Refuses (exit 2 through the CLI) when the kept base has no first-stage
/// (the platform would be empty) or the fox file yields no recovery
/// payload.
fn build_recovery(
    im: &Image,
    kept: &[bool],
    drops: &DropSet,
    orig: &OrigContent,
    orig_valid_dlkm: Option<(RamdiskEntry, Vec<u8>)>,
    label: &str,
    data: &[u8],
) -> Result<(Vec<Vec<u8>>, Vec<RamdiskEntry>)> {
    // Entry pool: per-fragment when the kept set is usable, whole-blob
    // rescue otherwise (stale-table base). attributed with its fragment
    // type so first-stage sources can be reported.
    let src: Vec<(u32, Vec<Entry>)> = if orig.table_ok {
        orig.frags.iter().map(|(e, en)| (e.entry_type, en.clone())).collect()
    } else {
        if !drops.frags.is_empty() {
            eprintln!(
                "warning: table unusable, --drop fragment selectors have no effect on whole-blob rescue"
            );
        }
        vec![(TYPE_PLATFORM, whole_blob_entries(im)?)]
    };
    let mut pool: Vec<Entry> = Vec::new();
    let mut fs_entries: Vec<Entry> = Vec::new();
    let (mut fs_plat, mut fs_other) = (0usize, 0usize);
    for (etype, en) in &src {
        for e in en {
            if cpio::name_str(e) == "TRAILER!!!" {
                continue;
            }
            pool.push(e.clone());
            if cpio::is_first_stage_path(&cpio::name_str(e)) {
                if *etype == TYPE_PLATFORM {
                    fs_plat += 1;
                } else {
                    fs_other += 1;
                }
                fs_entries.push(e.clone());
            }
        }
    }
    if drops.first_stage {
        let (stripped, n) = strip_first_stage(fs_entries);
        if n > 0 {
            eprintln!("dropped {n} first-stage entries from the base pool (--drop first-stage)");
        }
        fs_entries = stripped;
    }
    if fs_entries.is_empty() {
        return Err(Error::Parse(
            "refusing recovery install: no first_stage_ramdisk/** in the kept base content (platform would be empty)".to_string(),
        ));
    }
    eprintln!(
        "first-stage: {} entries ({} from platform, {} from other fragments) -> new platform",
        fs_entries.len(),
        fs_plat,
        fs_other,
    );
    if fs_other > 0 {
        eprintln!(
            "warning: first-stage harvested from non-platform fragments (old recovery); it moves to platform so replacing recovery stays safe"
        );
    }
    // Recovery payload: validated like a platform file (compressed stays
    // verbatim, raw cpio gets compressed), first-stage duplicates out.
    let fox_raw = normalize_input(label, data)?;
    let fox_entries = blob_entries(&fox_raw)
        .map_err(|e| Error::Parse(format!("internal error re-reading recovery file: {e}")))?;
    let (fox_stripped, n) = strip_first_stage(fox_entries);
    if n > 0 {
        eprintln!("dropped {n} first-stage duplicates from {label} (inbuild first-stage wins)");
    }
    let fox_rec = cpio::drop_trailers(&fox_stripped);
    if fox_rec.is_empty() {
        return Err(Error::Parse(format!(
            "refusing recovery install: {label} yields no recovery payload"
        )));
    }
    // Verbatim bytes when nothing was stripped, re-encoded otherwise.
    let fox_bytes =
        if n == 0 { fox_raw } else { lz4legacy::compress_legacy(&cpio::build(&fox_rec)) };
    // board_id: first original recovery entry's, else the platform's.
    let rec_bid = im
        .table
        .iter()
        .find(|e| e.entry_type == TYPE_RECOVERY)
        .map(|e| e.board_id)
        .unwrap_or_else(|| board_id_of(im, kept, TYPE_PLATFORM));
    let mut frags: Vec<Vec<u8>> = Vec::new();
    let mut table: Vec<RamdiskEntry> = Vec::new();
    let (plat_raw, e0) = encode_fragment(&fs_entries, TYPE_PLATFORM, 0, board_id_of(im, kept, TYPE_PLATFORM));
    let mut off = plat_raw.len() as u32;
    eprintln!("platform: {} first-stage entries ({} bytes)", fs_entries.len(), plat_raw.len());
    frags.push(plat_raw);
    table.push(e0);
    let mut er_name = [0u8; 32];
    er_name[0..8].copy_from_slice(b"recovery");
    let er = RamdiskEntry {
        size: fox_bytes.len() as u32,
        offset: off,
        entry_type: TYPE_RECOVERY,
        name: er_name,
        board_id: rec_bid,
    };
    off += fox_bytes.len() as u32;
    eprintln!("recovery: {} entries from {label} ({} bytes)", fox_rec.len(), fox_bytes.len());
    frags.push(fox_bytes);
    table.push(er);
    // produced[] tracks (type, name) for the carryover filter; every kept
    // original recovery fragment is listed so none survives the replace.
    let mut produced: Vec<(u32, String)> = vec![(TYPE_PLATFORM, String::new())];
    for (i, e) in im.table.iter().enumerate() {
        if e.entry_type == TYPE_RECOVERY && kept.get(i).copied().unwrap_or(false) {
            produced.push((TYPE_RECOVERY, e.name_str()));
        }
    }
    let (_, _, pool_lib) = cpio::partition(&pool);
    if let Some((mut e, raw)) = orig_valid_dlkm {
        e.offset = off;
        e.size = raw.len() as u32;
        eprintln!("dlkm: kept original {:?} verbatim ({} bytes)", e.name_str(), raw.len());
        frags.push(raw);
        table.push(e.clone());
        produced.push((TYPE_DLKM, e.name_str()));
    } else if cpio::has_payload(&pool_lib) {
        let (dlkm_raw, e) = encode_fragment(&pool_lib, TYPE_DLKM, off, board_id_of(im, kept, TYPE_DLKM));
        let n_lib = pool_lib.iter().filter(|e| cpio::name_str(e) != "TRAILER!!!").count();
        eprintln!("dlkm: pulled {n_lib} lib/** entries out of the base pool ({} bytes)", dlkm_raw.len());
        frags.push(dlkm_raw);
        table.push(e);
        produced.push((TYPE_DLKM, "dlkm".to_string()));
    } else if pool.iter().any(|e| cpio::is_dlkm_path(&cpio::name_str(e))) {
        eprintln!("note: base pool holds lib/** dir entries only (no payload), no dlkm emitted");
    }
    append_carryovers(&mut frags, &mut table, im, &produced, kept);
    Ok((frags, table))
}

/// Main repack routine.
///
/// - `platform`: optional (path label, bytes) replacement content.
/// - `mode`: Keep (round-trip verbatim / normalize stale table),
///   Split (partition into platform/recovery/dlkm by subtree),
///   Merge (glue everything into one platform fragment).
/// - `drop` (`RepackOpts::drop`): `--drop` selectors — original fragments
///   matched by table type or name are left out in every mode (Keep
///   skips them and rechains, Split/Merge exclude them from the pool,
///   carryover and the verbatim dlkm fallback); the special `first-stage`
///   inverts the first-stage rule (see [`apply_first_stage`).
/// - `recovery` (`RepackOpts::recovery`): `--recovery <file>` payload —
///   recovery-install layout (see [`build_recovery`]); conflicts with a
///   platform file and with Split/Merge.
/// No-drop shorthand over [`repack_with_opts`] (kept for callers that
/// only select a layout mode).
#[allow(dead_code)]
pub fn repack(orig_bytes: &[u8], platform: Option<(&str, Vec<u8>)>, mode: Mode) -> Result<Vec<u8>> {
    repack_with_opts(
        orig_bytes,
        platform,
        RepackOpts { mode, drop: Vec::new(), sets: Vec::new(), recovery: None, drop_footer: false },
    )
}

pub fn repack_with_opts(
    orig_bytes: &[u8],
    platform: Option<(&str, Vec<u8>)>,
    opts: RepackOpts,
) -> Result<Vec<u8>> {
    let mode = opts.mode;
    let im = Image::load(orig_bytes)?;
    let drops = parse_drop(&opts.drop);
    let kept: Vec<bool> = im.table.iter().map(|e| !frag_dropped(&drops, e)).collect();
    for s in &drops.frags {
        if !im.table.iter().any(|e| frag_selector_matches(s, e)) {
            eprintln!("warning: --drop {s}: matched no fragment, ignored");
        }
    }
    if !im.table.is_empty() && !kept.iter().any(|&k| k) {
        return Err(Error::Usage(
            "--drop removes every fragment, nothing left to build".to_string(),
        ));
    }
    // Whole-blob rescue cannot honor fragment selectors (there are no
    // fragment boundaries in a single stream): warn when it is taken.
    let rescue_note = || {
        if !drops.frags.is_empty() {
            eprintln!(
                "warning: table unusable, --drop fragment selectors have no effect on whole-blob rescue"
            );
        }
    };
    let orig = orig_content(&im, &kept);
    // Valid original dlkm as (entry, bytes) fallback: the entry carries
    // the original board_id/name, the caller only refreshes offset/size.
    let orig_valid_dlkm: Option<(RamdiskEntry, Vec<u8>)> = {
        let mut out = None;
        for (i, e) in im.table.iter().enumerate() {
            if e.entry_type != TYPE_DLKM || !kept.get(i).copied().unwrap_or(false) {
                continue;
            }
            if let Ok(b) = im.frag_bytes(i) {
                if check_fragment(b).1 {
                    let mut ne = e.clone();
                    ne.size = b.len() as u32;
                    out = Some((ne, b.to_vec()));
                }
            }
        }
        out
    };

    // Entry pool for Split/Merge without a platform file: per-fragment
    // content when the kept set is usable, whole-blob rescue otherwise.
    let pooled_all = || -> Result<Vec<Entry>> {
        let mut en = if orig.table_ok {
            orig.frags.iter().flat_map(|(_, en)| en.clone()).collect()
        } else {
            rescue_note();
            whole_blob_entries(&im)?
        };
        if drops.first_stage {
            let (stripped, n) = strip_first_stage(en);
            if n > 0 {
                eprintln!("dropped {n} first-stage entries from the pool (--drop first-stage)");
            }
            en = stripped;
        }
        Ok(en)
    };
    // Valid non-platform original entries (dlkm/recovery) for the Merge
    // union. Unreadable ones are skipped with a stderr note: Merge must
    // never die on the stale-table garbage it replaces.
    let pooled_non_platform = || -> Vec<Entry> {
        let mut out = Vec::new();
        for (i, e) in im.table.iter().enumerate() {
            if e.entry_type == TYPE_PLATFORM || !kept.get(i).copied().unwrap_or(false) {
                continue;
            }
            if let Ok(b) = im.frag_bytes(i) {
                match blob_entries(b) {
                    Ok(en) => out.extend(en),
                    Err(_) => eprintln!(
                        "warning: original {} fragment unreadable, left out of the merge",
                        type_name(e.entry_type)
                    ),
                }
            }
        }
        out
    };

    let (new_frags_raw, new_entries) = if let Some((label, data)) = &opts.recovery {
        if platform.is_some() {
            return Err(Error::Usage(
                "--recovery conflicts with a platform file (pick one replacement)".to_string(),
            ));
        }
        if mode != Mode::Keep {
            return Err(Error::Usage(
                "--recovery conflicts with --split-first-stage/--merge".to_string(),
            ));
        }
        build_recovery(&im, &kept, &drops, &orig, orig_valid_dlkm, label, data)?
    } else {
        match (platform, mode) {
        (None, Mode::Keep) => {
            if orig.table_ok {
                // Survivors round-trip verbatim, offsets rechained (with
                // no --drop this is byte-identical to the input).
                let mut f = Vec::new();
                let mut t = Vec::new();
                for (i, e) in im.table.iter().enumerate() {
                    if !kept.get(i).copied().unwrap_or(false) {
                        continue;
                    }
                    f.push(im.frag_bytes(i).unwrap().to_vec());
                    t.push(e.clone());
                }
                let (f, t) = rechain(f, t);
                if drops.first_stage { strip_platforms(f, t)? } else { (f, t) }
            } else {
                // Stale-table single stream -> one platform entry over the
                // whole verbatim blob (validated first).
                rescue_note();
                let en = whole_blob_entries(&im)?;
                if drops.first_stage {
                    let (stripped, n) = strip_first_stage(en);
                    if n > 0 {
                        eprintln!("dropped {n} first-stage entries from whole-blob rescue (--drop first-stage)");
                    }
                    let (raw, e) =
                        encode_fragment(&stripped, TYPE_PLATFORM, 0, board_id_of(&im, &kept, TYPE_PLATFORM));
                    (vec![raw], vec![e])
                } else {
                    let mut e = RamdiskEntry::platform(im.ramdisk_blob.len() as u32);
                    e.board_id = board_id_of(&im, &kept, TYPE_PLATFORM);
                    (vec![im.ramdisk_blob.clone()], vec![e])
                }
            }
        }
        (None, Mode::Merge) => {
            let entries = cpio::drop_trailers(&pooled_all()?);
            let (raw, e) = encode_fragment(&entries, TYPE_PLATFORM, 0, board_id_of(&im, &kept, TYPE_PLATFORM));
            (vec![raw], vec![e])
        }
        (None, Mode::Split) => split_entries(pooled_all()?, None, &im, &kept)?,
        (Some((label, data)), Mode::Keep) => {
            let new_plat_raw = normalize_input(label, &data)?;
            let entries = blob_entries(&new_plat_raw).map_err(|e| {
                Error::Parse(format!("internal error re-reading new platform: {e}"))
            })?;
            let (entries, fs_rebuilt) =
                apply_first_stage(&im, &kept, orig.table_ok, &drops, label, entries);
            // A rebuilt entry set cannot reuse the passed bytes verbatim.
            let new_plat_raw = if fs_rebuilt {
                lz4legacy::compress_legacy(&cpio::build(&cpio::drop_trailers(&entries)))
            } else {
                new_plat_raw
            };
            let (plat_entries, _, lib_entries) = cpio::partition(&entries);
            let mut frags: Vec<Vec<u8>>;
            let mut table: Vec<RamdiskEntry>;
            // produced[] tracks (type, name) for the carryover filter.
            let mut produced: Vec<(u32, String)> = vec![(TYPE_PLATFORM, String::new())];
            if let Some((mut dlkm_entry, dlkm_raw)) = orig_valid_dlkm {
                // Valid original dlkm wins as fallback; new platform keeps
                // its own files untouched (verbatim bytes). The original
                // entry (name, board_id) is reused, offset refreshed.
                let off = new_plat_raw.len() as u32;
                let mut e0 = RamdiskEntry::platform(new_plat_raw.len() as u32);
                e0.board_id = board_id_of(&im, &kept, TYPE_PLATFORM);
                dlkm_entry.offset = off;
                dlkm_entry.size = dlkm_raw.len() as u32;
                frags = vec![new_plat_raw, dlkm_raw];
                table = vec![e0, dlkm_entry];
                produced.push((TYPE_DLKM, "dlkm".to_string()));
            } else if !cpio::has_payload(&lib_entries) {
                // No dlkm content anywhere: platform plus carryovers.
                let mut e0 = RamdiskEntry::platform(new_plat_raw.len() as u32);
                e0.board_id = board_id_of(&im, &kept, TYPE_PLATFORM);
                frags = vec![new_plat_raw];
                table = vec![e0];
            } else {
                // Fallback: pull lib out of the new platform into dlkm.
                let plat_cpio = cpio::build(&plat_entries);
                let dlkm_cpio = cpio::build(&lib_entries);
                let plat_raw = lz4legacy::compress_legacy(&plat_cpio);
                let dlkm_raw = lz4legacy::compress_legacy(&dlkm_cpio);
                let mut e0 = RamdiskEntry::platform(plat_raw.len() as u32);
                e0.board_id = board_id_of(&im, &kept, TYPE_PLATFORM);
                let mut e1 = RamdiskEntry::dlkm(dlkm_raw.len() as u32, plat_raw.len() as u32);
                e1.board_id = board_id_of(&im, &kept, TYPE_DLKM);
                frags = vec![plat_raw, dlkm_raw];
                table = vec![e0, e1];
                produced.push((TYPE_DLKM, "dlkm".to_string()));
            }
            append_carryovers(&mut frags, &mut table, &im, &produced, &kept);
            (frags, table)
        }
        (Some((label, data)), Mode::Merge) => {
            let new_plat_raw = normalize_input(label, &data)?;
            let entries = blob_entries(&new_plat_raw).map_err(|e| {
                Error::Parse(format!("internal error re-reading new platform: {e}"))
            })?;
            let (mut entries, _) = apply_first_stage(&im, &kept, orig.table_ok, &drops, label, entries);
            // Platform slots are replaced; non-platform original content
            // (dlkm/recovery) joins the single fragment.
            entries.extend(pooled_non_platform());
            let entries = cpio::drop_trailers(&entries);
            let (raw, e) =
                encode_fragment(&entries, TYPE_PLATFORM, 0, board_id_of(&im, &kept, TYPE_PLATFORM));
            (vec![raw], vec![e])
        }
        (Some((label, data)), Mode::Split) => {
            let new_plat_raw = normalize_input(label, &data)?;
            let entries = blob_entries(&new_plat_raw).map_err(|e| {
                Error::Parse(format!("internal error re-reading new platform: {e}"))
            })?;
            let (entries, _) = apply_first_stage(&im, &kept, orig.table_ok, &drops, label, entries);
            // Verbatim fallback: if the new content yields no dlkm payload
            // but the original dlkm is valid, keep it byte-identically
            // (entry included, so name/board_id survive).
            let (_, _, lib_check) = cpio::partition(&entries);
            let fallback = if cpio::has_payload(&lib_check) { None } else { orig_valid_dlkm };
            let (mut frags, mut table) = split_entries(entries, fallback, &im, &kept)?;
            let produced: Vec<(u32, String)> =
                table.iter().map(|e| (e.entry_type, e.name_str())).collect();
            append_carryovers(&mut frags, &mut table, &im, &produced, &kept);
            (frags, table)
        }
        }
    };

    let ramdisk_size: u64 = new_frags_raw.iter().map(|f| f.len() as u64).collect::<Vec<_>>().iter().sum();
    if ramdisk_size > u32::MAX as u64 {
        return Err(Error::Parse("new ramdisk too large".to_string()));
    }
    let mut hdr = im.hdr.clone();
    hdr.ramdisk_size = ramdisk_size as u32;
    apply_sets(&mut hdr, &opts.sets)?;
    hdr.table_entry_num = new_entries.len() as u32;
    hdr.table_entry_size = 108;
    hdr.table_size = (new_entries.len() as u32) * 108;
    let out = assemble(
        &hdr,
        &new_frags_raw,
        &new_entries,
        &im.dtb,
        &im.bootconfig,
        if opts.drop_footer { b"" } else { &im.footer },
    );
    verify_image(&out).map_err(|e| Error::Verify(format!("refusing to emit invalid image: {e}")))?;
    Ok(out)
}
