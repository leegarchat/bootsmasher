//! `bootsmasher vboot` subcommand: smart vendor_boot repack for Pixel 6.
//!
//! Grammar:
//!   vboot <vboot.img> [platform.cpio|platform.cpio.lz4] [out.img]
//!   vboot <vboot.img> [platform] -o|--out <out.img> [--pad-to N]
//!   vboot --verify <vboot.img>
//! Without an output path the finished image goes to stdout and nothing
//! else may touch stdout; diagnostics go to stderr.

use std::io::Write;

pub(crate) mod image;
pub(crate) mod lz4legacy;
pub(crate) mod cpio;
pub(crate) mod dtb;
pub(crate) mod ops;
pub(crate) mod space;

use crate::error::{Error, Result};
use image::type_name;
use lz4legacy::BlobKind;
use ops::Mode;

const HELP: &str = "bootsmasher vboot — smart vendor_boot repair flow (Pixel 6 / gs101)
Alias: vb.

Usage:
  bootsmasher vboot <vboot.img> [platform.cpio|platform.cpio.lz4] [out.img]
  bootsmasher vboot <vboot.img> [platform] -o <out.img> [--pad-to <bytes>]
  bootsmasher vboot --verify <vboot.img> [platform] [--check-dir <dir>]
  bootsmasher vboot --help

What it does:
  Potрошит vendor_boot умным анализатором, а не наугад: каждый фрагмент
  таблицы декомпрессируется и его cpio проверяется, сумма таблицы
  сверяется с заголовком, FDT проходятся. Диагностирует протухшую
  таблицу мейнтейнера (один поток + две записи) и чинит раскладку;
  валидные образы проходят байт-в-байт, без пережатия.

Layout modes (mutually exclusive):
  (none)               Keep the layout: valid images round-trip verbatim
                       (fragment bytes are copied, never recompressed);
                       a stale single-stream table becomes one platform
                       entry. With a platform file the platform is
                       replaced; a valid original dlkm is kept as
                       fallback, otherwise lib/** is pulled out of the
                       new platform into a fresh dlkm fragment. Any other
                       valid original fragments (shiba's \"16K\", recovery)
                       are carried over verbatim, rechained — never
                       silently dropped.
  --split-first-stage (--split)
                       Partition content into fragments by subtree:
                       first_stage_ramdisk/** + rest -> platform,
                       recovery/** + debug_ramdisk/** -> recovery
                       (name=\"recovery\", type=2),
                       lib/** -> dlkm. A dlkm/recovery fragment is
                       emitted only when it carries real files (a bare
                       lib or debug_ramdisk dir alone is not worth a
                       fragment); when the new content yields no dlkm
                       but the original dlkm is valid, it is kept
                       byte-identically.
  --merge              Glue everything into a single platform fragment
                       (platform slots replaced by the new file when
                       given, original dlkm/recovery content joins it;
                       mid-stream TRAILERs are dropped, exactly one is
                       written). Fragments are re-encoded marker-free
                       LZ4-legacy, like kernel ramdisks (the lz4 CLI
                       treats a zero word as corruption, so no end
                       marker is written).

Platform file formats:
  platform.cpio.lz4 stays verbatim after validation; a raw
  platform.cpio is compressed to LZ4-legacy. Anything else
  (erofs blob, garbage slice) is a usage error, never guessed.

Behavior:
  Header flags, cmdline, dtb and bootconfig are always preserved.
  The rebuilt image is fully re-verified in memory before anything is
  emitted; on any failure nothing is written and the error goes to
  stderr (never to stdout — the pipe stays clean).

Output:
  With an output path (-o/--out or positional) the image is written to
  the file. Without one the image bytes go to stdout with no other
  stdout output. --pad-to appends zero bytes up to the given size
  (e.g. 67108864 for the block-device size). Before writing, free space
  is checked: free(dir) must cover the image plus --min-free (default
  0; plain bytes or human sizes like 512M, 1GiB, 1.5G — K/M/G/T are
  binary). The checked dir is the output file's parent (or --check-dir
  override); for stdout output the check runs only with --check-dir.

Verify:
  Without a platform file: print the fragment table verdict (exit 0
  only when fully self-consistent), e.g.
    OK image: header v4, page 2048, ramdisk 28987491, 2 fragment(s)...
    INVALID image: ... [STALE TABLE, single stream]
      frag 0 platform: lz4 block 6 truncated (need 3970208, have 1175831)
  With a platform file: dry-run the whole pipeline in memory — print
  the resulting layout, the resulting size and the space verdict — and
  write nothing (checked dir defaults to '.').

Typical sessions:
  bootsmasher vboot --verify vendor_boot.img
  bootsmasher vb broken.img -o fixed.img
  bootsmasher vboot stock.img OrangeFox.ramdisk.lz4 -o fox_boot.img
  bootsmasher vboot broken.img full.cpio --split -o frag.img
  bootsmasher vboot stock.img --merge -o single.img
  bootsmasher vboot broken.img fox.lz4 --pad-to 67108864 --min-free 1G -o fox_64m.img
  bootsmasher vboot broken.img > fixed.img   # stdout = pure image bytes

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
    vboot: String,
    platform_path: Option<String>,
    out_path: Option<String>,
    verify_only: bool,
    pad_to: Option<usize>,
    mode: Mode,
    min_free: u64,
    check_dir: Option<String>,
}

fn parse_cli(args: &[String]) -> Result<Cli> {
    let mut vboot: Option<String> = None;
    let mut platform_path: Option<String> = None;
    let mut out_positional: Option<String> = None;
    let mut out_flag: Option<String> = None;
    let mut verify_only = false;
    let mut pad_to: Option<usize> = None;
    let mut mode = Mode::Keep;
    let mut min_free: u64 = 0;
    let mut check_dir: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => return Err(Error::Usage("help requested".to_string())),
            "--verify" => verify_only = true,
            "--split-first-stage" | "--split" => {
                if mode == Mode::Merge {
                    return Err(Error::Usage("--split-first-stage conflicts with --merge".to_string()));
                }
                mode = Mode::Split;
            }
            "--merge" => {
                if mode == Mode::Split {
                    return Err(Error::Usage("--merge conflicts with --split-first-stage".to_string()));
                }
                mode = Mode::Merge;
            }
            "-o" | "--out" => {
                i += 1;
                out_flag = Some(args.get(i).ok_or_else(|| Error::Usage("--out needs a value".to_string()))?.clone());
            }
            "--pad-to" => {
                i += 1;
                let v = args.get(i).ok_or_else(|| Error::Usage("--pad-to needs a value".to_string()))?;
                pad_to = Some(v.parse::<usize>().map_err(|_| Error::Usage("--pad-to needs an integer".to_string()))?);
            }
            "--min-free" => {
                i += 1;
                let v = args.get(i).ok_or_else(|| Error::Usage("--min-free needs a value".to_string()))?;
                min_free = space::parse_size(v)?;
            }
            "--check-dir" => {
                i += 1;
                check_dir = Some(args.get(i).ok_or_else(|| Error::Usage("--check-dir needs a value".to_string()))?.clone());
            }
            s if s.starts_with('-') => return Err(Error::Usage(format!("unknown flag {s}"))),
            p => {
                if vboot.is_none() {
                    vboot = Some(p.to_string());
                } else if platform_path.is_none() {
                    platform_path = Some(p.to_string());
                } else if out_positional.is_none() {
                    out_positional = Some(p.to_string());
                } else {
                    return Err(Error::Usage("too many positional arguments".to_string()));
                }
            }
        }
        i += 1;
    }
    if verify_only {
        let v = vboot.ok_or_else(|| Error::Usage("--verify needs <vboot.img>".to_string()))?;
        if out_positional.is_some() || out_flag.is_some() {
            return Err(Error::Usage("--verify writes nothing, drop the output path".to_string()));
        }
        if platform_path.is_none()
            && (mode != Mode::Keep || min_free != 0 || check_dir.is_some() || pad_to.is_some())
        {
            return Err(Error::Usage(
                "--split-first-stage/--merge/--min-free/--check-dir/--pad-to need a platform file in --verify mode (dry-run)".to_string(),
            ));
        }
        return Ok(Cli {
            vboot: v,
            platform_path,
            out_path: None,
            verify_only: true,
            pad_to,
            mode,
            min_free,
            check_dir,
        });
    }
    let v = vboot.ok_or_else(|| Error::Usage("need <vboot.img>".to_string()))?;
    if out_flag.is_some() && out_positional.is_some() {
        return Err(Error::Usage("out.img positionally and via --out at once".to_string()));
    }
    Ok(Cli {
        vboot: v,
        platform_path,
        out_path: out_flag.or(out_positional),
        verify_only: false,
        pad_to,
        mode,
        min_free,
        check_dir,
    })
}

fn kind_str(k: BlobKind) -> &'static str {
    match k {
        BlobKind::Lz4Legacy => "lz4_legacy",
        BlobKind::Cpio => "cpio",
        BlobKind::Unknown => "unknown",
    }
}

/// Report printing that tolerates a closed pipe (`--verify ... | head`):
/// a dead reader must not abort the process, the verdict stands.
fn emit(line: &str) {
    use std::io::Write as _;
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{line}");
    let _ = o.flush();
}

fn run_inner(args: &[String]) -> Result<()> {
    let cli = parse_cli(args)?;
    let img = std::fs::read(&cli.vboot).map_err(|e| Error::Io(format!("cannot read {}: {e}", cli.vboot)))?;

    if cli.verify_only {
        if cli.platform_path.is_none() {
            print_verdict(&cli.vboot, &img, false)?;
            return Ok(());
        }
        // Dry-run: full pipeline in memory, report, write nothing.
        let p = cli.platform_path.as_ref().unwrap();
        let data = std::fs::read(p).map_err(|e| Error::Io(format!("cannot read {p}: {e}")))?;
        let mut out = ops::repack(&img, Some((p.as_str(), data)), cli.mode)?;
        if let Some(pad) = cli.pad_to {
            if out.len() > pad {
                return Err(Error::Usage(format!("image {} bytes exceeds --pad-to {pad}", out.len())));
            }
            out.resize(pad, 0);
        }
        let dir = cli.check_dir.clone().unwrap_or_else(|| ".".to_string());
        emit(&format!("dry-run: resulting image {} bytes", out.len()));
        print_verdict("result", &out, true)?;
        space::ensure_space(std::path::Path::new(&dir), out.len() as u64, cli.min_free)?;
        emit(&format!(
            "space: fits in {dir} (reserve {} bytes)",
            cli.min_free
        ));
        return Ok(());
    }

    let plat: Option<(&str, Vec<u8>)> = match &cli.platform_path {
        Some(p) => {
            let data = std::fs::read(p).map_err(|e| Error::Io(format!("cannot read {p}: {e}")))?;
            Some((p.as_str(), data))
        }
        None => None,
    };
    // Re-borrow with a lifetime tied to cli, not to the temporary above.
    let plat_ref: Option<(&str, Vec<u8>)> = plat.as_ref().map(|(s, d)| (s as &str, d.clone()));
    let mut out = ops::repack(&img, plat_ref, cli.mode)?;

    if let Some(pad) = cli.pad_to {
        if out.len() > pad {
            return Err(Error::Usage(format!("image {} bytes exceeds --pad-to {pad}", out.len())));
        }
        out.resize(pad, 0);
    }

    match cli.out_path {
        Some(p) => {
            let dir = match &cli.check_dir {
                Some(d) => d.clone(),
                None => match std::path::Path::new(&p).parent() {
                    Some(par) if !par.as_os_str().is_empty() => par.to_string_lossy().into_owned(),
                    _ => ".".to_string(),
                },
            };
            space::ensure_space(std::path::Path::new(&dir), out.len() as u64, cli.min_free)?;
            std::fs::write(&p, &out).map_err(|e| Error::Io(format!("cannot write {p}: {e}")))?;
            eprintln!("wrote {} ({} bytes)", p, out.len());
        }
        None => {
            if let Some(dir) = &cli.check_dir {
                space::ensure_space(std::path::Path::new(dir), out.len() as u64, cli.min_free)?;
            }
            std::io::stdout().write_all(&out)?;
        }
    }
    Ok(())
}

/// Print the OK/INVALID verdict + fragment table.
/// In dry-run mode the label is synthetic and the header line is shorter.
fn print_verdict(label: &str, img: &[u8], dry: bool) -> Result<()> {
    let a = ops::analyze(img).map_err(|e| Error::Verify(format!("INVALID: {e}")))?;
    let broken = a.stale_table
        || a.table_sum != a.header_ramdisk_size as u64
        || !a.offsets_chain_ok
        || a.frags.iter().any(|f| !f.valid);
    if dry {
        emit(&format!(
            "{}: header v{}, {} fragment(s), ramdisk {} (table sum {}), dtb {} FDT(s), bootconfig {} bytes{}",
            if broken { "INVALID" } else { "OK" },
            a.header_version,
            a.frags.len(),
            a.header_ramdisk_size,
            a.table_sum,
            a.dtb_fdts,
            a.bootconfig_len,
            if a.stale_table { " [STALE TABLE, single stream]" } else { "" },
        ));
    } else {
        emit(&format!(
            "{} {}: header v{}, page {}, ramdisk {} (table sum {}), {} fragment(s), dtb {} FDT(s), bootconfig {} bytes{}",
            if broken { "INVALID" } else { "OK" },
            label,
            a.header_version,
            a.page_size,
            a.header_ramdisk_size,
            a.table_sum,
            a.frags.len(),
            a.dtb_fdts,
            a.bootconfig_len,
            if a.stale_table { " [STALE TABLE, single stream]" } else { "" },
        ));
    }
    for f in &a.frags {
        emit(&format!(
            "  frag {} name={:?} type={} size={} offset={} fmt={} valid={} {}",
            f.index,
            f.name,
            type_name(f.etype),
            f.size,
            f.offset,
            kind_str(f.kind),
            if f.valid { "yes" } else { "NO" },
            f.detail,
        ));
    }
    if broken {
        return Err(Error::Verify("image is not self-consistent".to_string()));
    }
    Ok(())
}
