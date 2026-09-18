//! ANDROID! boot image headers v0..v4: parse, section layout, serialize.
//!
//! Layout mirrors magiskboot: sections are read sequentially, each padded
//! up to the page size (v3/v4 use a fixed 4096 page). recovery_dtbo is
//! read sequentially on unpack (its offset field is refreshed on repack).
//! PXA/Samsung oddities are refused with a clear message instead of
//! guessing.

use crate::error::{Error, Result};

pub const BOOT_MAGIC: &[u8; 8] = b"ANDROID!";

fn u32le(b: &[u8], off: usize, what: &str) -> Result<u32> {
    b.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| Error::Parse(format!("boot header truncated at {what}")))
}

fn u64le(b: &[u8], off: usize, what: &str) -> Result<u64> {
    b.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| Error::Parse(format!("boot header truncated at {what}")))
}

#[derive(Debug, Clone)]
pub struct BootHeader {
    pub version: u32,
    pub page_size: u32,
    pub kernel_size: u32,
    pub kernel_addr: u32,
    pub ramdisk_size: u32,
    pub ramdisk_addr: u32,
    pub second_size: u32,
    pub second_addr: u32,
    pub tags_addr: u32,
    pub os_version: u32,
    pub name: Vec<u8>,
    pub cmdline: Vec<u8>,
    pub extra_cmdline: Vec<u8>,
    pub id: Vec<u8>,
    pub recovery_dtbo_size: u32,
    pub recovery_dtbo_offset: u64,
    pub dtb_size: u32,
    pub dtb_addr: u64,
    pub header_size: u32,
    pub signature_size: u32,
    /// Raw header bytes (hdr_space), preserved verbatim on repack.
    pub raw: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: &'static str,
    pub file: &'static str,
    pub start: usize,
    pub len: usize,
}

#[derive(Debug, Clone)]
pub struct BootImage {
    pub hdr: BootHeader,
    /// Byte ranges of kernel/ramdisk/second/extra/dtbo/dtb/signature.
    pub sections: Vec<Section>,
    /// Bytes after the last section (vbmeta/seandroid/zeros).
    pub footer: Vec<u8>,
    /// Offset where the footer starts (informational).
    pub footer_off: usize,
}

pub fn align_up(x: usize, a: usize) -> usize {
    if a == 0 {
        return x;
    }
    let r = x % a;
    if r == 0 {
        x
    } else {
        x + (a - r)
    }
}

fn cstr(b: &[u8]) -> Vec<u8> {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    b[..end].to_vec()
}

impl BootHeader {
    pub fn cmdline_full(&self) -> String {
        let mut s = String::from_utf8_lossy(&self.cmdline).into_owned();
        let e = String::from_utf8_lossy(&self.extra_cmdline);
        if !e.is_empty() {
            s.push_str(&e);
        }
        s
    }

    pub fn os_version_str(&self) -> Option<(String, String)> {
        if self.os_version == 0 {
            return None;
        }
        let version = self.os_version >> 11;
        let patch = self.os_version & 0x7ff;
        let a = (version >> 14) & 0x7f;
        let b = (version >> 7) & 0x7f;
        let c = version & 0x7f;
        let y = (patch >> 4) + 2000;
        let m = patch & 0xf;
        Some((format!("{a}.{b}.{c}"), format!("{y}-{m:02}")))
    }
}

fn parse_v0_common(img: &[u8]) -> Result<(u32, u32, u32, u32, u32, u32)> {
    Ok((
        u32le(img, 8, "kernel_size")?,
        u32le(img, 12, "kernel_addr")?,
        u32le(img, 16, "ramdisk_size")?,
        u32le(img, 20, "ramdisk_addr")?,
        u32le(img, 24, "second_size")?,
        u32le(img, 28, "second_addr")?,
    ))
}

/// Header space in bytes (what repack must preserve verbatim).
fn hdr_space_v0v1v2(page: usize) -> usize {
    page
}

pub fn parse(img: &[u8]) -> Result<BootImage> {
    if img.len() < 8 || &img[0..8] != BOOT_MAGIC {
        return Err(Error::Parse("not a boot image (bad ANDROID! magic)".to_string()));
    }
    // Peek the version word: for v0 it is really the tags-adjacent page
    // size slot... layout: tags(32) page(36) ver_or_extra(40) osver(44).
    let tags = u32le(img, 32, "tags_addr")?;
    let page_field = u32le(img, 36, "page_size")?;
    let ver_field = u32le(img, 40, "header_version")?;
    if page_field >= 0x0200_0000 {
        return Err(Error::Parse(format!(
            "Samsung PXA header suspected (page field {page_field:#x}); refusing to guess"
        )));
    }
    // v3/v4 have cmdline at a different place and no tags/page words;
    // detect by trying v3 shape: header_version at offset 32 must be 3/4
    // AND the v0 page field must look insane for a page size.
    let v3ver = u32le(img, 32, "v3 header_version").unwrap_or(99);
    let looks_v3 = (v3ver == 3 || v3ver == 4) && (page_field == 0 || page_field > 65536);
    if looks_v3 {
        return parse_v3v4(img, v3ver);
    }
    parse_v0v1v2(img, tags, page_field, ver_field)
}

fn parse_v0v1v2(img: &[u8], tags: u32, page: u32, ver_field: u32) -> Result<BootImage> {
    if page == 0 || page > 65536 || (page as usize & (page as usize - 1)) != 0 {
        return Err(Error::Parse(format!("insane page_size {page}")));
    }
    let (ksize, kaddr, rsize, raddr, ssize, saddr) = parse_v0_common(img)?;
    // Version: magiskboot treats the union word as version; anything
    // above 2 on old images is Samsung extra_size — clamp to v0 then.
    let mut version = ver_field;
    let mut extra = 0u32;
    if version > 2 {
        extra = version;
        version = 0;
    }
    if img.len() < 32 + 1600 {
        return Err(Error::Parse("boot image too small for v0 header".to_string()));
    }
    let os_version = u32le(img, 44, "os_version")?;
    let name = cstr(&img[48..64]);
    let cmdline = cstr(&img[64..576]);
    let id = img[576..608].to_vec();
    let extra_cmdline = cstr(&img[608..1632]);
    let mut off = 1632usize;
    let mut recovery_dtbo_size = 0u32;
    let mut recovery_dtbo_offset = 0u64;
    let mut header_size = 0u32;
    let mut dtb_size = 0u32;
    let mut dtb_addr = 0u64;
    if version >= 1 {
        if img.len() < off + 16 {
            return Err(Error::Parse("boot image too small for v1 fields".to_string()));
        }
        recovery_dtbo_size = u32le(img, off, "recovery_dtbo_size")?;
        recovery_dtbo_offset = u64le(img, off + 4, "recovery_dtbo_offset")?;
        header_size = u32le(img, off + 12, "header_size")?;
        off += 16;
    }
    if version >= 2 {
        if img.len() < off + 12 {
            return Err(Error::Parse("boot image too small for v2 fields".to_string()));
        }
        dtb_size = u32le(img, off, "dtb_size")?;
        dtb_addr = u64le(img, off + 4, "dtb_addr")?;
        off += 12;
    }
    let _ = off;
    let hdr = BootHeader {
        version,
        page_size: page,
        kernel_size: ksize,
        kernel_addr: kaddr,
        ramdisk_size: rsize,
        ramdisk_addr: raddr,
        second_size: ssize,
        second_addr: saddr,
        tags_addr: tags,
        os_version,
        name,
        cmdline,
        extra_cmdline,
        id,
        recovery_dtbo_size,
        recovery_dtbo_offset,
        dtb_size,
        dtb_addr,
        header_size,
        signature_size: 0,
        raw: img[..hdr_space_v0v1v2(page as usize).min(img.len())].to_vec(),
    };
    // Sequential layout like magiskboot (dtbo offset field refreshed later).
    let page_us = page as usize;
    let mut sections = Vec::new();
    let mut cur = hdr_space_v0v1v2(page_us);
    let mut push = |name: &'static str, file: &'static str, len: u32, cur: &mut usize| {
        let l = len as usize;
        sections.push(Section { name, file, start: *cur, len: l });
        *cur = align_up(*cur + l, page_us);
    };
    push("kernel", "kernel", ksize, &mut cur);
    push("ramdisk", "ramdisk.cpio", rsize, &mut cur);
    push("second", "second", ssize, &mut cur);
    // extra only exists on v0-as-extra_size (Samsung quirk); keep honest.
    push("extra", "extra", if version == 0 { extra } else { 0 }, &mut cur);
    push("recovery_dtbo", "recovery_dtbo", recovery_dtbo_size, &mut cur);
    push("dtb", "dtb", dtb_size, &mut cur);
    finish(img, hdr, sections, cur)
}

fn parse_v3v4(img: &[u8], version: u32) -> Result<BootImage> {
    if img.len() < 1580 {
        return Err(Error::Parse("boot image too small for v3 header".to_string()));
    }
    let kernel_size = u32le(img, 8, "kernel_size")?;
    let ramdisk_size = u32le(img, 12, "ramdisk_size")?;
    let os_version = u32le(img, 16, "os_version")?;
    let header_size = u32le(img, 20, "header_size")?;
    let cmdline = cstr(&img[40..1576]);
    let mut signature_size = 0u32;
    if version == 4 {
        if img.len() < 1584 {
            return Err(Error::Parse("boot image too small for v4 signature_size".to_string()));
        }
        signature_size = u32le(img, 1576, "signature_size")?;
    }
    let page_us = 4096usize;
    let hdr = BootHeader {
        version,
        page_size: 4096,
        kernel_size,
        kernel_addr: 0,
        ramdisk_size,
        ramdisk_addr: 0,
        second_size: 0,
        second_addr: 0,
        tags_addr: 0,
        os_version,
        name: Vec::new(),
        cmdline,
        extra_cmdline: Vec::new(),
        id: Vec::new(),
        recovery_dtbo_size: 0,
        recovery_dtbo_offset: 0,
        dtb_size: 0,
        dtb_addr: 0,
        header_size,
        signature_size,
        raw: img[..page_us.min(img.len())].to_vec(),
    };
    let mut sections = Vec::new();
    let mut cur = page_us;
    let mut push = |name: &'static str, file: &'static str, len: u32, cur: &mut usize| {
        let l = len as usize;
        sections.push(Section { name, file, start: *cur, len: l });
        *cur = align_up(*cur + l, page_us);
    };
    push("kernel", "kernel", kernel_size, &mut cur);
    push("ramdisk", "ramdisk.cpio", ramdisk_size, &mut cur);
    push("signature", "signature", signature_size, &mut cur);
    finish(img, hdr, sections, cur)
}

fn finish(img: &[u8], hdr: BootHeader, sections: Vec<Section>, end: usize) -> Result<BootImage> {
    // Clamp every section to the actual file (partition dumps are padded,
    // truncated downloads are not) and report honestly instead of dying.
    let mut clamped = Vec::new();
    for s in sections {
        let avail = img.len().saturating_sub(s.start.min(img.len()));
        let len = s.len.min(avail);
        clamped.push(Section { name: s.name, file: s.file, start: s.start.min(img.len()), len });
    }
    let last_end = clamped.iter().map(|s| s.start + s.len).max().unwrap_or(0);
    let real_end = last_end.max(end.min(img.len()));
    let footer_off = real_end;
    let footer = if footer_off < img.len() { img[footer_off..].to_vec() } else { Vec::new() };
    Ok(BootImage { hdr, sections: clamped, footer, footer_off })
}

/// Serialize the header back: clone the raw prefix (unknown ROM/vendor
/// bytes survive) and patch only the fields unpack/repack manage.
pub fn serialize(h: &BootHeader) -> Result<Vec<u8>> {
    match h.version {
        0 | 1 | 2 => {
            let need = 1632 + if h.version >= 1 { 16 } else { 0 } + if h.version >= 2 { 12 } else { 0 };
            let mut out = h.raw.clone();
            if out.len() < need {
                out.resize(need, 0);
            }
            out[0..8].copy_from_slice(BOOT_MAGIC);
            out[8..12].copy_from_slice(&h.kernel_size.to_le_bytes());
            out[12..16].copy_from_slice(&h.kernel_addr.to_le_bytes());
            out[16..20].copy_from_slice(&h.ramdisk_size.to_le_bytes());
            out[20..24].copy_from_slice(&h.ramdisk_addr.to_le_bytes());
            out[24..28].copy_from_slice(&h.second_size.to_le_bytes());
            out[28..32].copy_from_slice(&h.second_addr.to_le_bytes());
            out[32..36].copy_from_slice(&h.tags_addr.to_le_bytes());
            out[36..40].copy_from_slice(&h.page_size.to_le_bytes());
            // version-or-extra word: keep version for clean v0-v2.
            out[40..44].copy_from_slice(&h.version.to_le_bytes());
            out[44..48].copy_from_slice(&h.os_version.to_le_bytes());
            let n = h.name.len().min(16);
            out[48..48 + n].copy_from_slice(&h.name[..n]);
            for b in out[48 + n..64].iter_mut() {
                *b = 0;
            }
            let c = h.cmdline.len().min(512);
            out[64..64 + c].copy_from_slice(&h.cmdline[..c]);
            for b in out[64 + c..576].iter_mut() {
                *b = 0;
            }
            let idn = h.id.len().min(32);
            out[576..576 + idn].copy_from_slice(&h.id[..idn]);
            let e = h.extra_cmdline.len().min(1024);
            out[608..608 + e].copy_from_slice(&h.extra_cmdline[..e]);
            for b in out[608 + e..1632].iter_mut() {
                *b = 0;
            }
            if h.version >= 1 {
                out[1632..1636].copy_from_slice(&h.recovery_dtbo_size.to_le_bytes());
                out[1636..1644].copy_from_slice(&h.recovery_dtbo_offset.to_le_bytes());
                out[1644..1648].copy_from_slice(&h.header_size.to_le_bytes());
            }
            if h.version >= 2 {
                out[1648..1652].copy_from_slice(&h.dtb_size.to_le_bytes());
                out[1652..1660].copy_from_slice(&h.dtb_addr.to_le_bytes());
            }
            Ok(out)
        }
        3 | 4 => {
            let mut out = h.raw.clone();
            if out.len() < 4096 {
                out.resize(4096, 0);
            }
            out[0..8].copy_from_slice(BOOT_MAGIC);
            out[8..12].copy_from_slice(&h.kernel_size.to_le_bytes());
            out[12..16].copy_from_slice(&h.ramdisk_size.to_le_bytes());
            out[16..20].copy_from_slice(&h.os_version.to_le_bytes());
            out[20..24].copy_from_slice(&h.header_size.to_le_bytes());
            // reserved[4] stays zero
            out[36..40].copy_from_slice(&h.version.to_le_bytes());
            let c = h.cmdline.len().min(1536);
            out[40..40 + c].copy_from_slice(&h.cmdline[..c]);
            for b in out[40 + c..1576].iter_mut() {
                *b = 0;
            }
            if h.version == 4 {
                out[1576..1580].copy_from_slice(&h.signature_size.to_le_bytes());
            }
            Ok(out)
        }
        v => Err(Error::Parse(format!("cannot serialize boot header v{v}"))),
    }
}

/// Split an appended kernel_dtb off the kernel blob (magiskboot parity):
/// first FDT magic with a sane totalsize wins.
pub fn split_kernel_dtb(kernel: &[u8]) -> (Vec<u8>, Vec<u8>) {
    if let Some(off) = find_fdt(kernel) {
        return (kernel[..off].to_vec(), kernel[off..].to_vec());
    }
    (kernel.to_vec(), Vec::new())
}

fn find_fdt(buf: &[u8]) -> Option<usize> {
    let mut pos = 0;
    while pos + 8 <= buf.len() {
        let rel = buf[pos..].windows(4).position(|w| w == b"\xd0\x0d\xfe\xed")?;
        let cur = pos + rel;
        let total = u32::from_be_bytes([buf[cur + 4], buf[cur + 5], buf[cur + 6], buf[cur + 7]]) as usize;
        if total >= 0x84 && cur + total <= buf.len() && total < 32 * 1024 * 1024 {
            return Some(cur);
        }
        pos = cur + 4;
    }
    None
}
