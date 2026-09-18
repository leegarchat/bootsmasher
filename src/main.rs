//! bootsmasher — standalone static Android boot-image smasher.
//! Subprograms: `vboot` (Pixel 6 vendor_boot repair flow), `unpack`
//! (magiskboot-compatible extraction for boot + vendor_boot), `repack`
//! (rebuild from an unpack dir, spec, base or template image).

mod error;
mod vboot;
mod bootimg;
mod codec;
mod cpiox;
mod spec;
mod unpack;
mod repack;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn say(line: &str) {
    use std::io::Write as _;
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{line}");
    let _ = o.flush();
}

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let program = argv.first().map(|s| s.as_str()).unwrap_or("bootsmasher");
    let args = if argv.len() > 1 { &argv[1..] } else { &[][..] };
    if args.is_empty() {
        eprintln!("bootsmasher {VERSION}\nUsage:\n  {program} vboot <args...>\n  {program} vboot --help\n  {program} --version");
        return std::process::ExitCode::from(1);
    }
    match args[0].as_str() {
        "-V" | "--version" => {
            say(&format!("bootsmasher {VERSION}"));
            std::process::ExitCode::from(0)
        }
        "-h" | "--help" | "help" => {
            say(&format!("bootsmasher {VERSION} — subprograms:\n  vboot    vendor_boot smart repair flow (Pixel 6)\n  unpack   extract boot/vendor_boot (magiskboot superset)\n  repack   rebuild boot/vendor_boot from an unpack dir\n\n  {program} <subprogram> --help"));
            std::process::ExitCode::from(0)
        }
        "vboot" => std::process::ExitCode::from(vboot::run(&args[1..]) as u8),
        "unpack" => std::process::ExitCode::from(unpack::run(&args[1..]) as u8),
        "repack" => std::process::ExitCode::from(repack::run(&args[1..]) as u8),
        other => {
            eprintln!("unknown subprogram '{other}' (want vboot|unpack|repack)");
            std::process::ExitCode::from(1)
        }
    }
}
