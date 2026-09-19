//! bootsmasher — standalone static Android boot-image smasher.
//!
//! Subprograms, each in its own directory: `vboot` (vendor_boot repair
//! flow), `unpack` (extraction for boot + vendor_boot), `repack`
//! (rebuild from an unpack dir), `cpio` (in-place newc surgery),
//! `compress[=fmt]` + `decompress` (single-file codecs). Shared building
//! blocks live in `common`; the global help lives in `help`, per-subprogram
//! texts in `<sub>/help.rs`. Every help prints the argv[0] basename, so a
//! renamed binary documents itself correctly.

mod common;
mod help;
mod vboot;
mod unpack;
mod repack;
mod cpio;
mod compress;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn say(line: &str) {
    use std::io::Write as _;
    let mut o = std::io::stdout().lock();
    let _ = writeln!(o, "{line}");
    let _ = o.flush();
}

/// Binary basename (`/usr/bin/bootsmasher` -> `bootsmasher`): this is the
/// `{prog}` every help text prints.
fn prog_name(argv0: &str) -> &str {
    argv0.rsplit(['/', '\\']).next().unwrap_or(argv0)
}

/// Print the manual for one subprogram (short or expand flavor).
/// Returns None for an unknown name.
fn sub_help(name: &str, prog: &str, expand: bool) -> Option<i32> {
    let text = match name {
        "vboot" => {
            if expand {
                vboot::help::expand(prog)
            } else {
                vboot::help::short(prog)
            }
        }
        "unpack" => {
            if expand {
                unpack::help::expand(prog)
            } else {
                unpack::help::short(prog)
            }
        }
        "repack" => {
            if expand {
                repack::help::expand(prog)
            } else {
                repack::help::short(prog)
            }
        }
        "cpio" => {
            if expand {
                cpio::help::expand(prog)
            } else {
                cpio::help::short(prog)
            }
        }
        "compress" => {
            if expand {
                compress::help::expand_compress(prog)
            } else {
                compress::help::short_compress(prog)
            }
        }
        "decompress" => {
            if expand {
                compress::help::expand_decompress(prog)
            } else {
                compress::help::short_decompress(prog)
            }
        }
        _ => return None,
    };
    say(&text);
    Some(0)
}

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let prog = prog_name(argv.first().map(|s| s.as_str()).unwrap_or("bootsmasher")).to_string();
    let args = if argv.len() > 1 { &argv[1..] } else { &[][..] };
    if args.is_empty() {
        eprintln!("{} {}\n\n{}\n\n(run '{prog} --help' for this text)", prog, VERSION, help::short(&prog));
        return std::process::ExitCode::from(1);
    }
    match args[0].as_str() {
        "-V" | "--version" => {
            say(&format!("{prog} {VERSION}"));
            std::process::ExitCode::from(0)
        }
        "-h" | "--help" => {
            say(&format!("{prog} {VERSION}\n\n{}", help::short(&prog)));
            std::process::ExitCode::from(0)
        }
        "--expand" => {
            say(&format!("{prog} {VERSION}\n\n{}", help::expand(&prog)));
            std::process::ExitCode::from(0)
        }
        "help" => {
            // help [expand] [subprogram]
            let rest: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();
            match rest.as_slice() {
                [] => {
                    say(&format!("{prog} {VERSION}\n\n{}", help::short(&prog)));
                    std::process::ExitCode::from(0)
                }
                ["expand"] => {
                    say(&format!("{prog} {VERSION}\n\n{}", help::expand(&prog)));
                    std::process::ExitCode::from(0)
                }
                ["expand", sub] => match sub_help(sub, &prog, true) {
                    Some(code) => std::process::ExitCode::from(code as u8),
                    None => {
                        eprintln!("unknown subprogram '{sub}' (want vboot|unpack|repack|cpio|compress|decompress)");
                        std::process::ExitCode::from(1)
                    }
                },
                [sub] => match sub_help(sub, &prog, false) {
                    Some(code) => std::process::ExitCode::from(code as u8),
                    None => {
                        eprintln!("unknown subprogram '{sub}' (want vboot|unpack|repack|cpio|compress|decompress)");
                        std::process::ExitCode::from(1)
                    }
                },
                _ => {
                    eprintln!("usage error: too many arguments (want 'help [expand] [subprogram]')");
                    std::process::ExitCode::from(1)
                }
            }
        }
        other if other == "compress" || other.starts_with("compress=") => {
            // The format is embedded in the command name itself.
            let fmt_str = other.strip_prefix("compress=").unwrap_or("gzip");
            match compress::parse_method(fmt_str) {
                Ok(fmt) => std::process::ExitCode::from(compress::run_compress(fmt, &args[1..], &prog) as u8),
                Err(e) => {
                    eprintln!("usage error: {e}");
                    std::process::ExitCode::from(1)
                }
            }
        }
        "vboot" => std::process::ExitCode::from(vboot::run(&args[1..], &prog) as u8),
        "unpack" => std::process::ExitCode::from(unpack::run(&args[1..], &prog) as u8),
        "repack" => std::process::ExitCode::from(repack::run(&args[1..], &prog) as u8),
        "cpio" => std::process::ExitCode::from(cpio::run(&args[1..], &prog) as u8),
        "decompress" => std::process::ExitCode::from(compress::run_decompress(&args[1..], &prog) as u8),
        other => {
            eprintln!("unknown subprogram '{other}' (want vboot|unpack|repack|cpio|compress|decompress|compress=fmt)");
            std::process::ExitCode::from(1)
        }
    }
}
