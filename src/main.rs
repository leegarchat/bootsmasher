//! bootsmasher — standalone static Android boot-image smasher.
//! Subprograms: `vboot` (Pixel 6 vendor_boot repair flow), `unpack`
//! (magiskboot-compatible extraction for boot + vendor_boot), `repack`
//! (rebuild from an unpack dir, spec, base or template image).
//! Every subprogram has short aliases (vb, u/up, r/rp) and --help.

mod error;
mod vboot;
mod bootimg;
mod codec;
mod cpiox;
mod cpio_cmd;
mod spec;
mod unpack;
mod repack;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const GLOBAL_HELP: &str = "bootsmasher — standalone static Android boot-image surgery.

Subprograms (short aliases in brackets):
  vboot [vb]     Smart vendor_boot repair flow (Pixel 6 / gs101):
                 stale-table normalize, platform replace,
                 --split-first-stage / --merge partition modes,
                 pre-emit verify, stdout mode, --min-free space gate.
  unpack [u|up]  Extract boot.img (v0..v4, kernels, dtb/dtbo) and
                 vendor_boot with auto-decompression and honest
                 per-section verdicts (broken parts are dumped anyway,
                 with the exact WHY — never dies like magiskboot).
  repack [r|rp]  Rebuild boot/vendor_boot from an unpack dir:
                 spec.toml or --base layout, --template foreign headers,
                 --set overrides, --format transcoding, footer control,
                 in-memory re-verify before writing.
  cpio [c]       In-place newc archive surgery (magiskboot port):
                 exists/ls/rm/mkdir/ln/mv/add/extract/test/patch/
                 backup/restore on raw 070701 cpio files.

Usage:
  bootsmasher <subprogram> [args...]
  bootsmasher <subprogram> --help     Full manual for one subprogram.
  bootsmasher help [subprogram]       Same as above.
  bootsmasher --version

Examples:
  bootsmasher unpack vendor_boot.img --out-dir dir -h --extract
  bootsmasher repack dir fixed.img --base stock.img
  bootsmasher vboot broken.img fox.lz4 -o fox_boot.img
  bootsmasher u boot.img -o dir -x
  bootsmasher cpio ramdisk.cpio \"exists init\" \"ls -r /system\"

Exit codes everywhere: 0 ok (unpack: possibly DEGRADED, see report),
1 usage error, 2 broken input / failed verification.";

fn say(line: &str) {
    use std::io::Write as _;
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{line}");
    let _ = o.flush();
}

/// Canonical subprogram name or None. Short aliases:
/// vb=vboot, u/up=unpack, r/rp=repack, c=cpio.
fn canonical(name: &str) -> Option<&'static str> {
    match name {
        "vboot" | "vb" => Some("vboot"),
        "unpack" | "u" | "up" => Some("unpack"),
        "repack" | "r" | "rp" => Some("repack"),
        "cpio" | "c" => Some("cpio"),
        _ => None,
    }
}

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let program = argv.first().map(|s| s.as_str()).unwrap_or("bootsmasher");
    let args = if argv.len() > 1 { &argv[1..] } else { &[][..] };
    if args.is_empty() {
        eprintln!("bootsmasher {VERSION}\n\n{GLOBAL_HELP}\n\n(run '{program} --help' for this text)");
        return std::process::ExitCode::from(1);
    }
    match args[0].as_str() {
        "-V" | "--version" => {
            say(&format!("bootsmasher {VERSION}"));
            std::process::ExitCode::from(0)
        }
        "-h" | "--help" => {
            say(&format!("bootsmasher {VERSION}\n\n{GLOBAL_HELP}"));
            std::process::ExitCode::from(0)
        }
        "help" => {
            // bootsmasher help [subprogram] — route to the sub manual.
            if args.len() > 1 {
                match canonical(&args[1]) {
                    Some("vboot") => std::process::ExitCode::from(vboot::run(&["--help".to_string()]) as u8),
                    Some("unpack") => std::process::ExitCode::from(unpack::run(&["--help".to_string()]) as u8),
                    Some("repack") => std::process::ExitCode::from(repack::run(&["--help".to_string()]) as u8),
                    Some("cpio") => std::process::ExitCode::from(cpio_cmd::run(&["--help".to_string()]) as u8),
                    _ => {
                        eprintln!("unknown subprogram '{}' (want vboot|unpack|repack|cpio)", args[1]);
                        std::process::ExitCode::from(1)
                    }
                }
            } else {
                say(&format!("bootsmasher {VERSION}\n\n{GLOBAL_HELP}"));
                std::process::ExitCode::from(0)
            }
        }
        other => match canonical(other) {
            Some("vboot") => std::process::ExitCode::from(vboot::run(&args[1..]) as u8),
            Some("unpack") => std::process::ExitCode::from(unpack::run(&args[1..]) as u8),
            Some("repack") => std::process::ExitCode::from(repack::run(&args[1..]) as u8),
            Some("cpio") => std::process::ExitCode::from(cpio_cmd::run(&args[1..]) as u8),
            _ => {
                eprintln!("unknown subprogram '{other}' (want vboot|vb|unpack|u|up|repack|r|rp|cpio|c)");
                std::process::ExitCode::from(1)
            }
        },
    }
}
