//! `bootsmasher repack` — rebuild boot/vendor_boot from an unpack dir.
//!
//! Layout comes from dir/spec.toml, else --base (magiskboot parity: only
//! files present replace components). Header scalars come from
//! spec/base, then dir/header (magiskboot parity), then --template, then
//! --set. Components compress back to their recorded/detected format
//! unless already compressed (magiskboot parity) or -n. The result is
//! re-verified in memory before anything is written.

use std::path::{Path, PathBuf};

use crate::bootimg;
use crate::codec::{self, Format};
use crate::error::{Error, Result};
use crate::spec;
use crate::vboot::cpio;
use crate::vboot::dtb;
use crate::vboot::image::{Header, RamdiskEntry, TYPE_DLKM, TYPE_PLATFORM, TYPE_RECOVERY};
use crate::vboot::ops;
use crate::vboot::space;

const HELP: &str = "bootsmasher repack — rebuild boot/vendor_boot from an unpack dir
Aliases: r, rp.

Usage:
  bootsmasher repack [dir=\".\"] [out=\"new-boot.img\"] [options]
  bootsmasher repack --help

Layout source (need one of; checked in this order):
  dir/spec.toml            Lean record from 'bootsmasher unpack': kind,
                           version, page, header scalars, per-section
                           file/format/size (+ names/types/board_id for
                           vendor ramdisks). A repack from the dir alone
                           works when every nonzero section has its file.
                           Sizes in spec are advisory: all header sizes
                           and table offsets are recomputed from the real
                           file bytes, so editing files never breaks the
                           build (spec sizes are only used in the
                           'missing file' error text).
  --base <img> (-b)        Original image (magiskboot parity): sizes,
                           formats and missing-file bytes come from it.
                           Without spec the table names/types come from
                           the base image table.
  --template <img> (-t)    Foreign image: header scalars (cmdline, name,
                           addrs, page, os_version...) are taken from it;
                           section bytes still come from dir/--base.
                           Example: LOS ramdisk + stock cmdline.

Options:
  -o <file>, --out <file>  Output image path. Default: positional [out],
                           default new-boot.img. -o and positional out
                           together are an error.
  -b <img>, --base <img>   See above.
  -t <img>, --template <img>  See above.
  -s k=v, --set k=v        Header scalar override, repeatable, wins over
                           everything except a later --set. Keys:
                             cmdline        free text (may contain spaces
                                            if quoted by the shell)
                             name           product name (<= 16 chars)
                             os_version     A.B.C, 7-bit parts (boot only)
                             os_patch_level Y-MM, e.g. 2026-09 (boot only)
                             page_size      2048 / 4096 (decimal or 0x...)
                             kernel_addr | ramdisk_addr | second_addr |
                             tags_addr      32-bit load addresses
                             dtb_addr       64-bit load address
                           Unknown keys and boot-only keys on vendor_boot
                           are usage errors (exit 1), never silent.
  -f target=fmt, --format target=fmt
                           Compression target, repeatable. Target is a file
                           name (kernel, ramdisk.cpio, dlkm.cpio, dtb...)
                           or a group: 'ramdisk' (every ramdisk fragment)
                           or 'all'. Formats:
                             raw | gzip | xz | lzma | lz4 | lz4_legacy
                           ('none'/'cpio' also mean raw; 'lz4_lg' means
                           lz4_legacy). Precedence per section: exact file
                           match > ramdisk-group > all > spec
                           stored_format > --base detected format > raw.
  -n                       Skip all compression: every present file is
                           copied verbatim (magiskboot parity for -n).
                           Unpack -n + repack -n is byte-identical to the
                           source prefix.
  --drop-footer            Omit trailing bytes (dir/footer.bin or base
                           tail). Shrinks the image; use with --pad-to to
                           re-pad to the block-device size.
  --pad-to <bytes>         Append zeros up to this size (e.g. 67108864).
                           Fails (nothing written) if the image is bigger.
  --min-free <size>        Reserve: output dir must fit image + reserve.
                           Plain bytes or human (512M, 1GiB, 1.5G; K/M/G/T
                           are binary). Default 0.
  --check-dir <dir>        Check free space in <dir> instead of the output
                           file's parent directory.

Header scalar precedence (later wins):
  spec.toml (or --base) -> dir/header (magiskboot parity: name, cmdline,
  os_version, os_patch_level) -> --template -> --set k=v.

Component formats in detail:
  A file that already sniffs as compressed is copied verbatim
  (magiskboot parity); a raw cpio/text file is compressed to the target
  format. v4 boot ramdisk is forced to lz4_legacy like magiskboot does
  (GKI merge rule: vendor ramdisks must share one method), unless -n or
  an explicit --format says otherwise — the forcing is reported as
  'RAMDISK_FMT: [old] -> [lz4_legacy]'.

Vendor_boot specifics:
  The ramdisk table is rebuilt from scratch: offsets rechained from the
  new blob sizes, types/names from spec (or --base table), board_id from
  spec board_id_hex (128 hex chars = 64 bytes, absent = zeros).
  dtb/bootconfig come from files, else --base bytes, else a clean error
  naming the missing size.

Boot specifics:
  kernel + kernel_dtb files concatenate (kernel_dtb is honored only with
  an explicit kernel file; otherwise a warning, base bytes kept). The
  recovery_dtbo offset field is refreshed to the real position. v4
  signature comes from the file, else --base bytes. dtb is NOT split out
  of kernel on repack (only on unpack).

Footer and AVB:
  Kept by default: dir/footer.bin (written by unpack, not recorded in
  spec), else --base trailing bytes (vbmeta + AVB footer). --drop-footer
  omits it. AVB hashes never survive content changes anyway — resign the
  image afterwards if the verified-boot chain matters.

Output:
  Always a file (default new-boot.img). --pad-to appends zeros. Free
  space is checked first (output parent or --check-dir must fit image +
  --min-free). The rebuilt image is re-parsed and every section
  re-verified in memory (ramdisk cpio, FDTs, table chaining); on failure
  nothing is written (exit 2).

Typical sessions:
  bootsmasher repack dir fixed.img
  bootsmasher r dir fox.img -b stock.img -f ramdisk.cpio=gzip
  bootsmasher repack pinit/ init_new.img --set cmdline=\"console=ttyS0\" -n
  bootsmasher repack dir out.img --drop-footer --pad-to 67108864
  bootsmasher repack broken-dir/ out.img --base broken.img
    # refuses (exit 2): refuses to emit an invalid image

Exit codes: 0 ok, 1 usage error, 2 broken input / failed verification.";

pub fn run(args: &[String]) -> i32 {
    match run_inner(args) {
        Ok(()) => 0,
        Err(Error::Usage(m)) => {
            eprintln!("usage error: {m}\n{HELP}");
            1
        }
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

struct Cli {
    dir: PathBuf,
    out: String,
    base: Option<String>,
    template: Option<String>,
    sets: Vec<(String, String)>,
    formats: Vec<(String, Format)>,
    no_compress: bool,
    drop_footer: bool,
    pad_to: Option<usize>,
    min_free: u64,
    check_dir: Option<String>,
}

fn parse_cli(args: &[String]) -> Result<Cli> {
    let mut dir: Option<String> = None;
    let mut out: Option<String> = None;
    let mut base = None;
    let mut template = None;
    let mut sets = Vec::new();
    let mut formats = Vec::new();
    let mut no_compress = false;
    let mut drop_footer = false;
    let mut pad_to = None;
    let mut min_free = 0u64;
    let mut check_dir = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Err(Error::Usage("help requested".to_string())),
            "-n" => no_compress = true,
            "--drop-footer" => drop_footer = true,
            "-o" | "--out" => {
                i += 1;
                let v = args.get(i).ok_or_else(|| Error::Usage("-o/--out needs a value".to_string()))?.clone();
                if out.is_some() {
                    return Err(Error::Usage("out.img positionally and via -o/--out at once".to_string()));
                }
                out = Some(v);
            }
            "-b" | "--base" => {
                i += 1;
                base = Some(args.get(i).ok_or_else(|| Error::Usage("-b/--base needs a value".to_string()))?.clone());
            }
            "-t" | "--template" => {
                i += 1;
                template = Some(
                    args.get(i).ok_or_else(|| Error::Usage("-t/--template needs a value".to_string()))?.clone(),
                );
            }
            "-s" | "--set" => {
                i += 1;
                let kv = args.get(i).ok_or_else(|| Error::Usage("-s/--set needs k=v".to_string()))?;
                let (k, v) = kv.split_once('=').ok_or_else(|| Error::Usage("-s/--set needs k=v".to_string()))?;
                sets.push((k.to_string(), v.to_string()));
            }
            "-f" | "--format" => {
                i += 1;
                let kv = args.get(i).ok_or_else(|| Error::Usage("-f/--format needs target=fmt".to_string()))?;
                let (t, f) = kv.split_once('=').ok_or_else(|| Error::Usage("-f/--format needs target=fmt".to_string()))?;
                formats.push((t.to_string(), Format::parse(f)?));
            }
            "--pad-to" => {
                i += 1;
                let v = args.get(i).ok_or_else(|| Error::Usage("--pad-to needs a value".to_string()))?;
                pad_to = Some(v.parse::<usize>().map_err(|_| Error::Usage("--pad-to needs an integer".to_string()))?);
            }
            "--min-free" => {
                i += 1;
                let v = args.get(i).ok_or_else(|| Error::Usage("--min-free needs a value".to_string()))?;
                min_free = crate::vboot::space::parse_size(v)?;
            }
            "--check-dir" => {
                i += 1;
                check_dir = Some(
                    args.get(i).ok_or_else(|| Error::Usage("--check-dir needs a value".to_string()))?.clone(),
                );
            }
            s if s.starts_with('-') => return Err(Error::Usage(format!("unknown flag {s}"))),
            p => {
                if dir.is_none() {
                    dir = Some(p.to_string());
                } else if out.is_none() {
                    out = Some(p.to_string());
                } else {
                    return Err(Error::Usage(
                        "too many positional arguments (out.img positionally and via -o/--out at once?)".to_string(),
                    ));
                }
            }
        }
        i += 1;
    }
    Ok(Cli {
        dir: PathBuf::from(dir.unwrap_or_else(|| ".".to_string())),
        out: out.unwrap_or_else(|| "new-boot.img".to_string()),
        base,
        template,
        sets,
        formats,
        no_compress,
        drop_footer,
        pad_to,
        min_free,
        check_dir,
    })
}

fn emit_err(line: &str) {
    eprintln!("{line}");
}

fn dir_file(dir: &Path, file: &str) -> Option<Vec<u8>> {
    std::fs::read(dir.join(file)).ok()
}

/// Resolve one component's image bytes.
///
/// `target`: format to produce when compressing. `on_disk_raw`: the dir
/// file already holds image bytes (spec on_disk=raw) -> verbatim.
/// Otherwise: missing file -> `fallback` (base bytes); present file that
/// already sniffs compressed -> verbatim (magiskboot parity); present raw
/// file -> compress to target unless -n (then verbatim + note).
fn resolve_component(
    dir: &Path,
    file: &str,
    target: Format,
    on_disk_raw: bool,
    no_compress: bool,
    fallback: Option<Vec<u8>>,
    declared: u32,
) -> Result<(Vec<u8>, String)> {
    match dir_file(dir, file) {
        Some(data) => {
            let n = data.len();
            if on_disk_raw || no_compress {
                return Ok((data, format!("{file}: verbatim ({n} bytes)")));
            }
            let sniffed = codec::sniff(&data);
            if sniffed.is_compressed() {
                Ok((data, format!("{file}: already {}, copied verbatim", sniffed.name())))
            } else if !target.is_compressed() {
                Ok((data, format!("{file}: raw ({n} bytes)")))
            } else {
                let enc = codec::compress(target, &data)?;
                let n2 = enc.len();
                Ok((enc, format!("{file}: raw -> {} ({n} -> {n2} bytes)", target.name())))
            }
        }
        None => match fallback {
            Some(b) => Ok((b, format!("{file}: missing, reused base bytes"))),
            None if declared == 0 => Ok((Vec::new(), format!("{file}: absent (size 0)"))),
            None => Err(Error::Parse(format!(
                "{file}: missing and no --base to take {declared} bytes from"
            ))),
        },
    }
}

/// Format target precedence: --format file > --format ramdisk-group/all >
/// recorded stored_format > Raw.
fn target_format(
    formats: &[(String, Format)],
    file: &str,
    is_ramdisk: bool,
    recorded: &str,
) -> Format {
    if let Some((_, f)) = formats.iter().find(|(t, _)| t == file) {
        return *f;
    }
    if is_ramdisk {
        if let Some((_, f)) = formats.iter().find(|(t, _)| t == "ramdisk") {
            return *f;
        }
    }
    if let Some((_, f)) = formats.iter().find(|(t, _)| t == "all") {
        return *f;
    }
    Format::parse(recorded).unwrap_or(Format::Raw)
}

fn run_inner(args: &[String]) -> Result<()> {
    let cli = parse_cli(args)?;
    if !cli.dir.is_dir() {
        return Err(Error::Usage(format!("{} is not a directory (unpack first?)", cli.dir.display())));
    }
    let spec = match spec::read_spec(&cli.dir) {
        Ok(s) => Some(s),
        Err(_) if cli.base.is_some() || cli.template.is_some() => None,
        Err(e) => return Err(e),
    };
    let base_bytes = match &cli.base {
        Some(p) => Some(std::fs::read(p).map_err(|e| Error::Io(format!("cannot read base {p}: {e}")))?),
        None => None,
    };
    let template_bytes = match &cli.template {
        Some(p) => Some(std::fs::read(p).map_err(|e| Error::Io(format!("cannot read template {p}: {e}")))?),
        None => None,
    };
    let kind = match (&spec, &base_bytes, &template_bytes) {
        (Some(s), _, _) => s.image.kind.clone(),
        (None, Some(b), _) => detect_kind(b)?,
        (None, None, Some(t)) => detect_kind(t)?,
        (None, None, None) => {
            return Err(Error::Usage("no spec.toml and no --base/--template: layout unknown".to_string()))
        }
    };
    let out = match kind.as_str() {
        "vendor_boot" => repack_vendor(&cli, spec.as_ref(), base_bytes.as_deref(), template_bytes.as_deref())?,
        "boot" => repack_boot(&cli, spec.as_ref(), base_bytes.as_deref(), template_bytes.as_deref())?,
        k => return Err(Error::Parse(format!("spec kind '{k}' unknown (want boot|vendor_boot)"))),
    };

    let mut out = out;
    if let Some(pad) = cli.pad_to {
        if out.len() > pad {
            return Err(Error::Usage(format!("image {} bytes exceeds --pad-to {pad}", out.len())));
        }
        out.resize(pad, 0);
    }
    let dir = match &cli.check_dir {
        Some(d) => d.clone(),
        None => match Path::new(&cli.out).parent() {
            Some(par) if !par.as_os_str().is_empty() => par.to_string_lossy().into_owned(),
            _ => ".".to_string(),
        },
    };
    space::ensure_space(Path::new(&dir), out.len() as u64, cli.min_free)?;
    std::fs::write(&cli.out, &out).map_err(|e| Error::Io(format!("cannot write {}: {e}", cli.out)))?;
    emit_err(&format!("wrote {} ({} bytes)", cli.out, out.len()));
    Ok(())
}

fn detect_kind(bytes: &[u8]) -> Result<String> {
    if bytes.len() >= 8 && bytes[0..8] == *b"VNDRBOOT" {
        Ok("vendor_boot".to_string())
    } else if bytes.len() >= 8 && bytes[0..8] == *b"ANDROID!" {
        Ok("boot".to_string())
    } else {
        Err(Error::Parse("base/template image is neither VNDRBOOT nor ANDROID!".to_string()))
    }
}

// ---------------- shared header-scalar handling ----------------

/// os_version A.B.C [+ keep patch] encode (magiskboot parity).
fn encode_os_version(cur: u32, v: &str) -> Result<u32> {
    let mut it = v.split('.');
    let a: u32 = it.next().ok_or_else(|| Error::Usage("--set os_version needs A.B.C".to_string()))?.parse().map_err(|_| Error::Usage("--set os_version needs A.B.C".to_string()))?;
    let b: u32 = it.next().ok_or_else(|| Error::Usage("--set os_version needs A.B.C".to_string()))?.parse().map_err(|_| Error::Usage("--set os_version needs A.B.C".to_string()))?;
    let c: u32 = it.next().ok_or_else(|| Error::Usage("--set os_version needs A.B.C".to_string()))?.parse().map_err(|_| Error::Usage("--set os_version needs A.B.C".to_string()))?;
    if a > 127 || b > 127 || c > 127 {
        return Err(Error::Usage("--set os_version parts must fit in 7 bits".to_string()));
    }
    Ok((((a << 14) | (b << 7) | c) << 11) | (cur & 0x7ff))
}

fn encode_patch_level(cur: u32, v: &str) -> Result<u32> {
    let (y, m) = v.split_once('-').ok_or_else(|| Error::Usage("--set os_patch_level needs Y-MM".to_string()))?;
    let y: u32 = y.parse().map_err(|_| Error::Usage("--set os_patch_level needs Y-MM".to_string()))?;
    let m: u32 = m.parse().map_err(|_| Error::Usage("--set os_patch_level needs Y-MM".to_string()))?;
    if y < 2000 || m > 12 {
        return Err(Error::Usage("--set os_patch_level needs Y-MM like 2026-09".to_string()));
    }
    Ok(((cur >> 11) << 11) | ((y - 2000) << 4) | m)
}

fn parse_u32(v: &str, key: &str) -> Result<u32> {
    let t = v.trim();
    let (radix, digits) = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (16, h)
    } else {
        (10, t)
    };
    u32::from_str_radix(digits, radix)
        .map_err(|_| Error::Usage(format!("--set {key} needs an integer, got '{v}'")))
}

fn parse_u64(v: &str, key: &str) -> Result<u64> {
    let t = v.trim();
    let (radix, digits) = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (16, h)
    } else {
        (10, t)
    };
    u64::from_str_radix(digits, radix)
        .map_err(|_| Error::Usage(format!("--set {key} needs an integer, got '{v}'")))
}

/// magiskboot-compatible dir/header file (name/cmdline/os_version/...).
fn apply_header_file_boot(h: &mut bootimg::BootHeader, dir: &Path) -> Result<()> {
    let text = match std::fs::read_to_string(dir.join("header")) {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };
    for line in text.lines() {
        let (k, v) = match line.split_once('=') {
            Some(p) => p,
            None => continue,
        };
        match k.trim() {
            "name" => {
                h.name = v.as_bytes().to_vec();
            }
            "cmdline" => {
                let b = v.as_bytes();
                if h.version < 3 {
                    let (a, rest) = b.split_at(b.len().min(512));
                    h.cmdline = a.to_vec();
                    h.extra_cmdline = rest.to_vec();
                } else {
                    h.cmdline = b.to_vec();
                }
            }
            "os_version" => h.os_version = encode_os_version(h.os_version, v.trim())?,
            "os_patch_level" => h.os_version = encode_patch_level(h.os_version, v.trim())?,
            _ => {}
        }
    }
    emit_err("header: applied dir/header overrides");
    Ok(())
}

fn apply_header_file_vendor(h: &mut Header, dir: &Path) {
    let text = match std::fs::read_to_string(dir.join("header")) {
        Ok(t) => t,
        Err(_) => return,
    };
    for line in text.lines() {
        let (k, v) = match line.split_once('=') {
            Some(p) => p,
            None => continue,
        };
        match k.trim() {
            "name" => h.name = fixed_bytes(v.as_bytes(), 16).try_into().unwrap(),
            "cmdline" => h.cmdline = fixed_bytes(v.as_bytes(), 2048).try_into().unwrap(),
            _ => {}
        }
    }
    emit_err("header: applied dir/header overrides");
}

fn fixed_bytes(b: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let n = b.len().min(len);
    out[..n].copy_from_slice(&b[..n]);
    out
}

fn apply_sets_boot(h: &mut bootimg::BootHeader, sets: &[(String, String)]) -> Result<()> {
    for (k, v) in sets {
        match k.as_str() {
            "cmdline" => {
                if h.version < 3 {
                    let b = v.as_bytes();
                    let (a, rest) = b.split_at(b.len().min(512));
                    h.cmdline = a.to_vec();
                    h.extra_cmdline = rest.to_vec();
                } else {
                    h.cmdline = v.as_bytes().to_vec();
                }
            }
            "name" => h.name = v.as_bytes().to_vec(),
            "os_version" => h.os_version = encode_os_version(h.os_version, v)?,
            "os_patch_level" => h.os_version = encode_patch_level(h.os_version, v)?,
            "page_size" => h.page_size = parse_u32(v, k)?,
            "kernel_addr" => h.kernel_addr = parse_u32(v, k)?,
            "ramdisk_addr" => h.ramdisk_addr = parse_u32(v, k)?,
            "second_addr" => h.second_addr = parse_u32(v, k)?,
            "tags_addr" => h.tags_addr = parse_u32(v, k)?,
            "dtb_addr" => h.dtb_addr = parse_u64(v, k)?,
            _ => return Err(Error::Usage(format!("--set: unknown key '{k}'"))),
        }
    }
    Ok(())
}

fn apply_sets_vendor(h: &mut Header, sets: &[(String, String)]) -> Result<()> {
    for (k, v) in sets {
        match k.as_str() {
            "cmdline" => h.cmdline = fixed_bytes(v.as_bytes(), 2048).try_into().unwrap(),
            "name" => h.name = fixed_bytes(v.as_bytes(), 16).try_into().unwrap(),
            "page_size" => h.page_size = parse_u32(v, k)?,
            "kernel_addr" => h.kernel_addr = parse_u32(v, k)?,
            "ramdisk_addr" => h.ramdisk_addr = parse_u32(v, k)?,
            "tags_addr" => h.tags_addr = parse_u32(v, k)?,
            "dtb_addr" => h.dtb_addr = parse_u64(v, k)?,
            "os_version" | "os_patch_level" => {
                return Err(Error::Usage(format!("--set {k} is boot-only (vendor_boot has no os_version)")))
            }
            _ => return Err(Error::Usage(format!("--set: unknown key '{k}'"))),
        }
    }
    Ok(())
}

// ---------------- vendor_boot repack ----------------

fn spec_vendor_header(s: &spec::ImageSpec) -> Result<Header> {
    let mut cmdline = [0u8; 2048];
    if let Some(c) = &s.cmdline {
        let b = c.as_bytes();
        cmdline[..b.len().min(2048)].copy_from_slice(&b[..b.len().min(2048)]);
    }
    let mut name = [0u8; 16];
    if let Some(n) = &s.name {
        let b = n.as_bytes();
        name[..b.len().min(16)].copy_from_slice(&b[..b.len().min(16)]);
    }
    Ok(Header {
        header_version: s.header_version,
        page_size: s.page_size,
        kernel_addr: s.kernel_addr.unwrap_or(0),
        ramdisk_addr: s.ramdisk_addr.unwrap_or(0),
        ramdisk_size: 0,
        cmdline,
        tags_addr: s.tags_addr.unwrap_or(0),
        name,
        header_size: s.header_size.unwrap_or(2128),
        dtb_size: 0,
        dtb_addr: s.dtb_addr.unwrap_or(0),
        table_size: 0,
        table_entry_num: 0,
        table_entry_size: 108,
        bootconfig_size: s.bootconfig_size.unwrap_or(0),
    })
}

fn etype_of(s: &str) -> Result<u32> {
    match s {
        "platform" => Ok(TYPE_PLATFORM),
        "recovery" => Ok(TYPE_RECOVERY),
        "dlkm" => Ok(TYPE_DLKM),
        "none" => Ok(0),
        _ => Err(Error::Parse(format!("spec ramdisk type '{s}' unknown"))),
    }
}

fn repack_vendor(
    cli: &Cli,
    spec: Option<&spec::Spec>,
    base: Option<&[u8]>,
    template: Option<&[u8]>,
) -> Result<Vec<u8>> {
    // Base image sections (bytes fallback + table fidelity).
    let base_img = match base {
        Some(b) => Some(ops::Image::load(b).map_err(|e| Error::Parse(format!("base image: {e}")))?),
        None => None,
    };
    // Header scalars: spec (or base) -> dir/header -> template -> --set.
    let mut hdr = if let Some(s) = spec {
        spec_vendor_header(&s.image)?
    } else if let Some(b) = &base_img {
        b.hdr.clone()
    } else {
        return Err(Error::Parse("vendor repack without spec needs --base".to_string()));
    };
    apply_header_file_vendor(&mut hdr, &cli.dir);
    if let Some(t) = template {
        let th = ops::Image::load(t).map_err(|e| Error::Parse(format!("template image: {e}")))?.hdr;
        hdr.cmdline = th.cmdline;
        hdr.name = th.name;
        hdr.page_size = th.page_size;
        hdr.kernel_addr = th.kernel_addr;
        hdr.ramdisk_addr = th.ramdisk_addr;
        hdr.tags_addr = th.tags_addr;
        hdr.dtb_addr = th.dtb_addr;
        emit_err("header: scalars taken from --template");
    }
    apply_sets_vendor(&mut hdr, &cli.sets)?;

    // Fragment list: spec entries, else base table.
    let frag_defs: Vec<(String, String, u32, u32, Option<String>)> = match spec {
        Some(s) => s
            .ramdisk
            .iter()
            .map(|r| (r.file.clone(), r.name.clone(), etype_of(&r.etype).unwrap_or(TYPE_PLATFORM), r.declared_size, r.board_id_hex.clone()))
            .collect(),
        None => base_img
            .as_ref()
            .unwrap()
            .table
            .iter()
            .map(|e| {
                let f = if e.name_str().is_empty() {
                    "vendor_ramdisk/ramdisk.cpio".to_string()
                } else {
                    format!("vendor_ramdisk/{}.cpio", e.name_str())
                };
                (f, e.name_str(), e.entry_type, e.size, Some(spec::hex_encode(&e.board_id)))
            })
            .collect(),
    };
    let mut frags: Vec<Vec<u8>> = Vec::new();
    let mut entries: Vec<RamdiskEntry> = Vec::new();
    let mut off = 0u32;
    for (file, name, etype, declared, board_hex) in &frag_defs {
        let spec_entry = spec.and_then(|s| s.ramdisk.iter().find(|r| &r.file == file));
        let recorded = spec_entry.map(|r| r.stored_format.as_str()).unwrap_or("raw");
        let on_disk_raw = spec_entry.is_some_and(|r| r.on_disk == "raw");
        // Base fallback slice for this fragment (by original offset/size).
        let base_slice = match &base_img {
            Some(b) => {
                let be = b.table.iter().find(|e| {
                    let f = if e.name_str().is_empty() {
                        "vendor_ramdisk/ramdisk.cpio".to_string()
                    } else {
                        format!("vendor_ramdisk/{}.cpio", e.name_str())
                    };
                    &f == file
                });
                be.and_then(|e| {
                    let s = e.offset as usize;
                    let en = s + e.size as usize;
                    if en <= b.ramdisk_blob.len() {
                        Some(b.ramdisk_blob[s..en].to_vec())
                    } else {
                        None
                    }
                })
            }
            None => None,
        };
        let short = file.strip_prefix("vendor_ramdisk/").unwrap_or(file);
        let target = target_format(&cli.formats, short, true, recorded);
        let (bytes, note) = resolve_component(&cli.dir, file, target, on_disk_raw, cli.no_compress, base_slice, *declared)?;
        emit_err(&note);
        let mut board_id = [0u8; 64];
        if let Some(hex) = board_hex {
            let raw = spec::hex_decode(hex)?;
            if raw.len() != 64 {
                return Err(Error::Parse(format!("{file}: board_id_hex must decode to 64 bytes")));
            }
            board_id.copy_from_slice(&raw);
        }
        let mut nn = [0u8; 32];
        nn[..name.as_bytes().len().min(32)].copy_from_slice(&name.as_bytes()[..name.as_bytes().len().min(32)]);
        entries.push(RamdiskEntry { size: bytes.len() as u32, offset: off, entry_type: *etype, name: nn, board_id });
        off += bytes.len() as u32;
        frags.push(bytes);
    }
    // dtb / bootconfig.
    let base_dtb = base_img.as_ref().map(|b| b.dtb.clone());
    let base_bc = base_img.as_ref().map(|b| b.bootconfig.clone());
    let dtb_declared = spec.map(|s| s.blob.iter().find(|b| b.name == "dtb").map(|b| b.declared_size).unwrap_or(0)).unwrap_or_else(|| base_img.as_ref().map(|b| b.hdr.dtb_size).unwrap_or(0));
    let bc_declared = spec.map(|s| s.blob.iter().find(|b| b.name == "bootconfig").map(|b| b.declared_size).unwrap_or(0)).unwrap_or_else(|| base_img.as_ref().map(|b| b.hdr.bootconfig_size).unwrap_or(0));
    let (dtb, note) = resolve_component(&cli.dir, "dtb", Format::Raw, true, true, base_dtb, dtb_declared)?;
    emit_err(&note);
    let (bc, note) = resolve_component(&cli.dir, "bootconfig", Format::Raw, true, true, base_bc, bc_declared)?;
    emit_err(&note);

    hdr.ramdisk_size = off;
    hdr.dtb_size = dtb.len() as u32;
    hdr.bootconfig_size = bc.len() as u32;
    hdr.table_entry_num = entries.len() as u32;
    hdr.table_entry_size = 108;
    hdr.table_size = entries.len() as u32 * 108;
    let out = ops::assemble(&hdr, &frags, &entries, &dtb, &bc);
    ops::verify_image(&out).map_err(|e| Error::Verify(format!("rebuilt image invalid: {e}")))?;
    // Footer: dir/footer.bin else base trailing bytes (unless dropped).
    let mut out = out;
    if !cli.drop_footer {
        if let Some(fb) = dir_file(&cli.dir, "footer.bin") {
            out.extend_from_slice(&fb);
            emit_err(&format!("footer: appended dir/footer.bin ({} bytes)", fb.len()));
        } else if let Some(b) = base {
            let base_end = base_img.as_ref().map(|im| {
                let page = im.hdr.page_size as usize;
                crate::vboot::image::align_up(
                    crate::vboot::image::align_up(
                        crate::vboot::image::align_up(
                            crate::vboot::image::align_up(im.hdr.header_size as usize, page)
                                + im.hdr.ramdisk_size as usize,
                            page,
                        ) + im.hdr.dtb_size as usize,
                        page,
                    ) + im.hdr.table_size as usize,
                    page,
                ) + im.hdr.bootconfig_size as usize
            }).unwrap_or(0);
            let base_end = crate::vboot::image::align_up(base_end, base_img.as_ref().map(|im| im.hdr.page_size as usize).unwrap_or(2048));
            if base_end < b.len() {
                out.extend_from_slice(&b[base_end..]);
                emit_err(&format!("footer: appended base trailing bytes ({} bytes)", b.len() - base_end));
            }
        }
    }
    Ok(out)
}

// ---------------- boot repack ----------------

fn repack_boot(
    cli: &Cli,
    spec: Option<&spec::Spec>,
    base: Option<&[u8]>,
    template: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let base_img = match base {
        Some(b) => Some(bootimg::parse(b).map_err(|e| Error::Parse(format!("base image: {e}")))?),
        None => None,
    };
    let mut hdr = if let Some(s) = spec {
        spec_boot_header(s)?
    } else if let Some(b) = &base_img {
        b.hdr.clone()
    } else {
        return Err(Error::Parse("boot repack without spec needs --base".to_string()));
    };
    apply_header_file_boot(&mut hdr, &cli.dir)?;
    if let Some(t) = template {
        let th = bootimg::parse(t).map_err(|e| Error::Parse(format!("template image: {e}")))?.hdr;
        hdr.cmdline = th.cmdline;
        hdr.extra_cmdline = th.extra_cmdline;
        hdr.name = th.name;
        hdr.page_size = th.page_size;
        hdr.kernel_addr = th.kernel_addr;
        hdr.ramdisk_addr = th.ramdisk_addr;
        hdr.second_addr = th.second_addr;
        hdr.tags_addr = th.tags_addr;
        hdr.os_version = th.os_version;
        hdr.dtb_addr = th.dtb_addr;
        emit_err("header: scalars taken from --template");
    }
    apply_sets_boot(&mut hdr, &cli.sets)?;

    let base_section = |name: &str| -> Option<Vec<u8>> {
        base_img.as_ref().and_then(|b| {
            b.sections.iter().find(|s| s.name == name).and_then(|s| {
                base.map(|bb| bb[s.start..s.start + s.len].to_vec())
            })
        })
    };
    let spec_blob = |name: &str| spec.and_then(|s| s.blob.iter().find(|b| b.name == name));
    let declared_of = |name: &str, fallback: u32| -> u32 {
        spec_blob(name).map(|b| b.declared_size).unwrap_or(fallback)
    };

    // kernel (+ optional kernel_dtb append).
    let k_target = fmt_target(cli, spec, base, "kernel", false);
    let (mut kernel, note) = resolve_component(
        &cli.dir,
        "kernel",
        k_target,
        spec_blob("kernel").is_some_and(|b| b.on_disk == "raw"),
        cli.no_compress,
        base_section("kernel"),
        declared_of("kernel", base_img.as_ref().map(|b| b.hdr.kernel_size).unwrap_or(0)),
    )?;
    emit_err(&note);
    if dir_file(&cli.dir, "kernel_dtb").is_some() {
        if dir_file(&cli.dir, "kernel").is_some() {
            let kd = dir_file(&cli.dir, "kernel_dtb").unwrap();
            emit_err(&format!("kernel_dtb: appended {} bytes to kernel", kd.len()));
            kernel.extend_from_slice(&kd);
        } else {
            emit_err("warning: kernel_dtb file ignored (kernel comes from --base; give an explicit kernel file to append)");
        }
    }
    // ramdisk (v4 boot forces lz4-legacy like magiskboot, unless told otherwise).
    let mut r_target = fmt_target(cli, spec, base, "ramdisk.cpio", true);
    let forced_lz4 = hdr.version == 4
        && !cli.no_compress
        && !cli.formats.iter().any(|(t, _)| t == "ramdisk.cpio" || t == "ramdisk" || t == "all")
        && r_target != Format::Lz4Legacy;
    if forced_lz4 {
        emit_err(&format!("RAMDISK_FMT: [{}] -> [lz4_legacy] (v4 GKI merge rule)", r_target.name()));
        r_target = Format::Lz4Legacy;
    }
    let (ramdisk, note) = {
        let r_spec = spec.and_then(|s| s.ramdisk.iter().find(|r| r.file == "ramdisk.cpio"));
        let on_raw = r_spec.is_some_and(|r| r.on_disk == "raw");
        let declared = r_spec.map(|r| r.declared_size).unwrap_or_else(|| base_img.as_ref().map(|b| b.hdr.ramdisk_size).unwrap_or(0));
        resolve_component(
            &cli.dir,
            "ramdisk.cpio",
            r_target,
            on_raw,
            cli.no_compress,
            base_section("ramdisk"),
            declared,
        )?
    };
    emit_err(&note);

    // second / extra / dtbo / dtb / signature.
    let mut parts: Vec<(&str, &str, bool)> = vec![
        ("second", "second", false),
        ("extra", "extra", false),
        ("recovery_dtbo", "recovery_dtbo", false),
        ("dtb", "dtb", false),
        ("signature", "signature", false),
    ];
    let mut blobs: Vec<(&str, Vec<u8>)> = Vec::new();
    for (name, file, is_ramdisk) in parts.drain(..) {
        let target = fmt_target(cli, spec, base, file, is_ramdisk);
        let on_raw = spec_blob(name).is_some_and(|b| b.on_disk == "raw");
        let declared = declared_of(name, 0);
        let (bytes, note) = resolve_component(&cli.dir, file, target, on_raw, cli.no_compress, base_section(name), declared)?;
        emit_err(&note);
        blobs.push((name, bytes));
    }
    let get = |n: &str| blobs.iter().find(|(x, _)| *x == n).map(|(_, b)| b.clone()).unwrap_or_default();

    hdr.kernel_size = kernel.len() as u32;
    hdr.ramdisk_size = ramdisk.len() as u32;
    hdr.second_size = get("second").len() as u32;
    // extra has no header field on clean images; Samsung quirk not rewritten.
    hdr.recovery_dtbo_size = get("recovery_dtbo").len() as u32;
    hdr.dtb_size = get("dtb").len() as u32;
    hdr.signature_size = get("signature").len() as u32;

    let page = hdr.page_size.max(1) as usize;
    let mut out = bootimg::serialize(&hdr)?;
    out.resize(bootimg::align_up(out.len(), page), 0);
    let push = |data: &[u8], out: &mut Vec<u8>| {
        out.extend_from_slice(data);
        out.resize(bootimg::align_up(out.len(), page), 0);
    };
    push(&kernel, &mut out);
    push(&ramdisk, &mut out);
    push(&get("second"), &mut out);
    let extra = get("extra");
    if !extra.is_empty() {
        push(&extra, &mut out);
    }
    let dtbo = get("recovery_dtbo");
    if !dtbo.is_empty() {
        hdr.recovery_dtbo_offset = out.len() as u64;
        push(&dtbo, &mut out);
    } else {
        hdr.recovery_dtbo_offset = 0;
    }
    let dtb = get("dtb");
    if !dtb.is_empty() {
        push(&dtb, &mut out);
    }
    let sig = get("signature");
    if !sig.is_empty() {
        out.extend_from_slice(&sig);
        out.resize(bootimg::align_up(out.len(), page), 0);
    }
    // Re-serialize header with final sizes/offsets (dtbo offset!).
    let head = bootimg::serialize(&hdr)?;
    let n = head.len().min(out.len());
    out[..n].copy_from_slice(&head[..n]);

    verify_boot_image(&out)?;
    // Footer like vendor.
    if !cli.drop_footer {
        if let Some(fb) = dir_file(&cli.dir, "footer.bin") {
            out.extend_from_slice(&fb);
            emit_err(&format!("footer: appended dir/footer.bin ({} bytes)", fb.len()));
        } else if let Some(bi) = &base_img {
            if !bi.footer.is_empty() {
                out.extend_from_slice(&bi.footer);
                emit_err(&format!("footer: appended base trailing bytes ({} bytes)", bi.footer.len()));
            }
        }
    }
    Ok(out)
}

fn fmt_target(cli: &Cli, spec: Option<&spec::Spec>, base: Option<&[u8]>, file: &str, is_ramdisk: bool) -> Format {
    if let Some((_, f)) = cli.formats.iter().find(|(t, _)| t == file) {
        return *f;
    }
    if is_ramdisk {
        if let Some((_, f)) = cli.formats.iter().find(|(t, _)| t == "ramdisk") {
            return *f;
        }
    }
    if let Some((_, f)) = cli.formats.iter().find(|(t, _)| t == "all") {
        return *f;
    }
    if let Some(s) = spec {
        let rec = s
            .ramdisk
            .iter()
            .find(|r| r.file == file || r.file.ends_with(&format!("/{file}")))
            .map(|r| r.stored_format.as_str())
            .or_else(|| s.blob.iter().find(|b| b.file == file).map(|b| b.stored_format.as_str()))
            .unwrap_or("raw");
        if let Ok(f) = Format::parse(rec) {
            if f != Format::Raw {
                return f;
            }
        }
    }
    if let Some(b) = base {
        // Sniff the base section by file name mapping.
        if let Ok(img) = bootimg::parse(b) {
            let secname = match file {
                "kernel" => "kernel",
                "ramdisk.cpio" => "ramdisk",
                "second" => "second",
                "extra" => "extra",
                "recovery_dtbo" => "recovery_dtbo",
                "dtb" => "dtb",
                "signature" => "signature",
                _ => "",
            };
            if let Some(s) = img.sections.iter().find(|x| x.name == secname) {
                if s.len > 0 {
                    return codec::sniff(&b[s.start..s.start + s.len]);
                }
            }
        } else if let Ok(im) = ops::Image::load(b) {
            // vendor base: only ramdisk frags have formats here
            for (i, e) in im.table.iter().enumerate() {
                let f = if e.name_str().is_empty() {
                    "ramdisk.cpio".to_string()
                } else {
                    format!("{}.cpio", e.name_str())
                };
                if file == f || file.ends_with(&f) {
                    if let Ok(sl) = im.frag_bytes(i) {
                        let sn = codec::sniff(sl);
                        if sn != Format::Raw {
                            return sn;
                        }
                    }
                }
            }
        }
    }
    Format::Raw
}

fn spec_boot_header(s: &spec::Spec) -> Result<bootimg::BootHeader> {
    let im = &s.image;
    let os_version = spec::os_encode(0, im.os_version.as_deref(), im.os_patch_level.as_deref())?;
    Ok(bootimg::BootHeader {
        version: im.header_version,
        page_size: im.page_size,
        kernel_size: 0,
        kernel_addr: im.kernel_addr.unwrap_or(0),
        ramdisk_size: 0,
        ramdisk_addr: im.ramdisk_addr.unwrap_or(0),
        second_size: 0,
        second_addr: im.second_addr.unwrap_or(0),
        tags_addr: im.tags_addr.unwrap_or(0),
        os_version,
        name: im.name.clone().map(|x| x.into_bytes()).unwrap_or_default(),
        cmdline: im.cmdline.clone().map(|x| x.into_bytes()).unwrap_or_default(),
        extra_cmdline: im.extra_cmdline.clone().map(|x| x.into_bytes()).unwrap_or_default(),
        id: Vec::new(),
        recovery_dtbo_size: 0,
        recovery_dtbo_offset: 0,
        dtb_size: 0,
        dtb_addr: im.dtb_addr.unwrap_or(0),
        header_size: im.header_size.unwrap_or(0),
        signature_size: 0,
        raw: Vec::new(),
    })
}

/// Re-parse our own output and sanity-check every section.
fn verify_boot_image(bytes: &[u8]) -> Result<()> {
    let img = bootimg::parse(bytes).map_err(|e| Error::Verify(format!("rebuilt boot image does not parse: {e}")))?;
    for s in &img.sections {
        let data = &bytes[s.start..s.start + s.len];
        match s.name {
            "ramdisk" if !data.is_empty() => {
                let fmt = codec::sniff(data);
                let dec = codec::decompress(fmt, data)
                    .map_err(|e| Error::Verify(format!("rebuilt ramdisk does not decompress ({fmt:?}): {e}")))?;
                cpio::parse(&dec).map_err(|e| Error::Verify(format!("rebuilt ramdisk cpio bad: {e}")))?;
            }
            "dtb" if !data.is_empty() => {
                dtb::verify(data, data.len())
                    .map_err(|e| Error::Verify(format!("rebuilt dtb invalid: {e}")))?;
            }
            _ => {}
        }
    }
    Ok(())
}
