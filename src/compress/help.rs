//! `compress` / `decompress` help texts. `{prog}` is the argv[0] basename.

pub fn short_compress(prog: &str) -> String {
    format!(
        "{prog} compress — squeeze one file with a named codec

Usage:
  {prog} compress[=format] <infile> [outfile]
  {prog} compress --help | --expand

Format (default: gzip): gzip | xz | lzma | lz4 | lz4_legacy (lz4_lg).
'-' is binary stdin/stdout. Without [outfile] the input is replaced
by '<infile>.<ext>'. bzip2 is not built in (usage error).

Examples:
  {prog} compress ramdisk.cpio
  {prog} compress=xz ramdisk.cpio ramdisk.cpio.xz

Exit codes: 0 ok, 1 usage error, 2 broken input / I/O failure.
Details: {prog} compress --expand"
    )
}

pub fn expand_compress(prog: &str) -> String {
    format!(
        "{prog} compress — details (see `{prog} compress --help` for the short form)

  Reads <infile> whole, encodes it and writes [outfile] (stdin/stdout
  locked and flushed). Without [outfile] the input is replaced and the
  original removed only after the new file is fully written.
  Extensions: gzip gz, xz xz, lzma lzma, lz4/lz4_legacy lz4 (legacy
  shares lz4's extension, like magiskboot).
  bzip2 has no pure-Rust backend here: 'compress=bzip2' is a usage
  error (exit 1). Unknown formats fail with magiskboot's message:
  'Unsupported or unknown compression format: <fmt>' (exit 1).
  Pipe example:
    cat ramdisk.cpio | {prog} compress - - | {prog} decompress - -"
    )
}

pub fn short_decompress(prog: &str) -> String {
    format!(
        "{prog} decompress — expand one archive by magic

Usage:
  {prog} decompress <infile> [outfile]
  {prog} decompress --help | --expand

Sniffs the magic, reports 'Detected format: <name>' on stderr and
decodes. '-' is binary stdin/stdout. Without [outfile] the archive
extension is stripped (else 'Input file is not a supported type!').

Examples:
  {prog} decompress ramdisk.cpio.gz
  {prog} decompress ramdisk.cpio.xz out.cpio

Exit codes: 0 ok, 1 usage error, 2 unsupported/corrupt input.
Details: {prog} decompress --expand"
    )
}

pub fn expand_decompress(prog: &str) -> String {
    format!(
        "{prog} decompress — details (see `{prog} decompress --help` for the short form)

  Magic order (magiskboot): gzip 1f 8b / xz fd 37 7a 58 5a 00 /
  lzma-alone 5d + pow2 dict / lz4-frame 03|04 21|22 4c|4d 18 /
  lz4-legacy 02 21 4c 18. Reports 'Detected format: <name>' on stderr.
  No known magic (or a wrong archive
  extension) fails with 'Input file is not a supported type!' (exit 2),
  like magiskboot. A BZh (bzip2) blob fails as unsupported (exit 2):
  no pure-Rust backend is built in — the single deliberate deviation
  from magiskboot."
    )
}
