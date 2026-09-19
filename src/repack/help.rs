//! `repack` help texts. `{prog}` is the argv[0] basename at runtime.

pub fn short(prog: &str) -> String {
    format!(
        "{prog} repack — rebuild boot/vendor_boot from an unpack dir

Usage:
  {prog} repack [dir=\".\"] [out=\"new-boot.img\"] [options]
  {prog} repack --help | --expand

Layout: dir/spec.toml, else --base <img> (without both: exit 1).
Header scalars: spec/base -> dir/header -> --template -> --set.

Options:
  -b <img>           base image (sizes, formats, missing-file bytes)
  -t <img>           template image (foreign header scalars)
  -s k=v             header override, repeatable (cmdline, name,
                     os_version, os_patch_level, page_size, *_addr,
                     dtb_addr)
  -f target=fmt      compression target, repeatable (raw|gzip|xz|lzma|
                     lz4|lz4_legacy; groups: ramdisk, all)
  -n                 skip all compression (verbatim copy)
  --drop-footer      omit trailing bytes
  --pad-to <bytes>   zero-pad up to size
  --min-free <size>  space reserve (e.g. 512M)
  --check-dir <dir>  space-check dir override
  -o <file>          output path (or positional [out])

The rebuilt image is re-verified in memory; on failure nothing is
written.

Examples:
  {prog} repack dir fixed.img
  {prog} repack dir fox.img -b stock.img -f ramdisk.cpio=gzip
  {prog} repack pinit/ init.img -s cmdline=\"console=ttyS0\" -n

Exit codes: 0 ok, 1 usage error, 2 broken input / failed verification.
Details: {prog} repack --expand"
    )
}

pub fn expand(prog: &str) -> String {
    format!(
        "{prog} repack — details (see `{prog} repack --help` for the short form)

Header scalar precedence (later wins):
  spec.toml (or --base) -> dir/header (magiskboot parity: name, cmdline,
  os_version, os_patch_level) -> --template -> --set k=v. Unknown keys
  and boot-only keys (os_version, os_patch_level) on vendor_boot are
  usage errors, never silent.

Component formats:
  A file that already sniffs as compressed is copied verbatim; a raw
  file is compressed to the target. Precedence per section: exact file
  match > ramdisk-group > all > spec stored_format > --base detected
  format > raw. v4 boot ramdisk is forced to lz4_legacy (GKI merge rule,
  like magiskboot) unless -n or an explicit --format says otherwise —
  reported as 'RAMDISK_FMT: [old] -> [lz4_legacy]'.

Vendor_boot specifics:
  The ramdisk table is rebuilt from scratch: offsets rechained from the
  new blob sizes, types/names from spec (or --base table), board_id from
  spec board_id_hex (128 hex chars = 64 bytes, absent = zeros).
  dtb/bootconfig come from files, else --base bytes, else a clean error
  naming the missing size.

Boot specifics:
  kernel + kernel_dtb concatenate (kernel_dtb honored only with an
  explicit kernel file; otherwise a warning, base bytes kept). The
  recovery_dtbo offset is refreshed. dtb is NOT split out of kernel on
  repack (only on unpack).

Footer and AVB:
  Kept by default: dir/footer.bin, else --base trailing bytes. AVB
  hashes never survive content changes — resign afterwards if the
  verified-boot chain matters.

Output:
  Always a file (default new-boot.img). Space is checked first (output
  parent or --check-dir must fit image + --min-free). Sections are
  re-parsed and re-verified in memory (ramdisk cpio, FDTs, table
  chaining); on failure nothing is written (exit 2)."
    )
}
