//! `spec.toml`: full-fidelity layout record written by `unpack`,
//! trusted by `repack`. Human-editable; unknown keys are ignored on read
//! (forward tolerance), missing files fall back to `--base` bytes.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub image: ImageSpec,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ramdisk: Vec<RamdiskSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blob: Vec<BlobSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rescue: Option<RescueSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub footer: Option<FooterSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSpec {
    /// "boot" | "vendor_boot"
    pub kind: String,
    pub header_version: u32,
    pub page_size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// All scalar header fields, kind-dependent; absent = 0/empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_addr: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ramdisk_addr: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_addr: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags_addr: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_cmdline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dtb_addr: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_entry_num: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table_entry_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootconfig_size: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RamdiskSpec {
    /// File on disk, relative to the unpack dir.
    pub file: String,
    pub name: String,
    #[serde(rename = "type")]
    pub etype: String,
    /// Format of the bytes IN THE IMAGE (raw|gzip|xz|lzma|lz4|lz4_legacy).
    pub stored_format: String,
    /// "decompressed" (file holds cpio) or "raw" (file holds image bytes).
    pub on_disk: String,
    pub declared_size: u32,
    pub declared_offset: u32,
    pub valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why_invalid: Option<String>,
    /// 64 board_id bytes as 128 hex chars; absent = all zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_id_hex: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobSpec {
    /// kernel | kernel_dtb | second | extra | recovery_dtbo | dtb |
    /// signature | bootconfig
    pub name: String,
    pub file: String,
    pub stored_format: String,
    pub on_disk: String,
    pub declared_size: u32,
    pub available_size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RescueSpec {
    /// Whole-blob rescue for stale-table images (decompressed cpio).
    pub file: String,
    pub stored_format: String,
    pub entries: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FooterSpec {
    pub size: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub all_zero: bool,
}

pub fn write_spec(dir: &Path, spec: &Spec) -> Result<()> {
    let text = toml::to_string_pretty(spec)
        .map_err(|e| Error::Io(format!("cannot serialize spec.toml: {e}")))?;
    std::fs::write(dir.join("spec.toml"), text)?;
    Ok(())
}

pub fn read_spec(dir: &Path) -> Result<Spec> {
    let text = std::fs::read_to_string(dir.join("spec.toml"))
        .map_err(|e| Error::Io(format!("cannot read spec.toml: {e} (unpack first, or pass --base/--template)")))?;
    toml::from_str(&text).map_err(|e| Error::Parse(format!("bad spec.toml: {e}")))
}

pub(crate) fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(char::from_digit((x >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((x & 0xf) as u32, 16).unwrap());
    }
    s
}

pub(crate) fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        return Err(Error::Parse(format!("bad hex (odd length): '{s}'")));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16);
        let lo = (bytes[i + 1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => return Err(Error::Parse(format!("bad hex: '{s}'"))),
        }
        i += 2;
    }
    Ok(out)
}
