//! `vboot` help texts. `{prog}` is the argv[0] basename at runtime.

pub fn short(prog: &str) -> String {
    format!(
        "{prog} vboot — smart vendor_boot repair flow (Pixel 6 / gs101)

Usage:
  {prog} vboot <vboot.img> [platform.cpio|platform.cpio.lz4] [out.img]
  {prog} vboot <vboot.img> [platform] -o <out.img> [--pad-to <bytes>]
  {prog} vboot <vboot.img> --recovery <fox.cpio|fox.cpio.lz4> [out.img]
  {prog} vboot --verify <vboot.img> [platform] [--check-dir <dir>]
  {prog} vboot --help | --expand

What it does:
  Verifies every ramdisk fragment (decompress + cpio check), the table
  sum against the header and the FDTs; repairs a stale single-stream
  table. Valid images round-trip byte-identically, never recompressed.

Layout modes (mutually exclusive):
  (none)              keep layout; a stale table becomes one platform entry
  --split-first-stage split content by subtree: platform / recovery / dlkm
  --merge             glue everything into one platform fragment
  --drop <sel,...>    drop original fragments by type or name
                      (platform, dlkm, recovery, none, 16K, ...; repeatable,
                      comma-separated, case-sensitive)
  --recovery <file>   recovery-install layout: the file becomes the
                      recovery fragment, the vendor blobs detach from it
                      (see expand). Conflicts with a platform file and
                      with --split-first-stage/--merge.
  --recovery-is-platform var1|var2
                      recovery-in-platform test layouts (install Type B/C):
                      the --recovery file rides the platform fragment, no
                      recovery fragment is emitted. var1 (Type B):
                      platform = native first_stage + file; var2 (Type C):
                      platform = file alone (must carry first_stage
                      itself). Needs --recovery; conflicts with a platform
                      file, --split-first-stage/--merge, --drop
                      first-stage.
  --drop-footer      omit the trailing vbmeta/AVB/padding tail (needed
                      when the new content outgrows the partition;
                      recovery install drops it, unlocked bootloader
                      required).
  -s k=v              header override, repeatable (cmdline, name,
                      page_size, kernel_addr, ramdisk_addr, tags_addr,
                      dtb_addr; applied last, like repack --set)

Fragments may be lz4-legacy, raw cpio, gzip, xz, lzma or lz4-frame;
the footer (vbmeta/AVB tail) and board_id values are preserved.

First-stage rule (with a platform file): the base image's own
first_stage_ramdisk/** wins by default; --drop first-stage inverts it.

Output: file (-o/--out or positional) or a pure stdout pipe; --pad-to
pads with zeros; --min-free reserves space. The rebuilt image is
re-verified in memory before anything is written.

Examples:
  {prog} vboot --verify vendor_boot.img
  {prog} vboot broken.img -o fixed.img
  {prog} vboot stock.img fox.ramdisk.lz4 -o fox_boot.img
  {prog} vboot stock.img --recovery fox.ramdisk.lz4 -o fox_rec.img
  {prog} vboot broken.img > fixed.img

Exit codes: 0 ok, 1 usage error, 2 broken input / failed verification.
Details: {prog} vboot --expand"
    )
}

pub fn expand(prog: &str) -> String {
    format!(
        "{prog} vboot — details (see `{prog} vboot --help` for the short form)

Problem it solves:
  A broken maintainer vendor_boot shipped a single LZ4-legacy ramdisk
  stream covering the whole blob while the table still described two
  fragments. The bootloader boots it (DTB via the header size) but
  fragment flashing fails and unpackers die mid-block. vboot detects
  exactly this instead of guessing: table sum vs header, per-fragment
  decompression + cpio validation, FDT walk.

Modes in detail:
  Keep (no flag): valid images pass through verbatim. With a platform
    file the platform is replaced; a valid original dlkm is kept as
    fallback, otherwise lib/** is pulled out of the new platform into
    a fresh dlkm fragment. Other valid original fragments (16K,
    recovery) carry over verbatim, rechained — never silently dropped.
  --split-first-stage: first_stage_ramdisk/** + rest -> platform,
    recovery/** + debug_ramdisk/** -> recovery (name=recovery,
    type=2), lib/** -> dlkm. A dlkm/recovery fragment is emitted only
    with real files (a bare lib dir alone is not worth a fragment).
  --merge: one platform fragment; mid-stream TRAILERs dropped, exactly
    one written. Re-encoded fragments are marker-free LZ4-legacy, like
    kernel ramdisks (no end marker: the lz4 CLI reads a zero word as
    corruption).
  --drop: Keep skips the selectors (survivors rechain); Split/Merge
    leave them out of the pool, carryover and dlkm fallback. A selector
    matching nothing warns on stderr and is ignored. first-stage is
    special (see below).

Recovery-install (--recovery <file>):
  Detaches the vendor blobs from recovery and installs the file as the
  recovery fragment. Output: platform = native first_stage_ramdisk/**
  only, harvested from the whole kept base (wherever it lives, so an
  old recovery holding first-stage can never brick the device when it
  is replaced); recovery = the file minus first-stage duplicates
  (verbatim when it has none); dlkm = a valid original dlkm verbatim,
  else lib/** pulled out of the base pool, else nothing; any other
  valid non-platform original (16K, ...) carries over verbatim, old
  recovery-type originals are always replaced. Refuses when the base
  has no first-stage or the file yields no recovery payload. Footer:
  the vbmeta/AVB tail is preserved unless --drop-footer; a grown
  ramdisk plus the old footer may exceed the partition — then drop
  the footer (unlocked bootloader required).

Recovery-in-platform (--recovery-is-platform var1|var2, install Type B/C):
  Test layouts for merged-platform stocks (gs101: no boot.img ramdisk,
  platform is the only ramdisk on normal boot). No recovery fragment.
  var1 (Type B): platform = native first_stage_ramdisk/** (refused when
  absent) + file (file first-stage duplicates dropped, inbuild wins);
  dlkm = valid original verbatim, else lib/** from the base pool.
  var2 (Type C): platform = file alone (refused when the file carries no
  first_stage_ramdisk/** — build a var2-AIO payload first); dlkm =
  valid original verbatim (matches the stock kernel), else lib/** from
  the file, else none. Old recovery-type originals are dropped in both
  (replaced by the file in platform); other fragments carry over.

First-stage rule, full form:
  With a platform file the base's own first_stage_ramdisk/** entries go
  first and the cpio's first-stage entries are dropped. Add first-stage
  to --drop to invert (the cpio wins, inbuild is dropped). Without a
  platform file --drop first-stage strips first_stage_ramdisk/** from
  the pool. Root-level init is ordinary platform payload, not
  first-stage.

Platform file formats:
  platform.cpio.lz4 stays verbatim after validation; raw platform.cpio
  is compressed to LZ4-legacy; gzip/xz/lzma/lz4 platform files stay
  verbatim after validation too. Anything else is a usage error, never
  guessed. Header flags, cmdline, DTB, bootconfig, footer and board_id
  values are preserved (fresh fragments inherit board_id from the kept
  originals of the same type).

Header overrides:
  -s k=v (repeatable, applied last): cmdline, name, page_size,
  kernel_addr, ramdisk_addr, tags_addr, dtb_addr. Unknown keys and
  boot-only keys (os_version) are usage errors.

Lossy corners (warned on stderr, never silent):
  Merge skips original fragments it cannot decode (they join the union
  only when readable). --drop removing every fragment is refused
  (usage error); an image with zero fragments is never emitted.
  Whole-blob rescue cannot honor fragment selectors (no boundaries in
  a single stream) — warned when taken.

Space gate:
  free(dir) must cover the image plus --min-free (default 0; plain
  bytes or human sizes like 512M, 1GiB). Checked dir: output file's
  parent, or --check-dir; stdout output checks only with --check-dir.

Verify:
  No platform file: fragment table verdict (exit 0 only if fully
  self-consistent). With a platform file or --drop: dry-run of the
  whole pipeline in memory (layout + size + space verdict), writes
  nothing."
    )
}
