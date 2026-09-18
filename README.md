# bootsmasher
Standalone static CLI for Android boot-image surgery. Four subprograms
(short aliases in brackets):

- `vboot` [`vb`] — smart `vendor_boot` repair flow (Pixel 6 specialization):
  stale-table normalize, platform replace, `--split-first-stage`
  (`--split`) / `--merge` partition modes, pre-emit verify, stdout mode,
  `--min-free` space gate.
- `unpack` [`u`|`up`] — magiskboot-compatible extraction for `boot.img`
  (v0..v4, kernels, dtb/dtbo) **and** `vendor_boot`, with
  auto-decompression, per-section verdicts and never-die reporting.
- `repack` [`r`|`rp`] — rebuild from an unpack dir: `spec.toml` or
  `--base` (`-b`), `--template` (`-t`), `--set` (`-s`), `--format` (`-f`),
  footer control, pre-write verify.
- `cpio` [`c`] — in-place newc archive surgery, faithful magiskboot
  port: `exists/ls/rm/mkdir/ln/mv/add/extract/test/patch/backup/restore`.

`bootsmasher help [subprogram]` prints a subprogram manual; every
subprogram also answers `--help`. Exit codes everywhere: 0 ok, 1 usage
error, 2 broken input / failed verification.

100% self-contained: pure Rust (`lz4_flex`, `flate2`, `lzma-rust2`,
`serde`/`toml`; `libc` statvfs binding on Unix only), no external
commands, no C code; static musl builds for Linux (x86_64/x86/aarch64/
armv7) and MinGW/LLVM builds for Windows (x86_64/aarch64).

## unpack (`u`, `up`)

```text
bootsmasher unpack <image> [-h] [-n] [-o <dir>] [-x] [--no-spec]
```

Auto-detects `ANDROID!` vs `VNDRBOOT`, dumps magiskboot-compatible names
(`kernel`, `kernel_dtb`, `ramdisk.cpio`, `second`, `extra`,
`recovery_dtbo`, `dtb`, `signature`, `bootconfig`, `header`, plus
`vendor_ramdisk/<name>.cpio`, `footer.bin` and our `spec.toml`):

- `-h` writes the magiskboot `header` file (name/cmdline/os_version);
  `spec.toml` is always written too (full fidelity: formats, sizes,
  board_id, footer) unless `--no-spec`.
- Default decompresses kernel/ramdisk/extra on the fly (gzip/xz/lzma/
  lz4-frame/lz4-legacy sniffed by magic); `-n` keeps original bytes.
- A fragment that fails to decompress is still dumped RAW and flagged
  `INVALID` — the run never aborts like magiskboot does.
- `--extract` expands every usable cpio into `<file>.d/` (files, dirs,
  symlinks, unix modes).
- Broken images get a per-section WHY (`block 6 truncated: need X, have
  Y — table slices one stream mid-block; table sum vs header, diff N`)
  and, for stale-table vendor_boot, a
  `vendor_ramdisk/ramdisk.full-rescue.cpio` with the whole blob as one
  valid stream. Final line `RESULT: OK` (exit 0) or `RESULT: DEGRADED`
  (exit 0; exit 2 only when even the header is unreadable).

## repack (`r`, `rp`)

```text
bootsmasher repack [dir="."] [out="new-boot.img"] [-b <img>] [-t <img>]
                   [-s k=v]... [-f target=fmt]... [-n] [--drop-footer]
                   [--pad-to N] [--min-free S] [--check-dir <dir>]
```

- Layout from `dir/spec.toml`; without it `--base` supplies sizes,
  formats and missing-file bytes (magiskboot parity: only present files
  replace components). Without both, the layout is unknown (exit 1).
- Header scalars: spec/base → `dir/header` (magiskboot parity) →
  `--template` → `--set` (`cmdline|name|os_version|os_patch_level|
  page_size|kernel_addr|ramdisk_addr|second_addr|tags_addr|dtb_addr`).
- Formats per section (`--format ramdisk.cpio=gzip`, groups `ramdisk`/
  `all`), defaulting to spec/base/detected; v4 boot ramdisk is forced to
  `lz4_legacy` (GKI merge rule, like magiskboot); already-compressed
  files pass through verbatim; `-n` skips compression.
- Vendor table rebuilt (offsets rechained, names/types/board_id kept);
  boot `recovery_dtbo` offset refreshed; `kernel_dtb` honored with an
  explicit kernel file.
- Footer kept (`footer.bin`, else base trailing bytes) unless
  `--drop-footer`. Output re-parsed and re-verified in memory; on
  failure nothing is written. Space gate: output dir must fit image +
  `--min-free` (bytes or `512M`).

## cpio (`c`)

```text
bootsmasher cpio <incpio> [commands...]
```

Faithful port of magiskboot's cpio: in-place newc (`070701`) archive
surgery. Each command is one shell-quoted argument; the file is
rewritten after the last command (`ls`/`test`/`exists` only report and
exit without writing; a missing `<incpio>` starts an empty archive).
Input must be raw newc — `unpack` without `-n` already writes it, or
decompress the `.lz4` first.

Commands: `exists ENTRY` (0/1) | `ls [-r] [PATH]` | `rm [-r] ENTRY` |
`mkdir MODE ENTRY` (octal) | `ln TARGET ENTRY` | `mv SOURCE DEST` |
`add MODE ENTRY INFILE` | `extract [ENTRY OUT]` |
`test` (0 stock / 1 Magisk / 2 unsupported) |
`patch` (fstab verify/avb/forceencrypt strip, `KEEPVERITY` /
`KEEPFORCEENCRYPT=true` keeps) | `backup ORIG [-n]` (diff into
`.backup/`, xz unless `-n`) | `restore` (xz entries decompressed back).

```sh
bootsmasher cpio ramdisk.cpio "exists init" "ls -r /system"
bootsmasher c ramdisk.cpio "add 644 new.rc ./new.rc" "ls new.rc"
bootsmasher cpio ramdisk.cpio patch
bootsmasher cpio ramdisk.cpio test; echo $?
```

```sh
bootsmasher unpack vendor_boot.img -o dir -h -x
bootsmasher u boot.img -o dir -n
bootsmasher repack dir fixed.img
bootsmasher r dir fox.img -b stock.img -f ramdisk.cpio=gzip
bootsmasher repack pinit/ init_new.img -s cmdline="console=ttyS0" -n
```

## Why

A broken maintainer `vendor_boot` shipped a single LZ4-legacy ramdisk stream
covering the whole blob while the ramdisk table still described two fragments
(`platform 22682993 + dlkm 7345789`, sum 5388 bytes short of the header).
The bootloader boots it (it finds the DTB via the header size) but
`fastbootd` fragment flashing fails (`Old offset mismatch`) and `magiskboot`
fails with `failed to fill whole buffer`. `bootsmasher vboot` detects exactly
this instead of guessing: every fragment is decompressed and its cpio is
verified, the table sum is checked against the header, and FDTs are walked.

## Usage (`vb`)

```text
bootsmasher vboot <vboot.img> [platform.cpio|platform.cpio.lz4] [out.img]
bootsmasher vboot <vboot.img> [platform] -o <out.img> [--pad-to <bytes>]
bootsmasher vboot --verify <vboot.img> [platform] [--check-dir <dir>]
```

Layout modes (mutually exclusive):
- none — keep the layout (valid images round-trip byte-identically;
  stale tables normalize to one platform entry). With a platform file the
  platform is replaced; a valid original dlkm is kept as fallback,
  otherwise `lib/**` is pulled into a fresh dlkm.
- `--split-first-stage` — partition content by subtree:
  `first_stage_ramdisk/**` + rest → platform, `recovery/**` +
  `debug_ramdisk/**` → recovery, `lib/**` → dlkm. A dlkm/recovery fragment
  is emitted only when it carries real files (a bare `lib` or
  `debug_ramdisk` dir alone is not worth a fragment); with no dlkm payload
  of its own a valid original dlkm is kept byte-identically.
- `--merge` — glue everything into one platform fragment (platform slots
  replaced by the new file when given; original dlkm/recovery content
  joins it; mid-stream TRAILERs are dropped, exactly one is written).

Re-encoded fragments use marker-free LZ4-legacy streams, exactly like
kernel-produced ramdisks (fragments butt against each other; the `lz4` CLI
treats a zero word as corruption, so no end marker is written).

- No platform file: normalize the layout. A stale single-stream table becomes
  one `platform` entry; a valid image round-trips byte-identically (fragment
  bytes are kept verbatim, never recompressed).
- With a platform file (`.lz4` stays verbatim after validation, raw `.cpio`
  is compressed to LZ4-legacy): the platform is replaced. A valid original
  `dlkm` fragment is kept as fallback, otherwise `lib/**` is pulled out of
  the new platform into a fresh `dlkm` fragment. Header flags, cmdline, DTB
  and bootconfig are preserved.
- The rebuilt image is fully re-verified in memory before anything is
  emitted; on failure nothing is written.
- Without an output path the image goes to **stdout** with no other stdout
  output (diagnostics go to stderr). `--pad-to 67108864` pads with zeros to
  the block-device size.
- Before writing, free space is checked: `free(dir)` must cover the image
  plus `--min-free` (default 0; plain bytes or human sizes like `512M`,
  `1GiB`). The checked dir is the output file's parent (or `--check-dir`
  override); for stdout output the check runs only with `--check-dir`.
- `vboot --verify base.img platform.cpio.lz4` dry-runs the whole pipeline
  in memory — resulting layout, resulting size and the space verdict
  (checked dir defaults to `.`) — and writes nothing.
- Exit codes: `0` ok, `1` usage error, `2` broken input / failed verification.

```sh
bootsmasher vboot --verify vendor_boot.img
bootsmasher vb broken_vendor_boot.img -o fixed.img
bootsmasher vboot stock_vendor_boot.img OrangeFox.ramdisk.lz4 -o fox_boot.img
bootsmasher vboot stock_vendor_boot.img --merge -o single.img
bootsmasher vboot broken.img full.cpio --split -o frag.img
bootsmasher vboot broken.img fox.lz4 --pad-to 67108864 --min-free 1G -o fox_64m.img
bootsmasher vboot broken.img > fixed.img
```

## Build

```sh
./build.sh --arch x64        # local fast path
./build.sh --arch all        # 4 Linux musl + 2 Windows (gnu x64, gnullvm arm64)
./build.sh --cross --arch windows
```

Linux targets need musl + cross gcc; Windows x86_64 needs `mingw-w64` and
`rustup target add x86_64-pc-windows-gnu`. Windows ARM64 has no distro
toolchain: its Rust target is `aarch64-pc-windows-gnullvm`
(`aarch64-pc-windows-gnu` does not exist) and it links with llvm-mingw
(mstorsjo), which `build.sh` picks up from `/opt/llvm-mingw`:

```sh
curl -LO https://github.com/mstorsjo/llvm-mingw/releases/download/20240619/llvm-mingw-20240619-ucrt-ubuntu-20.04-x86_64.tar.xz
sudo tar -xf llvm-mingw-20240619-ucrt-ubuntu-20.04-x86_64.tar.xz -C /opt/
sudo mv /opt/llvm-mingw-20240619-ucrt-ubuntu-20.04-x86_64 /opt/llvm-mingw
rustup target add aarch64-pc-windows-gnullvm
```

(`cross` has no container image for `aarch64-pc-windows-gnullvm` and falls
back to host cargo, which is exactly the path above.) Outputs land in `dist/`:
4 static musl ELFs + 2 Windows PEs, all verified from a clean tree with
zero `rustc` warnings.

## Tests

```sh
cargo build --release
./test_vboot.sh              # needs fixture images, skips when absent
```

Fixtures (override via `$BROKEN`, `$STOCK`, `$FOX`):
broken Pixel 6 `vendor_boot`, LOS `raven` stock `vendor_boot`,
`OrangeFox-R12.0-test_1-gs101.ramdisk.lz4`. The suite checks verify
verdicts, byte-identical round-trips, verbatim dlkm keep, stdout purity,
`--pad-to`, plus an optional `magiskboot` cross-check.

## Layout

```text
Cargo.toml            lz4_flex + flate2 + lzma-rust2 + serde/toml (+ libc statvfs on Unix);
                      release: LTO fat, abort, strip
build.sh              static multi-arch builder (linux musl x4 + windows gnu x2)
src/main.rs           subprogram dispatch (vboot | unpack | repack | cpio)
src/error.rs          error type (Usage/Fail/Io/Parse/Verify), stderr-only diagnostics
src/bootimg.rs        ANDROID! v0..v4 parse/serialize, kernel_dtb split
src/codec.rs          gzip/xz/lzma/lz4-frame/lz4-legacy sniff + transcode
src/cpiox.rs          cpio-to-directory extraction (safe paths, symlinks, modes)
src/cpio_cmd.rs       cpio subprogram (magiskboot port: 12 in-place commands)
src/spec.rs           spec.toml layout record (serde/toml, hex board_id)
src/unpack.rs         unpack subprogram (boot + vendor, diagnose, rescue)
src/repack.rs         repack subprogram (spec/base/template/set/format/footer)
src/vboot/mod.rs      vboot CLI (positional + -o/--out/--pad-to/--verify/--split-first-stage/--merge/--min-free/--check-dir)
src/vboot/image.rs    structures of the vendor_boot v3/v4 header and table
src/vboot/lz4legacy.rs  marker-free LZ4-legacy framing over lz4_flex block codec
src/vboot/cpio.rs     newc parse/build/partition (lib/** = dlkm, recovery|debug_ramdisk/** = recovery), 512-pad tolerant
src/vboot/dtb.rs      concatenated-FDT walker
src/vboot/ops.rs      analyzer + repack (Keep/Split/Merge) + pre-emit verifier + unpack diagnosis
src/vboot/space.rs    size parsing + pre-write free-space check (statvfs / GetDiskFreeSpaceExW)
test_vboot.sh         vboot suite (21 checks incl. 16K carryover), test_compare_dlkm.py helper
test_unpack_repack.sh unpack/repack/cpio suite (30 checks: GKI boot, init_boot,
                      recovery vendor_boot, roundtrips, -n identity, base
                      fallback, set/format, refuse-invalid, template, space,
                      edit-wins, aliases, cpio patch/backup/restore)
```

## License

MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
