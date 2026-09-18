//! `spec.toml`: layout record written by `unpack`, trusted by `repack`.
//!
//! Only what `repack` actually reads is stored: header scalars, per-file
//! formats/sizes/names/types/board_id. Everything else (validity verdicts,
//! entry counts, WHY strings, footer bytes) lives in the unpack stdout
//! report or on disk as files. Human-editable; unknown keys are ignored
//! on read (forward tolerance), missing files fall back to `--base` bytes.

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSpec {
    /// "boot" | "vendor_boot"
    pub kind: String,
    pub header_version: u32,
    pub page_size: u32,
    /// All scalar header fields, kind-dependent; absent = 0/empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_addr: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ramdisk_addr: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_addr: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags_addr: Option<u32>,
    /// Human form "A.B.C" (boot only). Decoded back to the bitfield on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,
    /// Human form "Y-MM" (boot only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_patch_level: Option<String>,
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

/// Split the os_version bitfield into human ("A.B.C", "Y-MM") strings.
pub(crate) fn os_human(os_version: u32) -> Option<(String, String)> {
    if os_version == 0 {
        return None;
    }
    let version = os_version >> 11;
    let patch = os_version & 0x7ff;
    let a = (version >> 14) & 0x7f;
    let b = (version >> 7) & 0x7f;
    let c = version & 0x7f;
    let y = (patch >> 4) + 2000;
    let m = patch & 0xf;
    Some((format!("{a}.{b}.{c}"), format!("{y}-{m:02}")))
}

/// Encode human ("A.B.C", "Y-MM") strings back into the bitfield.
/// Either side may be absent (None) to keep the current half.
pub(crate) fn os_encode(cur: u32, v: Option<&str>, p: Option<&str>) -> Result<u32> {
    let mut version = cur >> 11;
    let mut patch = cur & 0x7ff;
    if let Some(v) = v {
        let mut it = v.split('.');
        let a: u32 = it
            .next()
            .ok_or_else(|| Error::Parse(format!("bad os_version '{v}' (want A.B.C)")))?
            .parse()
            .map_err(|_| Error::Parse(format!("bad os_version '{v}' (want A.B.C)")))?;
        let b: u32 = it
            .next()
            .ok_or_else(|| Error::Parse(format!("bad os_version '{v}' (want A.B.C)")))?
            .parse()
            .map_err(|_| Error::Parse(format!("bad os_version '{v}' (want A.B.C)")))?;
        let c: u32 = it
            .next()
            .ok_or_else(|| Error::Parse(format!("bad os_version '{v}' (want A.B.C)")))?
            .parse()
            .map_err(|_| Error::Parse(format!("bad os_version '{v}' (want A.B.C)")))?;
        if a > 127 || b > 127 || c > 127 {
            return Err(Error::Parse(format!("bad os_version '{v}' (parts fit in 7 bits)")));
        }
        version = (a << 14) | (b << 7) | c;
    }
    if let Some(p) = p {
        let (y, m) = p
            .split_once('-')
            .ok_or_else(|| Error::Parse(format!("bad os_patch_level '{p}' (want Y-MM)")))?;
        let y: u32 = y
            .parse()
            .map_err(|_| Error::Parse(format!("bad os_patch_level '{p}' (want Y-MM)")))?;
        let m: u32 = m
            .parse()
            .map_err(|_| Error::Parse(format!("bad os_patch_level '{p}' (want Y-MM)")))?;
        if y < 2000 || m > 12 {
            return Err(Error::Parse(format!("bad os_patch_level '{p}' (want Y-MM)")));
        }
        patch = ((y - 2000) << 4) | m;
    }
    Ok((version << 11) | patch)
}
