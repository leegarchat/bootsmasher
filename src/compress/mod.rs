//! `bootsmasher compress` / `bootsmasher decompress` — magiskboot parity.
//!
//! `compress[=format]` squeezes one file with the named codec (default
//! gzip); `decompress` sniffs the magic and expands it back. `-` on either
//! side means binary stdin/stdout. Without an outfile the input file is
//! replaced: compress appends the format extension, decompress strips it
//! (and aborts when the name does not carry it, like magiskboot).
//!
//! Pure Rust on every target: gzip/xz/lzma/lz4-frame/lz4-legacy go through
//! `crate::common::codec`. There is no bzip2 backend (only what Cargo.toml already
//! has), so `compress=bzip2` is an honest usage error and a BZh blob on
//! `decompress` is an "unsupported format" failure (exit 2), never a panic.
//! That is the single deliberate deviation from magiskboot.

use std::io::{Read as _, Write as _};

use crate::common::codec::{self, Format};
use crate::common::error::{Error, Result};

pub(crate) mod help;



/// magiskboot's `FileFormat::from_str` set, minus bzip2 (no backend).
/// Unknown names keep magiskboot's message verbatim.
pub fn parse_method(s: &str) -> Result<Format> {
    match s {
        "gzip" => Ok(Format::Gzip),
        "xz" => Ok(Format::Xz),
        "lzma" => Ok(Format::Lzma),
        "bzip2" => Err(Error::Usage("bzip2 not supported in this build (no pure-Rust backend)".to_string())),
        "lz4" => Ok(Format::Lz4Frame),
        "lz4_legacy" | "lz4_lg" => Ok(Format::Lz4Legacy),
        _ => Err(Error::Usage(format!("Unsupported or unknown compression format: {s}"))),
    }
}

/// magiskboot's `FileFormat::ext()`: lz4_legacy shares lz4's extension.
fn ext_of(fmt: Format) -> &'static str {
    match fmt {
        Format::Gzip => "gz",
        Format::Xz => "xz",
        Format::Lzma => "lzma",
        Format::Lz4Frame | Format::Lz4Legacy => "lz4",
        Format::Raw => "",
    }
}

/// Run `compress[=fmt]` (fmt already resolved, default gzip). Exit code.
pub fn run_compress(fmt: Format, args: &[String], prog: &str) -> i32 {
    if args.iter().any(|a| a == "--help") {
        println!("{}", help::short_compress(prog));
        return 0;
    }
    if args.iter().any(|a| a == "--expand") {
        println!("{}", help::expand_compress(prog));
        return 0;
    }
    match compress_inner(fmt, args) {
        Ok(()) => 0,
        Err(Error::Usage(m)) => {
            eprintln!("usage error: {m}\n{}", help::short_compress(prog));
            1
        }
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

/// Run `decompress`. Exit code.
pub fn run_decompress(args: &[String], prog: &str) -> i32 {
    if args.iter().any(|a| a == "--help") {
        println!("{}", help::short_decompress(prog));
        return 0;
    }
    if args.iter().any(|a| a == "--expand") {
        println!("{}", help::expand_decompress(prog));
        return 0;
    }
    match decompress_inner(args) {
        Ok(()) => 0,
        Err(Error::Usage(m)) => {
            eprintln!("usage error: {m}\n{}", help::short_decompress(prog));
            1
        }
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

fn positionals(args: &[String], what: &str) -> Result<(String, Option<String>)> {
    let mut pos: Vec<String> = Vec::new();
    for a in args {
        if a == "-" {
            pos.push(a.clone());
        } else if a.starts_with('-') {
            return Err(Error::Usage(format!("unknown flag {a} ({what} takes only <infile> [outfile])")));
        } else {
            pos.push(a.clone());
        }
    }
    match pos.len() {
        0 => Err(Error::Usage(format!("need <infile> ({what} <infile> [outfile])"))),
        1 => Ok((pos.remove(0), None)),
        2 => {
            let out = pos.pop();
            let inp = pos.pop().unwrap_or_default();
            Ok((inp, out))
        }
        _ => Err(Error::Usage(format!("too many arguments ({what} <infile> [outfile])"))),
    }
}

fn read_input(infile: &str) -> Result<Vec<u8>> {
    if infile == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .lock()
            .read_to_end(&mut buf)
            .map_err(|e| Error::Io(format!("cannot read stdin: {e}")))?;
        Ok(buf)
    } else {
        std::fs::read(infile).map_err(|e| Error::Io(format!("cannot read {infile}: {e}")))
    }
}

fn write_output(outfile: &str, data: &[u8]) -> Result<()> {
    if outfile == "-" {
        let mut o = std::io::stdout().lock();
        o.write_all(data).map_err(|e| Error::Io(format!("cannot write stdout: {e}")))?;
        o.flush().map_err(|e| Error::Io(format!("cannot flush stdout: {e}")))?;
        Ok(())
    } else {
        std::fs::write(outfile, data).map_err(|e| Error::Io(format!("cannot write {outfile}: {e}")))
    }
}

fn compress_inner(fmt: Format, args: &[String]) -> Result<()> {
    let (infile, outfile) = positionals(args, "compress[=format]")?;
    let data = read_input(&infile)?;
    let enc = codec::compress(fmt, &data)?;
    // Destination: explicit outfile, stdout for stdin, else infile + ext.
    let mut rm_in = false;
    let dest = match outfile {
        Some(o) => o,
        None if infile == "-" => "-".to_string(),
        None => {
            let o = format!("{infile}.{}", ext_of(fmt));
            eprintln!("Compressing to [{o}]");
            rm_in = true;
            o
        }
    };
    write_output(&dest, &enc)?;
    if rm_in {
        std::fs::remove_file(&infile).map_err(|e| Error::Io(format!("cannot remove {infile}: {e}")))?;
    }
    Ok(())
}

fn decompress_inner(args: &[String]) -> Result<()> {
    let (infile, outfile) = positionals(args, "decompress")?;
    let data = read_input(&infile)?;
    // bzip2 has no backend here: fail loudly (exit 2), never panic.
    if data.len() >= 3 && data[0..3] == [b'B', b'Z', b'h'] {
        return Err(Error::Parse("unsupported format: bzip2 not supported in this build".to_string()));
    }
    let fmt = codec::sniff(&data);
    eprintln!("Detected format: {}", fmt.name());
    if !fmt.is_compressed() {
        return Err(Error::Parse("Input file is not a supported type!".to_string()));
    }
    let mut rm_in = false;
    let dest = match outfile {
        Some(o) => o,
        None if infile == "-" => "-".to_string(),
        None => {
            // Strip the archive extension; a name without it is an error,
            // exactly like magiskboot.
            let ext = ext_of(fmt);
            match infile.rsplit_once('.') {
                Some((stem, e)) if e == ext => {
                    let o = stem.to_string();
                    eprintln!("Decompressing to [{o}]");
                    rm_in = true;
                    o
                }
                _ => return Err(Error::Parse("Input file is not a supported type!".to_string())),
            }
        }
    };
    let dec = codec::decompress(fmt, &data)?;
    write_output(&dest, &dec)?;
    if rm_in {
        std::fs::remove_file(&infile).map_err(|e| Error::Io(format!("cannot remove {infile}: {e}")))?;
    }
    Ok(())
}
