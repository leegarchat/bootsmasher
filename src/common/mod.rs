//! Shared building blocks for all subprograms.
//!
//! Everything here is subprogram-agnostic: error type, image formats
//! (`bootimg` for `ANDROID!`, `vendor` for `VNDRBOOT`), codecs, `newc`
//! cpio handling, FDT walking, `spec.toml`, directory extraction and the
//! free-space gate. Subprograms (`vboot`, `unpack`, `repack`, `cpio`,
//! `compress`) only depend on `common` (plus `vboot::ops`, the
//! vendor_boot analyzer shared by `vboot`/`unpack`/`repack`).

// bootimg/extract/spec serve unpack/repack only: compiled out of the
// `small` build (vboot+install, for recovery ramdisks).
#[cfg(not(feature = "small"))]
pub(crate) mod bootimg;
pub(crate) mod codec;
pub(crate) mod cpio;
pub(crate) mod dtb;
pub(crate) mod error;
#[cfg(not(feature = "small"))]
pub(crate) mod extract;
pub(crate) mod lz4legacy;
pub(crate) mod space;
#[cfg(all(not(feature = "small"), feature = "spec"))]
pub(crate) mod spec;
pub(crate) mod vendor;
