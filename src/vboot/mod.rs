//! `bootsmasher vboot` subcommand: smart vendor_boot repack for Pixel 6.
//!
//! Grammar:
//!   vboot <vboot.img> [platform.cpio|platform.cpio.lz4] [out.img]
//!   vboot <vboot.img> [platform] -o|--out <out.img> [--pad-to N]
//!   vboot --verify <vboot.img>
//! Without an output path the finished image goes to stdout and nothing
//! else may touch stdout; diagnostics go to stderr.

use std::io::Write;

pub(crate) mod ops;
pub(crate) mod help;

use crate::common::error::{Error, Result};
use crate::common::space;
use crate::common::vendor::type_name;
use crate::common::lz4legacy::BlobKind;
use ops::{Mode, RepackOpts};


pub fn run(args: &[String], prog: &str) -> i32 {
    if args.iter().any(|a| a == "--help") {
        println!("{}", help::short(prog));
        return 0;
    }
    if args.iter().any(|a| a == "--expand") {
        println!("{}", help::expand(prog));
        return 0;
    }
    match run_inner(args) {
        Ok(()) => 0,
        Err(Error::Usage(m)) => {
            eprintln!("usage error: {m}\n{}", help::short(prog));
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
    recovery_path: Option<String>,
    out_path: Option<String>,
    verify_only: bool,
    /// Verify mode with a platform/recovery file or --drop: dry-run the pipeline.
    dry_run: bool,
    pad_to: Option<usize>,
    mode: Mode,
    drop: Vec<String>,
    sets: Vec<(String, String)>,
    drop_footer: bool,
    min_free: u64,
    check_dir: Option<String>,
}

fn parse_cli(args: &[String]) -> Result<Cli> {
    let mut vboot: Option<String> = None;
    let mut platform_path: Option<String> = None;
    let mut recovery_path: Option<String> = None;
    let mut out_positional: Option<String> = None;
    let mut out_flag: Option<String> = None;
    let mut verify_only = false;
    let mut pad_to: Option<usize> = None;
    let mut mode = Mode::Keep;
    let mut drop: Vec<String> = Vec::new();
    let mut sets: Vec<(String, String)> = Vec::new();
    let mut drop_footer = false;
    let mut min_free: u64 = 0;
    let mut check_dir: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
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
            "--drop" => {
                i += 1;
                let v = args.get(i).ok_or_else(|| Error::Usage("--drop needs a value".to_string()))?;
                let before = drop.len();
                for s in v.split(',') {
                    let s = s.trim();
                    if !s.is_empty() {
                        drop.push(s.to_string());
                    }
                }
                if drop.len() == before {
                    return Err(Error::Usage("--drop needs at least one selector".to_string()));
                }
            }
            "--recovery" => {
                i += 1;
                if recovery_path.is_some() {
                    return Err(Error::Usage("--recovery given twice".to_string()));
                }
                recovery_path = Some(
                    args.get(i).ok_or_else(|| Error::Usage("--recovery needs a file".to_string()))?.clone(),
                );
            }
            "--drop-footer" => drop_footer = true,
            "-s" | "--set" => {
                i += 1;
                let kv = args.get(i).ok_or_else(|| Error::Usage("-s/--set needs k=v".to_string()))?;
                let (k, v) = kv.split_once('=').ok_or_else(|| Error::Usage("-s/--set needs k=v".to_string()))?;
                sets.push((k.to_string(), v.to_string()));
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
    if recovery_path.is_some() && platform_path.is_some() {
        return Err(Error::Usage("--recovery conflicts with a platform file (pick one replacement)".to_string()));
    }
    if recovery_path.is_some() && mode != Mode::Keep {
        return Err(Error::Usage("--recovery conflicts with --split-first-stage/--merge".to_string()));
    }
    if verify_only {
        let v = vboot.ok_or_else(|| Error::Usage("--verify needs <vboot.img>".to_string()))?;
        if out_positional.is_some() || out_flag.is_some() {
            return Err(Error::Usage("--verify writes nothing, drop the output path".to_string()));
        }
        if platform_path.is_none()
            && recovery_path.is_none()
            && (mode != Mode::Keep || min_free != 0 || check_dir.is_some() || pad_to.is_some())
        {
            return Err(Error::Usage(
                "--split-first-stage/--merge/--min-free/--check-dir/--pad-to need a platform or recovery file in --verify mode (dry-run)".to_string(),
            ));
        }
        // Without a platform/recovery file AND without --drop this is a
        // pure verdict; otherwise it dry-runs the pipeline (writes nothing).
        let dry_run = platform_path.is_some() || recovery_path.is_some() || !drop.is_empty();
        return Ok(Cli {
            vboot: v,
            platform_path,
            recovery_path,
            out_path: None,
            verify_only: true,
            dry_run,
            pad_to,
            mode,
            drop,
            sets,
            drop_footer,
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
        recovery_path,
        out_path: out_flag.or(out_positional),
        verify_only: false,
        dry_run: false,
        pad_to,
        mode,
        drop,
        sets,
        drop_footer,
        min_free,
        check_dir,
    })
}

fn kind_str(k: BlobKind) -> &'static str {
    k.name()
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
        if !cli.dry_run {
            print_verdict(&cli.vboot, &img, false)?;
            return Ok(());
        }
        // Dry-run: full pipeline in memory, report, write nothing.
        let plat_data: Option<(String, Vec<u8>)> = match &cli.platform_path {
            Some(p) => {
                let data =
                    std::fs::read(p).map_err(|e| Error::Io(format!("cannot read {p}: {e}")))?;
                Some((p.clone(), data))
            }
            None => None,
        };
        let plat_arg: Option<(&str, Vec<u8>)> =
            plat_data.as_ref().map(|(s, d)| (s.as_str(), d.clone()));
        let rec_data: Option<(String, Vec<u8>)> = match &cli.recovery_path {
            Some(p) => {
                let data =
                    std::fs::read(p).map_err(|e| Error::Io(format!("cannot read {p}: {e}")))?;
                Some((p.clone(), data))
            }
            None => None,
        };
        let mut out = ops::repack_with_opts(
            &img,
            plat_arg,
            RepackOpts {
                mode: cli.mode,
                drop: cli.drop.clone(),
                sets: cli.sets.clone(),
                recovery: rec_data,
                drop_footer: cli.drop_footer,
            },
        )?;
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
    let rec_ref: Option<(String, Vec<u8>)> = match &cli.recovery_path {
        Some(p) => {
            let data = std::fs::read(p).map_err(|e| Error::Io(format!("cannot read {p}: {e}")))?;
            Some((p.clone(), data))
        }
        None => None,
    };
    let mut out = ops::repack_with_opts(
        &img,
        plat_ref,
        RepackOpts {
            mode: cli.mode,
            drop: cli.drop.clone(),
            sets: cli.sets.clone(),
            recovery: rec_ref,
            drop_footer: cli.drop_footer,
        },
    )?;

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
