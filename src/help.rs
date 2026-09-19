//! Global help texts. `{prog}` is the argv[0] basename at runtime.

pub fn short(prog: &str) -> String {
    format!(
        "{prog} — standalone static Android boot-image surgery.

Subprograms:
  vboot         smart vendor_boot repair flow (Pixel 6 / gs101)
  unpack        extract boot.img (v0..v4) and vendor_boot, honest verdicts
  repack        rebuild boot/vendor_boot from an unpack dir
  cpio          in-place newc archive surgery (magiskboot port)
  compress[=fmt]  squeeze one file with a codec (default gzip)
  decompress    expand one archive by magic
  pick          arrow-key menu for installer scripts (device/slot pick,
                Continue/Exit pacing, flash confirmation)

Usage:
  {prog} <subprogram> [args...]
  {prog} <subprogram> --help      short manual for one subprogram
  {prog} <subprogram> --expand    detailed manual for one subprogram
  {prog} help [subprogram]        same as --help
  {prog} help expand [subprogram] same as --expand
  {prog} --version

Examples:
  {prog} unpack vendor_boot.img -o dir -h -x
  {prog} repack dir fixed.img --base stock.img
  {prog} vboot broken.img fox.lz4 -o fox_boot.img

Exit codes everywhere: 0 ok, 1 usage error,
2 broken input / failed verification."
    )
}

pub fn expand(prog: &str) -> String {
    format!(
        "{prog} — subprograms in detail (short form: `{prog} --help`).

  vboot: checks every vendor_boot fragment, repairs stale tables,
    replaces the platform, splits/merges by subtree, verifies before
    emitting (file or stdout pipe).
  unpack: magic-detects ANDROID! vs VNDRBOOT, dumps magiskboot names +
    spec.toml, auto-decompresses, never aborts on broken sections
    (RESULT: OK / DEGRADED).
  repack: layout from spec.toml or --base; --template/--set override
    header scalars; --format transcodes; footer kept unless
    --drop-footer; re-verified in memory before writing.
  cpio: twelve in-place newc commands (exists/ls/rm/mkdir/ln/mv/add/
    extract/test/patch/backup/restore), magiskboot parity.
  compress[=gzip|xz|lzma|lz4|lz4_legacy]: one file, '-' is stdio;
    bzip2 refused (no backend built in).
  decompress: magic sniff + decode, extension stripped by default.
  pick: Up/Down + Enter menu, prints the choice (exit 0), Esc aborts
    (exit 1, empty stdout); --default answers without a terminal.

Notes: pure Rust, no external commands; diagnostics go to stderr, so
stdout stays a clean pipe (vboot/compress/decompress)."
    )
}
