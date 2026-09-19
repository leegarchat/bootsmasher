//! `bootsmasher pick` — arrow-key menu for installer scripts.
//!
//! Prints the chosen option to stdout (exit 0). Esc or `q` aborts with
//! an empty stdout and exit 1 (`aborted by user`, no help dump — scripts
//! treat any non-zero exit plus empty stdout as "user walked away").
//! Ctrl-C kills via SIGINT (standard terminal behavior, not an exit code).
//! Without a terminal on stdin the `--default` option wins (for pipes
//! and CI); without it that is a usage error. Pure Rust (dialoguer,
//! default-features off: no editor/password/fuzzy backends).

use std::io::IsTerminal as _;

use crate::common::error::{Error, Result};

pub(crate) mod help;

struct Cli {
    prompt: String,
    /// 0-based preselected index (--default is 1-based on the CLI).
    default: Option<usize>,
    options: Vec<String>,
}

fn parse_cli(args: &[String]) -> Result<Cli> {
    let mut prompt = "Select:".to_string();
    let mut default: Option<usize> = None;
    let mut options: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--prompt" => {
                i += 1;
                prompt = args
                    .get(i)
                    .ok_or_else(|| Error::Usage("--prompt needs a value".to_string()))?
                    .clone();
            }
            "--default" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or_else(|| Error::Usage("--default needs a 1-based index".to_string()))?;
                let n: usize = v
                    .parse()
                    .map_err(|_| Error::Usage("--default needs a 1-based index".to_string()))?;
                if n == 0 {
                    return Err(Error::Usage("--default is 1-based (first option is 1)".to_string()));
                }
                default = Some(n - 1);
            }
            s if s.starts_with('-') => return Err(Error::Usage(format!("unknown flag {s}"))),
            o => options.push(o.to_string()),
        }
        i += 1;
    }
    if options.is_empty() {
        return Err(Error::Usage("need at least one option (pick [--prompt T] [--default N] opt...)".to_string()));
    }
    if let Some(d) = default {
        if d >= options.len() {
            return Err(Error::Usage(format!(
                "--default {} out of range (have {} options)",
                d + 1,
                options.len()
            )));
        }
    }
    Ok(Cli { prompt, default, options })
}

fn pick_inner(args: &[String]) -> Result<String> {
    let cli = parse_cli(args)?;
    if !std::io::stdin().is_terminal() {
        // Pipes and CI: no arrows to press, the default wins.
        return match cli.default {
            Some(d) => Ok(cli.options[d].clone()),
            None => Err(Error::Usage(
                "stdin is not a terminal (pass --default for non-interactive use)".to_string(),
            )),
        };
    }
    let mut sel = dialoguer::Select::new().with_prompt(&cli.prompt).items(&cli.options);
    if let Some(d) = cli.default {
        sel = sel.default(d);
    }
    match sel.interact_opt() {
        Ok(Some(i)) => Ok(cli.options[i].clone()),
        Ok(None) => Err(Error::Fail("aborted by user".to_string())),
        // Ctrl-C raises SIGINT to self before we get here (standard);
        // this arm only fires if the signal was caught elsewhere.
        Err(e) if e.to_string().contains("interrupted") => {
            Err(Error::Fail("aborted by user".to_string()))
        }
        Err(e) => Err(Error::Io(format!("menu failed: {e}"))),
    }
}

/// Run `pick`. Exit code (choice goes to stdout, nothing else may).
pub fn run(args: &[String], prog: &str) -> i32 {
    if args.iter().any(|a| a == "--help") {
        println!("{}", help::short(prog));
        return 0;
    }
    if args.iter().any(|a| a == "--expand") {
        println!("{}", help::expand(prog));
        return 0;
    }
    match pick_inner(args) {
        Ok(choice) => {
            println!("{choice}");
            0
        }
        Err(Error::Usage(m)) => {
            eprintln!("usage error: {m}\n{}", help::short(prog));
            1
        }
        Err(Error::Fail(m)) => {
            eprintln!("{m}");
            1
        }
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}
