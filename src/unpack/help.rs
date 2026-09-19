//! `unpack` help texts. `{prog}` is the argv[0] basename at runtime.

pub fn short(prog: &str) -> String {
    format!(
        "{prog} unpack — extract boot/vendor_boot images

Usage:
  {prog} unpack <image> [-h] [-n] [-o <dir>] [-x] [--no-spec]
  {prog} unpack --help | --expand

What it does:
  Detects ANDROID! (boot v0..v4) vs VNDRBOOT (vendor_boot v3/v4) and
  dumps every section with magiskboot-compatible names (kernel,
  ramdisk.cpio, vendor_ramdisk/<name>.cpio, dtb, bootconfig, header,
  footer.bin) plus our spec.toml for repack.

Options:
  -h               also write the magiskboot 'header' file (-h never
                   means --help here)
  -n               keep components in stored (compressed) form
  -o <dir>         destination directory (default: .)
  -x, --extract    expand each usable cpio into <file>.d/ next to it
  --no-spec        skip spec.toml

Broken sections never abort the run: bytes are dumped RAW with the
exact WHY. Final line: RESULT: OK or RESULT: DEGRADED (exit 0 in both
cases; exit 2 only when even the header is unreadable).

Examples:
  {prog} unpack vendor_boot.img -o dir -h -x
  {prog} unpack boot.img -o dir -n

Exit codes: 0 unpacked (maybe DEGRADED), 1 usage error, 2 unreadable.
Details: {prog} unpack --expand"
    )
}

pub fn expand(prog: &str) -> String {
    format!(
        "{prog} unpack — details (see `{prog} unpack --help` for the short form)

Detected formats (sniffed by magic, magiskboot order):
  raw | gzip (1f 8b) | xz (fd 37 7a 58 5a 00) | lzma-alone (5d + pow2
  dict) | lz4-frame (03/04 21/22 4c/4d 18) | lz4-legacy (02 21 4c 18).
  Pixel kernels/ramdisks are usually lz4-legacy; GKI kernels ship as
  lz4-legacy Image.lz4 blobs. Default: kernel, ramdisk and extra are
  decompressed on the fly; -n keeps original bytes.

Reporting (stdout; diagnostics go to stderr):
  Every section prints byte range, declared vs available size, detected
  format and a verdict. A fragment that fails to decompress is dumped
  RAW and flagged INVALID, e.g.
    frag 0 platform: INVALID lz4 block 6 truncated (need 3970208, have
      1175831) — table slices one stream mid-block; table sum vs header
      (diff 5388)
  A stale-table vendor_boot additionally yields
  vendor_ramdisk/ramdisk.full-rescue.cpio (whole blob as one valid
  stream) so the content stays recoverable. vendor_boot with dtb_size 0
  is legal (dtb absent, not broken).

Notes:
  spec.toml records only what repack reads (scalars, formats, sizes,
  names/types/board_id); sizes are advisory, verdicts stay in the
  report. -x trees (<file>.d/) are a one-way inspection aid; repack
  works from the .cpio files. unpack always writes files, never stdout
  (use vboot for pipe mode)."
    )
}
