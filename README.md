# bootsmasher

Standalone static CLI for Android boot-image surgery. Subprogram `vboot` (v0.1.0)
analyzes and repacks `vendor_boot` v3/v4 images, specialized for Pixel 6
(gs101, page size 2048). 100% self-contained: pure Rust (`lz4_flex` only),
no external commands, no C dependencies; static musl builds for Linux
(x86_64/x86/aarch64/armv7) and MinGW builds for Windows (x86_64/aarch64).

## Why

A broken maintainer `vendor_boot` shipped a single LZ4-legacy ramdisk stream
covering the whole blob while the ramdisk table still described two fragments
(`platform 22682993 + dlkm 7345789`, sum 5388 bytes short of the header).
The bootloader boots it (it finds the DTB via the header size) but
`fastbootd` fragment flashing fails (`Old offset mismatch`) and `magiskboot`
fails with `failed to fill whole buffer`. `bootsmasher vboot` detects exactly
this instead of guessing: every fragment is decompressed and its cpio is
verified, the table sum is checked against the header, and FDTs are walked.

## Usage

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
bootsmasher vboot broken_vendor_boot.img -o fixed.img
bootsmasher vboot stock_vendor_boot.img OrangeFox.ramdisk.lz4 -o fox_boot.img
bootsmasher vboot stock_vendor_boot.img --merge -o single.img
bootsmasher vboot broken.img full.cpio --split-first-stage -o frag.img
bootsmasher vboot broken.img fox.lz4 --pad-to 67108864 --min-free 1G -o fox_64m.img
bootsmasher vboot broken.img > fixed.img
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
Cargo.toml            lz4_flex only (+ libc statvfs binding on Unix); release: LTO fat, abort, strip
build.sh              static multi-arch builder (linux musl x4 + windows gnu x2)
src/main.rs           subprogram dispatch (vboot in 0.1.0)
src/error.rs          single error type, stderr-only diagnostics
src/vboot/mod.rs      vboot CLI (positional + -o/--out/--pad-to/--verify/--split-first-stage/--merge/--min-free/--check-dir)
src/vboot/image.rs    vendor_boot v3/v4 header + ramdisk table structs
src/vboot/lz4legacy.rs  marker-free LZ4-legacy framing over lz4_flex block codec
src/vboot/cpio.rs     newc parse/build/partition (lib/** = dlkm, recovery|debug_ramdisk/** = recovery), 512-pad tolerant
src/vboot/dtb.rs      concatenated-FDT walker
src/vboot/ops.rs      analyzer + repack (Keep/Split/Merge) + pre-emit verifier
src/vboot/space.rs    size parsing + pre-write free-space check (statvfs / GetDiskFreeSpaceExW)
test_vboot.sh         functional suite (20 checks), test_compare_dlkm.py helper
```

## License

MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).
