//! Compressed-format sniffing + transcoding for boot image components.
//!
//! Supported (mirroring magiskboot's set, minus the C codecs):
//! gzip, xz, lzma-alone, lz4-frame, lz4-legacy. Everything else is `Raw`
//! and passes through untouched. Pure Rust on every target.

use crate::common::error::{Error, Result};
use crate::common::lz4legacy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Raw,
    Gzip,
    Xz,
    Lzma,
    Lz4Frame,
    Lz4Legacy,
}

impl Format {
    /// magiskboot-compatible name.
    pub fn name(self) -> &'static str {
        match self {
            Format::Raw => "raw",
            Format::Gzip => "gzip",
            Format::Xz => "xz",
            Format::Lzma => "lzma",
            Format::Lz4Frame => "lz4",
            Format::Lz4Legacy => "lz4_legacy",
        }
    }

    // Used by compress/repack only: silent in the `small` build.
    #[cfg_attr(feature = "small", allow(dead_code))]
    pub fn parse(s: &str) -> Result<Format> {
        match s.trim().to_ascii_lowercase().as_str() {
            "raw" | "none" | "cpio" => Ok(Format::Raw),
            "gzip" | "gz" => Ok(Format::Gzip),
            "xz" => Ok(Format::Xz),
            "lzma" => Ok(Format::Lzma),
            "lz4" | "lz4_frame" | "lz4-frame" => Ok(Format::Lz4Frame),
            "lz4_legacy" | "lz4-legacy" | "lz4_lg" | "lz4-lg" | "legacy" => Ok(Format::Lz4Legacy),
            _ => Err(Error::Usage(format!(
                "unknown format '{s}' (want raw|gzip|xz|lzma|lz4|lz4_legacy)"
            ))),
        }
    }

    // Used by compress/repack only: silent in the `small` build.
    #[cfg_attr(feature = "small", allow(dead_code))]
    pub fn is_compressed(self) -> bool {
        !matches!(self, Format::Raw)
    }
}

fn guess_lzma(buf: &[u8]) -> bool {
    // 0: (pb*5+lp)*9+lc; 1-4: dict size, must be a power of two;
    // 5-12: unpacked size, 8x 0xFF means "unknown" (streamed).
    if buf.len() <= 13 || buf[0] != 0x5d {
        return false;
    }
    let dict = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
    if dict == 0 || (dict & (dict - 1)) != 0 {
        return false;
    }
    buf[5..13] == [0xff; 8]
}

/// Identify a blob by magic (same order as magiskboot's check_fmt).
pub fn sniff(buf: &[u8]) -> Format {
    if buf.len() >= 2 && (buf[0..2] == [0x1f, 0x8b] || buf[0..2] == [0x1f, 0x9e]) {
        Format::Gzip
    } else if buf.len() >= 6 && buf[0..6] == [0xfd, b'7', b'z', b'X', b'Z', 0x00] {
        Format::Xz
    } else if guess_lzma(buf) {
        Format::Lzma
    } else if buf.len() >= 4
        && (buf[0..4] == [0x03, 0x21, 0x4c, 0x18] || buf[0..4] == [0x04, 0x22, 0x4d, 0x18])
    {
        Format::Lz4Frame
    } else if buf.len() >= 4 && buf[0..4] == [0x02, 0x21, 0x4c, 0x18] {
        Format::Lz4Legacy
    } else {
        Format::Raw
    }
}

fn fail(what: &str, e: impl std::fmt::Debug) -> Error {
    Error::Parse(format!("{what} failed: {e:?}"))
}

/// Decompress one blob. Raw passes through.
pub fn decompress(fmt: Format, data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read as _;
    match fmt {
        Format::Raw => Ok(data.to_vec()),
        Format::Gzip => {
            let mut d = flate2::read::GzDecoder::new(data);
            let mut out = Vec::new();
            d.read_to_end(&mut out).map_err(|e| fail("gzip decode", e))?;
            Ok(out)
        }
        Format::Xz => {
            let mut d = lzma_rust2::XzReader::new(data, false);
            let mut out = Vec::new();
            d.read_to_end(&mut out).map_err(|e| fail("xz decode", e))?;
            Ok(out)
        }
        Format::Lzma => {
            let mut d = lzma_rust2::LzmaReader::new_mem_limit(data, u32::MAX, None)
                .map_err(|e| fail("lzma setup", e))?;
            let mut out = Vec::new();
            d.read_to_end(&mut out).map_err(|e| fail("lzma decode", e))?;
            Ok(out)
        }
        Format::Lz4Frame => {
            let mut d = lz4_flex::frame::FrameDecoder::new(data);
            let mut out = Vec::new();
            d.read_to_end(&mut out).map_err(|e| fail("lz4-frame decode", e))?;
            Ok(out)
        }
        Format::Lz4Legacy => lz4legacy::decompress_legacy(data),
    }
}

/// Compress raw bytes into `fmt` (Raw = verbatim).
// Used by the compress subprogram only: silent in the `small` build.
#[cfg_attr(feature = "small", allow(dead_code))]
pub fn compress(fmt: Format, data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write as _;
    match fmt {
        Format::Raw => Ok(data.to_vec()),
        Format::Gzip => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(data).map_err(|e| fail("gzip encode", e))?;
            e.finish().map_err(|e| fail("gzip finish", e))
        }
        Format::Xz => {
            let mut e = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::default())
                .map_err(|e| fail("xz setup", e))?;
            e.write_all(data).map_err(|e| fail("xz encode", e))?;
            e.finish().map_err(|e| fail("xz finish", e))
        }
        Format::Lzma => {
            let mut e =
                lzma_rust2::LzmaWriter::new_use_header(Vec::new(), &lzma_rust2::LzmaOptions::default(), None)
                    .map_err(|e| fail("lzma setup", e))?;
            e.write_all(data).map_err(|e| fail("lzma encode", e))?;
            e.finish().map_err(|e| fail("lzma finish", e))
        }
        Format::Lz4Frame => {
            let mut e = lz4_flex::frame::FrameEncoder::new(Vec::new());
            e.write_all(data).map_err(|e| fail("lz4-frame encode", e))?;
            e.finish().map_err(|e| fail("lz4-frame finish", e))
        }
        Format::Lz4Legacy => Ok(lz4legacy::compress_legacy(data)),
    }
}
