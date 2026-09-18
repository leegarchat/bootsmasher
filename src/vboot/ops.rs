//! Smart vendor_boot analyzer + repacker (Pixel 6 / gs101 specialization).
//!
//! Pipeline:
//!   load (bounds-checked sectioning) -> analyze (per-fragment validity,
//!   stale-table detection) -> build new fragment set -> assemble ->
//!   verify rebuilt image in memory -> emit (file or stdout, no chatter).

use crate::error::{Error, Result};
use crate::vboot::cpio::{self, Entry};
use crate::vboot::dtb;
use crate::vboot::image::{align_up, type_name, Header, RamdiskEntry, HEADER_LEN};
use crate::vboot::image::{TYPE_DLKM, TYPE_PLATFORM, TYPE_RECOVERY};
use crate::vboot::lz4legacy::{self, BlobKind};

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

/// A fully sectioned vendor_boot image.
pub struct Image {
    pub hdr: Header,
    pub ramdisk_blob: Vec<u8>,
    pub dtb: Vec<u8>,
    pub table: Vec<RamdiskEntry>,
    pub bootconfig: Vec<u8>,
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
                stored_format: match f.kind {
                    BlobKind::Lz4Legacy => "lz4_legacy".to_string(),
                    BlobKind::Cpio => "cpio".to_string(),
                    BlobKind::Unknown => "unknown".to_string(),
                },
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
            stored_format: match kind {
                BlobKind::Lz4Legacy => "lz4_legacy".to_string(),
                BlobKind::Cpio => "cpio".to_string(),
                BlobKind::Unknown => "unknown".to_string(),
            },
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
        BlobKind::Unknown => (BlobKind::Unknown, false, "unknown fragment format".to_string(), None),
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
/// lz4-legacy stays verbatim (after validation), raw cpio gets compressed.
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
        BlobKind::Unknown => Err(Error::Parse(format!(
            "platform file {path}: neither lz4-legacy nor cpio (need platform.cpio or platform.cpio.lz4)"
        ))),
    }
}

/// Assemble an image from a header template + fragments + dtb + bootconfig.
pub(crate) fn assemble(hdr: &Header, frags: &[Vec<u8>], entries: &[RamdiskEntry], dtb: &[u8], bootconfig: &[u8]) -> Vec<u8> {
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
    out
}

/// Verify a freshly assembled image fully in memory before emitting.
pub fn verify_image(bytes: &[u8]) -> Result<()> {
    let im = Image::load(bytes).map_err(|e| Error::Verify(format!("rebuilt image does not parse: {e}")))?;
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

/// Decompress one fragment blob (lz4-legacy or raw cpio) into entries.
fn blob_entries(blob: &[u8]) -> Result<Vec<Entry>> {
    match lz4legacy::sniff(blob) {
        BlobKind::Lz4Legacy => {
            let dec = lz4legacy::decompress_legacy(blob)?;
            cpio::parse(&dec)
        }
        BlobKind::Cpio => cpio::parse(blob),
        BlobKind::Unknown => Err(Error::Parse("fragment is neither lz4-legacy nor cpio".to_string())),
    }
}

/// Entries of every valid original fragment, plus a flag telling whether
/// the table as a whole is trustworthy (chained offsets, matching sum).
struct OrigContent {
    frags: Vec<(RamdiskEntry, Vec<Entry>)>,
    table_ok: bool,
}

fn orig_content(im: &Image) -> OrigContent {
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

/// Raw bytes + table entry for one freshly encoded fragment.
fn encode_fragment(entries: &[Entry], etype: u32, offset: u32) -> (Vec<u8>, RamdiskEntry) {
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
    (raw, entry)
}

/// Build platform/recovery/dlkm fragments from an entry pool.
/// `verbatim_dlkm`: valid original dlkm bytes, used only when the pool
/// yields no dlkm payload of its own.
fn split_entries(
    entries: Vec<Entry>,
    verbatim_dlkm: Option<Vec<u8>>,
) -> Result<(Vec<Vec<u8>>, Vec<RamdiskEntry>)> {
    let (plat, rec, dlkm) = cpio::partition(&entries);
    if plat.is_empty() {
        return Err(Error::Parse(
            "split leaves platform empty (content is only lib/recovery?)".to_string(),
        ));
    }
    let mut frags = Vec::new();
    let mut table = Vec::new();
    let (plat_raw, e0) = encode_fragment(&plat, TYPE_PLATFORM, 0);
    let mut off = plat_raw.len() as u32;
    frags.push(plat_raw);
    table.push(e0);
    if cpio::has_payload(&rec) {
        let (rec_raw, e) = encode_fragment(&rec, TYPE_RECOVERY, off);
        off += rec_raw.len() as u32;
        frags.push(rec_raw);
        table.push(e);
    }
    if cpio::has_payload(&dlkm) {
        let (dlkm_raw, e) = encode_fragment(&dlkm, TYPE_DLKM, off);
        frags.push(dlkm_raw);
        table.push(e);
    } else if let Some(raw) = verbatim_dlkm {
        let e = RamdiskEntry::dlkm(raw.len() as u32, off);
        frags.push(raw);
        table.push(e);
    }
    Ok((frags, table))
}

/// Main repack routine.
///
/// - `platform`: optional (path label, bytes) replacement content.
/// - `mode`: Keep (round-trip verbatim / normalize stale table),
///   Split (partition into platform/recovery/dlkm by subtree),
///   Merge (glue everything into one platform fragment).
pub fn repack(orig_bytes: &[u8], platform: Option<(&str, Vec<u8>)>, mode: Mode) -> Result<Vec<u8>> {
    let im = Image::load(orig_bytes)?;
    let orig = orig_content(&im);
    let orig_valid_dlkm_raw: Option<Vec<u8>> = {
        let mut out = None;
        for (i, e) in im.table.iter().enumerate() {
            if e.entry_type != TYPE_DLKM {
                continue;
            }
            if let Ok(b) = im.frag_bytes(i) {
                if check_fragment(b).1 {
                    out = Some(b.to_vec());
                }
            }
        }
        out
    };

    // Entry pool for Split/Merge without a platform file: per-fragment
    // content when the table is trustworthy, whole-blob rescue otherwise.
    let pooled_all = || -> Result<Vec<Entry>> {
        if orig.table_ok {
            Ok(orig.frags.iter().flat_map(|(_, en)| en.clone()).collect())
        } else {
            whole_blob_entries(&im)
        }
    };
    // Valid non-platform original entries (dlkm/recovery) for the Merge
    // union. Unreadable ones are skipped with a stderr note: Merge must
    // never die on the stale-table garbage it replaces.
    let pooled_non_platform = || -> Vec<Entry> {
        let mut out = Vec::new();
        for (i, e) in im.table.iter().enumerate() {
            if e.entry_type == TYPE_PLATFORM {
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

    let (new_frags_raw, new_entries) = match (platform, mode) {
        (None, Mode::Keep) => {
            if orig.table_ok {
                // Byte-identical round trip.
                let mut f = Vec::new();
                let mut t = Vec::new();
                for (i, e) in im.table.iter().enumerate() {
                    f.push(im.frag_bytes(i).unwrap().to_vec());
                    t.push(e.clone());
                }
                (f, t)
            } else {
                // Stale-table single stream -> one platform entry over the
                // whole verbatim blob (validated first).
                whole_blob_entries(&im)?;
                let e = RamdiskEntry::platform(im.ramdisk_blob.len() as u32);
                (vec![im.ramdisk_blob.clone()], vec![e])
            }
        }
        (None, Mode::Merge) => {
            let entries = cpio::drop_trailers(&pooled_all()?);
            let (raw, e) = encode_fragment(&entries, TYPE_PLATFORM, 0);
            (vec![raw], vec![e])
        }
        (None, Mode::Split) => split_entries(pooled_all()?, None)?,
        (Some((label, data)), Mode::Keep) => {
            let new_plat_raw = normalize_input(label, &data)?;
            let entries = blob_entries(&new_plat_raw).map_err(|e| {
                Error::Parse(format!("internal error re-reading new platform: {e}"))
            })?;
            let (plat_entries, _, lib_entries) = cpio::partition(&entries);
            if let Some(dlkm_raw) = orig_valid_dlkm_raw {
                // Valid original dlkm wins as fallback; new platform keeps
                // its own files untouched (verbatim bytes).
                let off = new_plat_raw.len() as u32;
                let e0 = RamdiskEntry::platform(new_plat_raw.len() as u32);
                let e1 = RamdiskEntry::dlkm(dlkm_raw.len() as u32, off);
                (vec![new_plat_raw, dlkm_raw], vec![e0, e1])
            } else if !cpio::has_payload(&lib_entries) {
                // No dlkm content anywhere: platform-only image.
                let e0 = RamdiskEntry::platform(new_plat_raw.len() as u32);
                (vec![new_plat_raw], vec![e0])
            } else {
                // Fallback: pull lib out of the new platform into dlkm.
                let plat_cpio = cpio::build(&plat_entries);
                let dlkm_cpio = cpio::build(&lib_entries);
                let plat_raw = lz4legacy::compress_legacy(&plat_cpio);
                let dlkm_raw = lz4legacy::compress_legacy(&dlkm_cpio);
                let e0 = RamdiskEntry::platform(plat_raw.len() as u32);
                let e1 = RamdiskEntry::dlkm(dlkm_raw.len() as u32, plat_raw.len() as u32);
                (vec![plat_raw, dlkm_raw], vec![e0, e1])
            }
        }
        (Some((label, data)), Mode::Merge) => {
            let new_plat_raw = normalize_input(label, &data)?;
            let mut entries = blob_entries(&new_plat_raw).map_err(|e| {
                Error::Parse(format!("internal error re-reading new platform: {e}"))
            })?;
            // Platform slots are replaced; non-platform original content
            // (dlkm/recovery) joins the single fragment.
            entries.extend(pooled_non_platform());
            let entries = cpio::drop_trailers(&entries);
            let (raw, e) = encode_fragment(&entries, TYPE_PLATFORM, 0);
            (vec![raw], vec![e])
        }
        (Some((label, data)), Mode::Split) => {
            let new_plat_raw = normalize_input(label, &data)?;
            let entries = blob_entries(&new_plat_raw).map_err(|e| {
                Error::Parse(format!("internal error re-reading new platform: {e}"))
            })?;
            // Verbatim fallback: if the new content yields no dlkm payload
            // but the original dlkm is valid, keep it byte-identically.
            let (_, _, lib_check) = cpio::partition(&entries);
            let fallback = if cpio::has_payload(&lib_check) { None } else { orig_valid_dlkm_raw };
            split_entries(entries, fallback)?
        }
    };

    let ramdisk_size: u64 = new_frags_raw.iter().map(|f| f.len() as u64).collect::<Vec<_>>().iter().sum();
    if ramdisk_size > u32::MAX as u64 {
        return Err(Error::Parse("new ramdisk too large".to_string()));
    }
    let mut hdr = im.hdr.clone();
    hdr.ramdisk_size = ramdisk_size as u32;
    hdr.table_entry_num = new_entries.len() as u32;
    hdr.table_entry_size = 108;
    hdr.table_size = (new_entries.len() as u32) * 108;
    let out = assemble(&hdr, &new_frags_raw, &new_entries, &im.dtb, &im.bootconfig);
    verify_image(&out).map_err(|e| Error::Verify(format!("refusing to emit invalid image: {e}")))?;
    Ok(out)
}
