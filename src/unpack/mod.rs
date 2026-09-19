//! `bootsmasher unpack` — magiskboot-compatible extraction, but universal
//! and honest: works on boot.img (v0..v4, kernels, dtb/dtbo) and
//! vendor_boot, auto-decompresses, never dies on broken sections.
//! Every section gets a verdict + WHY; broken bytes are still dumped
//! (truncated where the file ends) so nothing is silently lost.

use std::path::{Path, PathBuf};

use crate::common::bootimg;
use crate::common::codec::{self, Format};
use crate::common::extract;
use crate::common::error::{Error, Result};
use crate::common::spec;
use crate::common::cpio;
use crate::common::dtb;
use crate::common::vendor::type_name;
use crate::vboot::ops;

pub(crate) mod help;


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
        Ok(degraded) => {
            println!("RESULT: {}", if degraded { "DEGRADED (see INVALID lines above)" } else { "OK" });
            0
        }
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
    image: String,
    header: bool,
    raw: bool,
    out_dir: PathBuf,
    extract: bool,
    spec: bool,
}

fn parse_cli(args: &[String]) -> Result<Cli> {
    let mut image: Option<String> = None;
    let mut header = false;
    let mut raw = false;
    let mut out_dir = PathBuf::from(".");
    let mut extract = false;
    let mut spec = true;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // NOTE: -h dumps the header FILE (magiskboot parity).
            "-h" => header = true,
            "-n" => raw = true,
            "-x" | "--extract" => extract = true,
            "--no-spec" => spec = false,
            "-o" | "--out-dir" => {
                i += 1;
                out_dir = PathBuf::from(
                    args.get(i).ok_or_else(|| Error::Usage("-o/--out-dir needs a value".to_string()))?,
                );
            }
            s if s.starts_with('-') => return Err(Error::Usage(format!("unknown flag {s}"))),
            p => {
                if image.is_some() {
                    return Err(Error::Usage("only one <image> argument".to_string()));
                }
                image = Some(p.to_string());
            }
        }
        i += 1;
    }
    let image = image.ok_or_else(|| Error::Usage("need <image>".to_string()))?;
    Ok(Cli { image, header, raw, out_dir, extract, spec })
}

fn say(line: &str) {
    println!("{line}");
}

fn write_file(dir: &Path, name: &str, data: &[u8]) -> Result<PathBuf> {
    let p = dir.join(name);
    if let Some(par) = p.parent() {
        std::fs::create_dir_all(par)?;
    }
    std::fs::write(&p, data)?;
    Ok(p)
}

fn run_inner(args: &[String]) -> Result<bool> {
    let cli = parse_cli(args)?;
    std::fs::create_dir_all(&cli.out_dir)?;
    let bytes = std::fs::read(&cli.image)
        .map_err(|e| Error::Io(format!("cannot read {}: {e}", cli.image)))?;
    if bytes.len() >= 8 && bytes[0..8] == *b"VNDRBOOT" {
        unpack_vendor(&cli, &bytes)
    } else if bytes.len() >= 8 && bytes[0..8] == *b"ANDROID!" {
        unpack_boot(&cli, &bytes)
    } else {
        Err(Error::Parse(format!(
            "unknown image magic {:02x?} (want VNDRBOOT or ANDROID!)",
            &bytes[..bytes.len().min(8)]
        )))
    }
}

// ---------------- vendor_boot ----------------

fn unpack_vendor(cli: &Cli, bytes: &[u8]) -> Result<bool> {
    let dir = &cli.out_dir;
    let d = ops::diagnose(bytes);
    let mut degraded = !d.overall_ok;
    let hdr = match &d.header {
        Some(h) => h.clone(),
        None => return Err(Error::Parse(format!("vendor_boot header: {}", d.header_note))),
    };
    say(&format!(
        "vendor_boot v{} page={} ramdisk_size={} ({:#x})",
        hdr.header_version, hdr.page_size, hdr.ramdisk_size, hdr.ramdisk_size
    ));
    say(&format!(
        "header: name={:?} cmdline={:?} [{}]",
        hdr.name_str(),
        hdr.cmdline_str(),
        d.header_note
    ));
    say(&format!(
        "table: {}/{} entries read [{}]",
        d.table_entries_read, d.table_declared, d.table_why
    ));
    if d.table_why != "ok" {
        degraded = true;
    }

    let mut spec_ramdisks = Vec::new();
    let vdir = dir.join("vendor_ramdisk");
    for f in &d.frags {
        if f.index == usize::MAX {
            say(&format!("blob: INVALID {}", f.why));
            degraded = true;
            continue;
        }
        let fname = if f.name.is_empty() { "ramdisk.cpio".to_string() } else { format!("{}.cpio", f.name) };
        say(&format!(
            "frag {} {:?} type={} declared={} off={} avail={} fmt={} -> {}",
            f.index,
            f.name,
            type_name(f.etype),
            f.declared_size,
            f.declared_offset,
            f.available,
            f.stored_format,
            if f.valid { format!("ok ({} files)", f.entries) } else { format!("INVALID: {}", f.why) },
        ));
        if !f.valid {
            degraded = true;
        }
        // Slice bytes available for this fragment.
        let slice = frag_slice(bytes, &hdr, f.declared_offset as usize, f.available);
        // On-disk bytes: decompressed when possible (default), else raw.
        let (disk_bytes, on_disk) = if !cli.raw {
            match try_decompress_cpio(slice, &f.stored_format) {
                Some((dec, _)) => (dec, "decompressed"),
                None => (slice.to_vec(), "raw"),
            }
        } else {
            (slice.to_vec(), "raw")
        };
        if !disk_bytes.is_empty() || f.declared_size > 0 {
            write_file(&vdir, &fname, &disk_bytes)?;
        }
        if cli.extract && on_disk == "decompressed" {
            extract_report(&vdir, &fname, &disk_bytes)?;
        } else if cli.extract {
            // Raw on disk: still expand from memory when decodable.
            if let Some((dec, _)) = try_decompress_cpio(slice, &f.stored_format) {
                extract_report(&vdir, &fname, &dec)?;
            } else {
                say(&format!("  extract: skipped {fname} ({})", f.why));
            }
        }
        spec_ramdisks.push(spec::RamdiskSpec {
            file: format!("vendor_ramdisk/{fname}"),
            name: f.name.clone(),
            etype: type_name(f.etype).to_string(),
            // "unknown" sniff verdict is not a format; on disk these are
            // verbatim bytes.
            stored_format: match f.stored_format.as_str() {
                "" | "unknown" => "raw".to_string(),
                s => s.to_string(),
            },
            on_disk: on_disk.to_string(),
            declared_size: f.declared_size,
            board_id_hex: frag_board_id(bytes, &hdr, f.index),
        });
    }

    // Whole-blob rescue for the stale-table case (file on disk + report
    // line; repack works from the per-fragment files, so no spec record).
    if d.whole_blob_single_stream && !d.overall_ok {
        let blob = whole_blob(bytes, &hdr);
        if let Some((dec, n)) = try_decompress_cpio(blob, "lz4-legacy") {
            let rfile = if cli.raw {
                write_file(&vdir, "ramdisk.full-rescue.bin", blob)?;
                "vendor_ramdisk/ramdisk.full-rescue.bin"
            } else {
                write_file(&vdir, "ramdisk.full-rescue.cpio", &dec)?;
                "vendor_ramdisk/ramdisk.full-rescue.cpio"
            };
            say(&format!("rescue: whole blob is one valid stream -> {rfile} ({n} files)"));
        }
    }

    // DTB / bootconfig (available bytes, even when truncated).
    let page = hdr.page_size.max(1) as usize;
    let rs = crate::common::vendor::align_up(hdr.header_size as usize, page);
    let ram_end = rs + hdr.ramdisk_size as usize;
    let dtb_start = crate::common::vendor::align_up(ram_end, page);
    let dtb_avail = dtb_start
        .checked_add(d.dtb_available)
        .map(|e| &bytes[dtb_start.min(bytes.len())..e.min(bytes.len())])
        .unwrap_or(&[]);
    say(&format!(
        "dtb: declared={} avail={} [{}]{}",
        d.dtb_declared,
        d.dtb_available,
        d.dtb_why,
        if d.dtb_fdts > 0 { format!(" ({} FDTs)", d.dtb_fdts) } else { String::new() }
    ));
    if d.dtb_why != "ok" && !d.dtb_why.starts_with("absent") {
        degraded = true;
    }
    if !dtb_avail.is_empty() {
        write_file(dir, "dtb", dtb_avail)?;
    }
    let dtb_end = dtb_start + hdr.dtb_size as usize;
    let table_end = crate::common::vendor::align_up(dtb_end, page) + hdr.table_size as usize;
    let bc_off = crate::common::vendor::align_up(table_end, page);
    let bc_avail = bc_off
        .checked_add(d.bootconfig_available)
        .map(|e| &bytes[bc_off.min(bytes.len())..e.min(bytes.len())])
        .unwrap_or(&[]);
    say(&format!(
        "bootconfig: declared={} avail={} [{}]",
        d.bootconfig_declared,
        d.bootconfig_available,
        if d.bootconfig_available == d.bootconfig_declared as usize {
            "ok"
        } else {
            degraded = true;
            "TRUNCATED"
        }
    ));
    if !bc_avail.is_empty() {
        write_file(dir, "bootconfig", bc_avail)?;
    }

    // Footer: bytes after the computed image end (file + report only).
    let img_end = crate::common::vendor::align_up(bc_off + hdr.bootconfig_size as usize, page);
    write_footer(dir, bytes, img_end)?;

    if cli.header {
        let mut h = String::new();
        h.push_str(&format!("name={}\n", hdr.name_str()));
        h.push_str(&format!("cmdline={}\n", hdr.cmdline_str()));
        write_file(dir, "header", h.as_bytes())?;
    }
    if cli.spec {
        write_vendor_spec(&hdr, &d, spec_ramdisks, dir)?;
    }
    Ok(degraded)
}

fn frag_slice<'a>(bytes: &'a [u8], hdr: &crate::common::vendor::Header, off: usize, len: usize) -> &'a [u8] {
    let page = hdr.page_size.max(1) as usize;
    let rs = crate::common::vendor::align_up(hdr.header_size as usize, page);
    let s = rs + off;
    if s >= bytes.len() {
        return &[];
    }
    &bytes[s..(s + len).min(bytes.len())]
}

/// board_id hex of table entry `index` (None when all zero or unreadable).
fn frag_board_id(
    bytes: &[u8],
    hdr: &crate::common::vendor::Header,
    index: usize,
) -> Option<String> {
    let page = hdr.page_size.max(1) as usize;
    let rs = crate::common::vendor::align_up(hdr.header_size as usize, page);
    let ram_end = rs + hdr.ramdisk_size as usize;
    let dtb_start = crate::common::vendor::align_up(ram_end, page);
    let table_off = crate::common::vendor::align_up(dtb_start + hdr.dtb_size as usize, page);
    let e_off = table_off + index * 108;
    let entry = bytes.get(e_off..e_off + 108)?;
    let board = &entry[44..108];
    if board.iter().all(|&b| b == 0) {
        return None;
    }
    Some(spec::hex_encode(board))
}

fn whole_blob<'a>(bytes: &'a [u8], hdr: &crate::common::vendor::Header) -> &'a [u8] {    let page = hdr.page_size.max(1) as usize;
    let rs = crate::common::vendor::align_up(hdr.header_size as usize, page);
    let e = (rs + hdr.ramdisk_size as usize).min(bytes.len());
    &bytes[rs.min(bytes.len())..e]
}

/// Decompress a fragment slice to cpio (when its format allows),
/// returning (cpio_bytes, non_trailer_file_count).
fn try_decompress_cpio(slice: &[u8], stored_format: &str) -> Option<(Vec<u8>, usize)> {
    let fmt = match stored_format {
        "lz4_legacy" | "lz4-legacy" => Format::Lz4Legacy,
        "lz4" => Format::Lz4Frame,
        "gzip" => Format::Gzip,
        "xz" => Format::Xz,
        "lzma" => Format::Lzma,
        "cpio" | "raw" | "" => {
            return cpio::parse(slice)
                .ok()
                .map(|en| (slice.to_vec(), en.iter().filter(|e| cpio::name_str(e) != "TRAILER!!!").count()));
        }
        _ => return None,
    };
    let dec = codec::decompress(fmt, slice).ok()?;
    let en = cpio::parse(&dec).ok()?;
    let n = en.iter().filter(|e| cpio::name_str(e) != "TRAILER!!!").count();
    // A decompressed blob without TRAILER is not a ramdisk cpio.
    if en.iter().all(|e| cpio::name_str(e) != "TRAILER!!!") {
        return None;
    }
    Some((dec, n))
}

fn extract_report(vdir: &Path, fname: &str, cpio_bytes: &[u8]) -> Result<()> {
    let stem = fname.strip_suffix(".cpio").unwrap_or(fname);
    let dest = vdir.join(format!("{stem}.d"));
    match extract::extract(cpio_bytes, &dest) {
        Ok(r) => {
            say(&format!(
                "  extract: {} -> {} files, {} dirs, {} symlinks{}",
                dest.display(),
                r.files,
                r.dirs,
                r.symlinks,
                if r.skipped.is_empty() {
                    String::new()
                } else {
                    format!(" ({} skipped, e.g. {})", r.skipped.len(), r.skipped[0])
                }
            ));
            Ok(())
        }
        Err(e) => {
            say(&format!("  extract: {fname} failed: {e}"));
            Ok(())
        }
    }
}

fn write_footer(dir: &Path, bytes: &[u8], img_end: usize) -> Result<()> {
    let start = img_end.min(bytes.len());
    let tail = &bytes[start..];
    let nonzero = tail.iter().filter(|&&b| b != 0).count();
    if nonzero == 0 {
        say(&format!(
            "footer: none ({} zero bytes of partition padding to {:#x})",
            tail.len(),
            bytes.len()
        ));
        return Ok(());
    }
    // Stored verbatim (magiskboot parity): padded partition dumps carry
    // their padding here; use --drop-footer + --pad-to on repack for lean.
    write_file(dir, "footer.bin", tail)?;
    let mut note = format!(
        "footer: {} trailing bytes ({} nonzero) at {start:#x} -> footer.bin",
        tail.len(),
        nonzero
    );
    if tail.len() >= 4 {
        let magic = &tail[..4];
        if magic == b"AVB0" {
            note.push_str(" (starts with vbmeta image)");
        }
        if tail.len() >= 64 && tail[tail.len() - 64..tail.len() - 60] == *b"AVBf" {
            note.push_str(" (AVB footer at file end)");
        } else if magic == b"-SIG" || tail.starts_with(b"SEANDROIDENFORCE") {
            note.push_str(" (signature/enforce magic)");
        }
    }
    say(&note);
    Ok(())
}

fn write_vendor_spec(
    hdr: &crate::common::vendor::Header,
    d: &ops::Diagnosis,
    ramdisks: Vec<spec::RamdiskSpec>,
    dir: &Path,
) -> Result<()> {
    spec::write_spec(
        dir,
        &spec::Spec {
            image: spec::ImageSpec {
                kind: "vendor_boot".to_string(),
                header_version: hdr.header_version,
                page_size: hdr.page_size,
                kernel_addr: Some(hdr.kernel_addr),
                ramdisk_addr: Some(hdr.ramdisk_addr),
                second_addr: None,
                tags_addr: Some(hdr.tags_addr),
                os_version: None,
                os_patch_level: None,
                name: Some(hdr.name_str()),
                cmdline: Some(hdr.cmdline_str()),
                extra_cmdline: None,
                dtb_addr: Some(hdr.dtb_addr),
                header_size: Some(hdr.header_size),
                bootconfig_size: Some(hdr.bootconfig_size),
            },
            ramdisk: ramdisks,
            blob: vec![
                spec::BlobSpec {
                    name: "dtb".to_string(),
                    file: "dtb".to_string(),
                    stored_format: "raw".to_string(),
                    on_disk: "raw".to_string(),
                    declared_size: d.dtb_declared,
                },
                spec::BlobSpec {
                    name: "bootconfig".to_string(),
                    file: "bootconfig".to_string(),
                    stored_format: "raw".to_string(),
                    on_disk: "raw".to_string(),
                    declared_size: d.bootconfig_declared,
                },
            ],
        },
    )
}

// ---------------- boot ----------------

fn unpack_boot(cli: &Cli, bytes: &[u8]) -> Result<bool> {
    let dir = &cli.out_dir;
    let img = bootimg::parse(bytes)?;
    let h = &img.hdr;
    say(&format!(
        "boot v{} page={} kernel_size={} ramdisk_size={}",
        h.version, h.page_size, h.kernel_size, h.ramdisk_size
    ));
    let mut degraded = false;
    let mut blobs: Vec<spec::BlobSpec> = Vec::new();
    let mut ramdisks: Vec<spec::RamdiskSpec> = Vec::new();

    for s in &img.sections {
        // extra is nonstandard (no header field on clean images): whatever
        // bytes the sequential layout yields ARE its declared size.
        let declared = if s.name == "extra" { s.len } else { declared_len(&img, s.name) };
        let data = &bytes[s.start..s.start + s.len];
        let status = if s.len < declared {
            degraded = true;
            format!("TRUNCATED: header says {declared}, file has {} (cut download?)", s.len)
        } else {
            "ok".to_string()
        };
        match s.name {
            "kernel" => {
                let (kern, kern_dtb) = bootimg::split_kernel_dtb(data);
                let fmt = codec::sniff(&kern);
                say(&format!(
                    "kernel: {}/{} bytes fmt={} [{}]{}",
                    kern.len(),
                    declared,
                    fmt.name(),
                    status,
                    if kern_dtb.is_empty() {
                        String::new()
                    } else {
                        format!(" (+kernel_dtb {} bytes appended)", kern_dtb.len())
                    }
                ));
                let (disk, on_disk) = maybe_decompress(cli.raw, fmt, &kern);
                if declared > 0 || !disk.is_empty() {
                    write_file(dir, "kernel", &disk)?;
                }
                if !kern_dtb.is_empty() {
                    write_file(dir, "kernel_dtb", &kern_dtb)?;
                    say(&format!("  kernel_dtb: {} bytes (FDT found inside kernel) [ok]", kern_dtb.len()));
                }
                blobs.push(mkblob("kernel", "kernel", fmt, on_disk, declared as u32));
                if !kern_dtb.is_empty() {
                    blobs.push(mkblob("kernel_dtb", "kernel_dtb", Format::Raw, "raw", kern_dtb.len() as u32));
                }
            }
            "ramdisk" => {
                if data.is_empty() && declared == 0 {
                    say("ramdisk: absent (size 0, GKI-style image) [ok]");
                    ramdisks.push(spec::RamdiskSpec {
                        file: "ramdisk.cpio".to_string(),
                        name: String::new(),
                        etype: "platform".to_string(),
                        stored_format: "raw".to_string(),
                        on_disk: "raw".to_string(),
                        declared_size: 0,
                        board_id_hex: None,
                    });
                    continue;
                }
                let fmt = codec::sniff(data);
                match try_ramdisk_cpio(data, fmt) {
                    Some((dec, n)) => {
                        say(&format!(
                            "ramdisk: {}/{} bytes fmt={} -> cpio {} files [{}]",
                            s.len, declared, fmt.name(), n, status
                        ));
                        let (disk, on_disk) = if cli.raw { (data.to_vec(), "raw") } else { (dec, "decompressed") };
                        if declared > 0 || !disk.is_empty() {
                            write_file(dir, "ramdisk.cpio", &disk)?;
                        }
                        if cli.extract && on_disk == "decompressed" {
                            extract_boot_report(dir, "ramdisk.cpio", &disk)?;
                        } else if cli.extract {
                            match try_ramdisk_cpio(data, fmt) {
                                Some((dec2, _)) => extract_boot_report(dir, "ramdisk.cpio", &dec2)?,
                                None => say("  extract: skipped ramdisk.cpio (not a cpio)"),
                            }
                        }
                        ramdisks.push(spec::RamdiskSpec {
                            file: "ramdisk.cpio".to_string(),
                            name: String::new(),
                            etype: "platform".to_string(),
                            stored_format: fmt.name().to_string(),
                            on_disk: on_disk.to_string(),
                            declared_size: declared as u32,
                            board_id_hex: None,
                        });
                    }
                    None => {
                        degraded = true;
                        let why = if fmt.is_compressed() {
                            match codec::decompress(fmt, data) {
                                Ok(dec) => match cpio::parse(&dec) {
                                    Ok(_) => "cpio without TRAILER".to_string(),
                                    Err(e) => format!("{fmt:?} ok but cpio bad: {e}"),
                                },
                                Err(e) => format!("{fmt:?} decompress fails: {e}"),
                            }
                        } else {
                            match cpio::parse(data) {
                                Ok(_) => "cpio without TRAILER".to_string(),
                                Err(e) => format!("not a cpio ({e}) and no compression magic"),
                            }
                        };
                        say(&format!("ramdisk: {}/{} bytes fmt={} -> INVALID: {} [{}]", s.len, declared, fmt.name(), why, status));
                        if declared > 0 {
                            write_file(dir, "ramdisk.cpio", data)?;
                        }
                        ramdisks.push(spec::RamdiskSpec {
                            file: "ramdisk.cpio".to_string(),
                            name: String::new(),
                            etype: "platform".to_string(),
                            stored_format: fmt.name().to_string(),
                            on_disk: "raw".to_string(),
                            declared_size: declared as u32,
                            board_id_hex: None,
                        });
                    }
                }
            }
            "dtb" => {
                let note = if data.is_empty() {
                    "absent".to_string()
                } else {
                    match dtb::verify(data, data.len()) {
                        Ok(n) => format!("{n} FDT(s) [ok]"),
                        Err(e) => {
                            degraded = true;
                            format!("INVALID: {e}")
                        }
                    }
                };
                say(&format!("dtb: {}/{} bytes [{}] {}", s.len, declared, status, note));
                if !data.is_empty() {
                    write_file(dir, "dtb", data)?;
                }
                blobs.push(mkblob("dtb", "dtb", Format::Raw, "raw", declared as u32));
            }
            _ => {
                // second | extra | recovery_dtbo | signature: raw, except
                // extra (magiskboot parity: decompress when compressed).
                let fmt = codec::sniff(data);
                let (disk, on_disk, note) = if s.name == "extra" && !cli.raw && fmt.is_compressed() {
                    match codec::decompress(fmt, data) {
                        Ok(dec) => (dec, "decompressed", format!("fmt={} decompressed", fmt.name())),
                        Err(e) => {
                            degraded = true;
                            (data.to_vec(), "raw", format!("INVALID: {} decompress fails: {e}", fmt.name()))
                        }
                    }
                } else {
                    (data.to_vec(), "raw", format!("fmt={}", fmt.name()))
                };
                let presence = if declared == 0 && data.is_empty() { "absent" } else { &status };
                say(&format!("{}: {}/{} bytes {} [{}]", s.name, s.len, declared, note, presence));
                if !disk.is_empty() {
                    write_file(dir, s.file, &disk)?;
                }
                blobs.push(mkblob(s.name, s.file, fmt, on_disk, declared as u32));
            }
        }
    }

    write_footer(dir, bytes, img.footer_off)?;
    if cli.header {
        write_boot_header(dir, h)?;
    }
    if cli.spec {
        spec::write_spec(
            dir,
            &spec::Spec {
                image: spec::ImageSpec {
                    kind: "boot".to_string(),
                    header_version: h.version,
                    page_size: h.page_size,
                    kernel_addr: (h.version < 3).then_some(h.kernel_addr),
                    ramdisk_addr: (h.version < 3).then_some(h.ramdisk_addr),
                    second_addr: (h.version < 3).then_some(h.second_addr),
                    tags_addr: (h.version < 3).then_some(h.tags_addr),
                    os_version: spec::os_human(h.os_version).map(|(v, _)| v),
                    os_patch_level: spec::os_human(h.os_version).map(|(_, p)| p),
                    name: Some(trim_nul(&h.name).into_owned()),
                    cmdline: Some(h.cmdline_full()),
                    extra_cmdline: None,
                    dtb_addr: (h.version == 2).then_some(h.dtb_addr),
                    header_size: (h.version >= 1).then_some(h.header_size),
                    bootconfig_size: None,
                },
                ramdisk: ramdisks,
                blob: blobs,
            },
        )?;
    }
    Ok(degraded)
}

/// NUL-trimmed lossy view of a fixed-size header string field.
fn trim_nul(b: &[u8]) -> std::borrow::Cow<'_, str> {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end])
}

fn declared_len(img: &bootimg::BootImage, name: &str) -> usize {
    let h = &img.hdr;
    match name {
        "kernel" => h.kernel_size as usize,
        "ramdisk" => h.ramdisk_size as usize,
        "second" => h.second_size as usize,
        "extra" => 0, // nonstandard: no header field on clean images
        "recovery_dtbo" => h.recovery_dtbo_size as usize,
        "dtb" => h.dtb_size as usize,
        "signature" => h.signature_size as usize,
        _ => 0,
    }
}

fn mkblob(name: &str, file: &str, fmt: Format, on_disk: &str, declared: u32) -> spec::BlobSpec {
    spec::BlobSpec {
        name: name.to_string(),
        file: file.to_string(),
        stored_format: fmt.name().to_string(),
        on_disk: on_disk.to_string(),
        declared_size: declared,
    }
}

fn maybe_decompress(raw_flag: bool, fmt: Format, data: &[u8]) -> (Vec<u8>, &'static str) {
    if !raw_flag && fmt.is_compressed() {
        if let Ok(dec) = codec::decompress(fmt, data) {
            return (dec, "decompressed");
        }
    }
    (data.to_vec(), "raw")
}

/// Decompress + require a TRAILER-bearing cpio.
fn try_ramdisk_cpio(data: &[u8], fmt: Format) -> Option<(Vec<u8>, usize)> {
    let dec = if fmt.is_compressed() { codec::decompress(fmt, data).ok()? } else { data.to_vec() };
    let en = cpio::parse(&dec).ok()?;
    if en.iter().all(|e| cpio::name_str(e) != "TRAILER!!!") {
        return None;
    }
    let n = en.iter().filter(|e| cpio::name_str(e) != "TRAILER!!!").count();
    Some((dec, n))
}

fn extract_boot_report(dir: &Path, fname: &str, cpio_bytes: &[u8]) -> Result<()> {
    let stem = fname.strip_suffix(".cpio").unwrap_or(fname);
    let dest = dir.join(format!("{stem}.d"));
    match extract::extract(cpio_bytes, &dest) {
        Ok(r) => {
            say(&format!(
                "  extract: {} -> {} files, {} dirs, {} symlinks{}",
                dest.display(),
                r.files,
                r.dirs,
                r.symlinks,
                if r.skipped.is_empty() { String::new() } else { format!(" ({} skipped)", r.skipped.len()) }
            ));
            Ok(())
        }
        Err(e) => {
            say(&format!("  extract: {fname} failed: {e}"));
            Ok(())
        }
    }
}

fn write_boot_header(dir: &Path, h: &bootimg::BootHeader) -> Result<()> {
    // magiskboot-compatible keys.
    let mut out = String::new();
    out.push_str(&format!("name={}\n", String::from_utf8_lossy(&h.name)));
    out.push_str(&format!("cmdline={}\n", h.cmdline_full()));
    if let Some((v, p)) = h.os_version_str() {
        out.push_str(&format!("os_version={v}\n"));
        out.push_str(&format!("os_patch_level={p}\n"));
    }
    write_file(dir, "header", out.as_bytes())?;
    Ok(())
}
