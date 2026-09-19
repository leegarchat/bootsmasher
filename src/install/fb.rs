//! Fastboot/adb transport for `bootsmasher install`.
//!
//! Thin wrappers over the external `fastboot`/`adb` binaries from
//! export.txt. Tool chatter goes to the run log, never to the console;
//! the console only gets the short human-readable lines.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::common::error::{Error, Result};

/// Hard cap for one tool call: a dropped USB must fail the run,
/// never hang the installer. (Note: `fastboot -s SERIAL ...` waits
/// forever for a missing serial, so callers only probe serials known
/// present; this is the backstop.)
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// Resolved tool binaries.
pub struct Tools {
    pub fastboot: PathBuf,
    pub adb: PathBuf,
}

/// Run a tool, capture output, append the full transcript to the log.
/// The command itself is never printed to the console. Bounded by
/// TOOL_TIMEOUT: on expiry the child is killed and an error returned.
pub fn cmd(bin: &Path, args: &[String], log: &mut File) -> Result<Output> {
    let mut line = format!("$ {}", bin.display());
    for a in args {
        line.push(' ');
        line.push_str(a);
    }
    let _ = writeln!(log, "{line}");
    let mut child = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Io(format!("cannot run {}: {e}", bin.display())))?;
    let deadline = Instant::now() + TOOL_TIMEOUT;
    let status = loop {
        match child.try_wait().map_err(|e| Error::Io(format!("wait failed: {e}")))? {
            Some(st) => break st,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = writeln!(log, "--- TIMEOUT after {}s, killed ---", TOOL_TIMEOUT.as_secs());
                    return Err(Error::Io(format!("{} timed out after {}s", bin.display(), TOOL_TIMEOUT.as_secs())));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    let mut so = Vec::new();
    let mut se = Vec::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_end(&mut so);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_end(&mut se);
    }
    let _ = writeln!(log, "--- rc={} ---", status);
    let _ = log.write_all(&so);
    let _ = log.write_all(&se);
    let _ = writeln!(log, "--- end ---");
    let _ = log.flush();
    // Rebuild Output from the captured streams.
    Ok(Output { status, stdout: so, stderr: se })
}

fn s(bin: &Path, args: &[&str], log: &mut File) -> Result<Output> {
    cmd(bin, &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(), log)
}

/// Serials from `<tool> devices`: lines whose second column is `want`.
pub fn serials(bin: &Path, want: &str, log: &mut File) -> Vec<String> {
    let out = match s(bin, &["devices"], log) {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let blob = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut v = Vec::new();
    for line in blob.lines() {
        let mut cols = line.split_whitespace();
        if let (Some(ser), Some(state)) = (cols.next(), cols.next()) {
            if state == want {
                v.push(ser.to_string());
            }
        }
    }
    v
}

/// `fastboot -s SERIAL getvar KEY` value (first token after `KEY:`),
/// searched in stdout+stderr. None when the var is missing/unreachable.
pub fn getvar(fb: &Path, serial: &str, key: &str, log: &mut File) -> Option<String> {
    let out = s(fb, &["-s", serial, "getvar", key], log).ok()?;
    let blob = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let needle = format!("{key}:");
    for line in blob.lines() {
        if let Some(pos) = line.find(&needle) {
            let val = line[pos + needle.len()..].trim();
            if !val.is_empty() {
                return val.split_whitespace().next().map(|t| t.to_string());
            }
        }
    }
    None
}

/// True when the device answers `is-userspace: yes` (fastbootd, not
/// the bootloader we need). Unreachable counts as false; callers probe
/// reachability separately.
pub fn is_userspace(fb: &Path, serial: &str, log: &mut File) -> bool {
    getvar(fb, serial, "is-userspace", log).as_deref() == Some("yes")
}

/// Wait up to `timeout_s` for exactly one non-fastbootd device.
/// Polls every 2 s like the reference script.
pub fn wait_bootloader(fb: &Path, timeout_s: u64, log: &mut File) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(timeout_s);
    while Instant::now() < deadline {
        let all = serials(fb, "fastboot", log);
        let mut bare: Vec<String> = Vec::new();
        for ser in &all {
            if !is_userspace(fb, ser, log) {
                bare.push(ser.clone());
            }
        }
        if bare.len() == 1 {
            return Some(bare.remove(0));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    None
}

pub fn fetch(fb: &Path, serial: &str, part: &str, out: &Path, log: &mut File) -> Result<()> {
    let o = s(
        fb,
        &["-s", serial, "fetch", part, &format!("{}", out.display())],
        log,
    )?;
    if o.status.success() {
        Ok(())
    } else {
        Err(Error::Verify(format!("fetch {part} failed")))
    }
}

pub fn flash(fb: &Path, serial: &str, part: &str, img: &Path, log: &mut File) -> Result<()> {
    let o = s(
        fb,
        &["-s", serial, "flash", part, &format!("{}", img.display())],
        log,
    )?;
    if o.status.success() {
        Ok(())
    } else {
        Err(Error::Verify(format!("flash {part} failed")))
    }
}

/// Best-effort reboot to bootloader (result ignored by the caller:
/// the wait loop decides).
pub fn reboot_bootloader(fb: &Path, serial: &str, log: &mut File) {
    let _ = s(fb, &["-s", serial, "reboot", "bootloader"], log);
}

/// True when `adb -s SERIAL get-state` exits 0 (device in system).
pub fn adb_alive(adb: &Path, serial: &str, log: &mut File) -> bool {
    s(adb, &["-s", serial, "get-state"], log).map(|o| o.status.success()).unwrap_or(false)
}

/// `adb -s SERIAL shell getprop PROP`, trimmed. None when empty/unreachable.
pub fn adb_prop(adb: &Path, serial: &str, prop: &str, log: &mut File) -> Option<String> {
    let o = s(adb, &["-s", serial, "shell", "getprop", prop], log).ok()?;
    if !o.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Human device label over adb: marketing model, else codename.
pub fn adb_label(adb: &Path, serial: &str, log: &mut File) -> Option<String> {
    adb_prop(adb, serial, "ro.product.model", log)
        .or_else(|| adb_prop(adb, serial, "ro.product.device", log))
}

/// Best-effort `adb reboot bootloader` (result ignored: wait decides).
pub fn adb_reboot_bootloader(adb: &Path, serial: &str, log: &mut File) {
    let _ = s(adb, &["-s", serial, "reboot", "bootloader"], log);
}

/// First two lines of `fastboot --version` for the log header.
pub fn version(fb: &Path, log: &mut File) {
    if let Ok(o) = s(fb, &["--version"], log) {
        let blob = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        for line in blob.lines().take(2) {
            let _ = writeln!(log, "{line}");
        }
    }
}
