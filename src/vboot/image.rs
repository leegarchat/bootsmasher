//! vendor_boot image header (v3/v4) parsing and assembly.
//!
//! Layout on disk (Pixel 6 / gs101, page_size 2048), every section starts
//! on a page boundary:
//!   header (ceil(header_size/page) pages) | ramdisk blobs (concatenated)
//!   | dtb | ramdisk table (1 page) | bootconfig (1 page)
//! A 64 MiB partition dump is the same image zero-padded to the block
//! device size, optionally with vbmeta + AVB footer after it.

use crate::error::{Error, Result};

pub const VENDOR_BOOT_MAGIC: &[u8; 8] = b"VNDRBOOT";
pub const HEADER_LEN: usize = 2128;

/// Ramdisk fragment types (bootimg.h).
pub const TYPE_PLATFORM: u32 = 1;
pub const TYPE_RECOVERY: u32 = 2;
pub const TYPE_DLKM: u32 = 3;

#[derive(Debug, Clone)]
pub struct RamdiskEntry {
    pub size: u32,
    pub offset: u32,
    pub entry_type: u32,
    pub name: [u8; 32],
    pub board_id: [u8; 64],
}

#[derive(Debug, Clone)]
pub struct Header {
    pub header_version: u32,
    pub page_size: u32,
    pub kernel_addr: u32,
    pub ramdisk_addr: u32,
    pub ramdisk_size: u32,
    pub cmdline: [u8; 2048],
    pub tags_addr: u32,
    pub name: [u8; 16],
    pub header_size: u32,
    pub dtb_size: u32,
    pub dtb_addr: u64,
    pub table_size: u32,
    pub table_entry_num: u32,
    pub table_entry_size: u32,
    pub bootconfig_size: u32,
}

fn u32le(b: &[u8], off: usize) -> Result<u32> {
    b.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| Error::Parse(format!("header truncated at offset {off}")))
}

fn u64le(b: &[u8], off: usize) -> Result<u64> {
    b.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| Error::Parse(format!("header truncated at offset {off}")))
}

impl Header {
    pub fn parse(img: &[u8]) -> Result<Header> {
        if img.len() < HEADER_LEN {
            return Err(Error::Parse(format!(
                "image too small for vendor_boot header: {} bytes",
                img.len()
            )));
        }
        if &img[0..8] != VENDOR_BOOT_MAGIC {
            return Err(Error::Parse("not a vendor_boot image (bad VNDRBOOT magic)".to_string()));
        }
        let mut cmdline = [0u8; 2048];
        cmdline.copy_from_slice(&img[28..28 + 2048]);
        let mut name = [0u8; 16];
        name.copy_from_slice(&img[2080..2096]);
        Ok(Header {
            header_version: u32le(img, 8)?,
            page_size: u32le(img, 12)?,
            kernel_addr: u32le(img, 16)?,
            ramdisk_addr: u32le(img, 20)?,
            ramdisk_size: u32le(img, 24)?,
            cmdline,
            tags_addr: u32le(img, 2076)?,
            name,
            header_size: u32le(img, 2096)?,
            dtb_size: u32le(img, 2100)?,
            dtb_addr: u64le(img, 2104)?,
            table_size: u32le(img, 2112)?,
            table_entry_num: u32le(img, 2116)?,
            table_entry_size: u32le(img, 2120)?,
            bootconfig_size: u32le(img, 2124)?,
        })
    }


    pub fn cmdline_str(&self) -> String {
        let end = self.cmdline.iter().position(|&b| b == 0).unwrap_or(self.cmdline.len());
        String::from_utf8_lossy(&self.cmdline[..end]).into_owned()
    }

    pub fn name_str(&self) -> String {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        String::from_utf8_lossy(&self.name[..end]).into_owned()
    }

    /// Byte offset where the ramdisk blob starts (header padded to page).
    pub fn ramdisk_start(&self) -> Result<usize> {
        if self.page_size == 0 || self.header_size == 0 {
            return Err(Error::Parse("header has zero page_size/header_size".to_string()));
        }
        Ok(align_up(self.header_size as usize, self.page_size as usize))
    }

    pub fn serialize(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0..8].copy_from_slice(VENDOR_BOOT_MAGIC);
        h[8..12].copy_from_slice(&self.header_version.to_le_bytes());
        h[12..16].copy_from_slice(&self.page_size.to_le_bytes());
        h[16..20].copy_from_slice(&self.kernel_addr.to_le_bytes());
        h[20..24].copy_from_slice(&self.ramdisk_addr.to_le_bytes());
        h[24..28].copy_from_slice(&self.ramdisk_size.to_le_bytes());
        h[28..28 + 2048].copy_from_slice(&self.cmdline);
        h[2076..2080].copy_from_slice(&self.tags_addr.to_le_bytes());
        h[2080..2096].copy_from_slice(&self.name);
        h[2096..2100].copy_from_slice(&self.header_size.to_le_bytes());
        h[2100..2104].copy_from_slice(&self.dtb_size.to_le_bytes());
        h[2104..2112].copy_from_slice(&self.dtb_addr.to_le_bytes());
        h[2112..2116].copy_from_slice(&self.table_size.to_le_bytes());
        h[2116..2120].copy_from_slice(&self.table_entry_num.to_le_bytes());
        h[2120..2124].copy_from_slice(&self.table_entry_size.to_le_bytes());
        h[2124..2128].copy_from_slice(&self.bootconfig_size.to_le_bytes());
        h
    }
}

impl RamdiskEntry {
    pub fn parse(buf: &[u8]) -> Result<RamdiskEntry> {
        if buf.len() < 108 {
            return Err(Error::Parse("ramdisk table entry truncated".to_string()));
        }
        let mut name = [0u8; 32];
        name.copy_from_slice(&buf[12..44]);
        let mut board_id = [0u8; 64];
        board_id.copy_from_slice(&buf[44..108]);
        Ok(RamdiskEntry {
            size: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            offset: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            entry_type: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            name,
            board_id,
        })
    }

    pub fn name_str(&self) -> String {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        String::from_utf8_lossy(&self.name[..end]).into_owned()
    }

    pub fn serialize(&self) -> [u8; 108] {
        let mut e = [0u8; 108];
        e[0..4].copy_from_slice(&self.size.to_le_bytes());
        e[4..8].copy_from_slice(&self.offset.to_le_bytes());
        e[8..12].copy_from_slice(&self.entry_type.to_le_bytes());
        e[12..44].copy_from_slice(&self.name);
        e[44..108].copy_from_slice(&self.board_id);
        e
    }

    pub fn platform(size: u32) -> RamdiskEntry {
        RamdiskEntry { size, offset: 0, entry_type: TYPE_PLATFORM, name: [0u8; 32], board_id: [0u8; 64] }
    }

    pub fn dlkm(size: u32, offset: u32) -> RamdiskEntry {
        let mut name = [0u8; 32];
        name[0..4].copy_from_slice(b"dlkm");
        RamdiskEntry { size, offset, entry_type: TYPE_DLKM, name, board_id: [0u8; 64] }
    }
}

pub fn align_up(x: usize, align: usize) -> usize {
    if align == 0 {
        return x;
    }
    let r = x % align;
    if r == 0 {
        x
    } else {
        x + (align - r)
    }
}

pub fn type_name(t: u32) -> &'static str {
    match t {
        0 => "none",
        TYPE_PLATFORM => "platform",
        TYPE_RECOVERY => "recovery",
        TYPE_DLKM => "dlkm",
        _ => "unknown",
    }
}
