//! `bootsmasher install` — OrangeFox vendor_boot installer, in-binary.
//!
//! Cross-platform port of `recovery_install_components/install.sh`:
//! everything runs in the BOOTLOADER (classic fastboot, NOT fastbootd).
//! Rebuild/verify/report reuse the in-process `vboot` ops; only
//! device traffic (fetch/flash/getvar/reboot) shells out to the
//! external fastboot/adb binaries from export.txt.
//!
//! Flow: [1/5] device, [2/5] mode/slot/backup selections with
//! back-navigation, [3/5] fetch stock, [4/5] prepare + per-slot report,
//! flash confirmation, [5/5] flash + fetch-back proof. Exit codes:
//! 0 ok (or clean abort), 1 usage / no device, 2 build/verify/flash.

pub(crate) mod help;
mod fb;

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::common::codec;
use crate::common::cpio;
use crate::common::vendor::type_name;
use crate::vboot::ops::{self, Mode, RecoveryInPlatform, RepackOpts};
use console::style;

use fb::Tools;

// ---------------------------------------------------------------- CLI ---

struct Cli {
    force: bool,
    slot: String,
    mode: String,
    backup: String,
    export: String,
    file: bool,
    input: String,
    cpio: String,
    output: String,
    log: String,
    recovery_is_platform: String,
    transport: String,
    demo: bool,
}

/// Maps a `--recovery-is-platform` value to the ops variant.
fn layout_variant(v: &str) -> Result<Option<RecoveryInPlatform>, String> {
    match v {
        "" => Ok(None),
        "var1" => Ok(Some(RecoveryInPlatform::Var1)),
        "var2" => Ok(Some(RecoveryInPlatform::Var2)),
        _ => Err("--recovery-is-platform needs var1|var2".to_string()),
    }
}

/// Display name for a layout value: Type A/B/C (questionnaire wording).
fn layout_name(v: &str) -> &'static str {
    match v {
        "var1" => "Type B (var1 all-in-platform)",
        "var2" => "Type C (var2 payload-only platform)",
        _ => "Type A (classic recovery fragment)",
    }
}

fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let mut c = Cli { force: false, slot: String::new(), mode: String::new(), backup: String::new(), export: String::new(), file: false, input: String::new(), cpio: String::new(), output: String::new(), log: String::new(), recovery_is_platform: String::new(), transport: String::new(), demo: false };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--force" => c.force = true,
            "--demo" => c.demo = true,
            "--file" => c.file = true,
            "--slot" => {
                i += 1;
                c.slot = args.get(i).ok_or("--slot needs a|b|both".to_string())?.clone();
            }
            s if s.starts_with("--slot=") => c.slot = s["--slot=".len()..].to_string(),
            "--mode" => {
                i += 1;
                c.mode = args.get(i).ok_or("--mode needs install|restore".to_string())?.clone();
            }
            s if s.starts_with("--mode=") => c.mode = s["--mode=".len()..].to_string(),
            "--backup" => {
                i += 1;
                c.backup = args.get(i).ok_or("--backup needs latest|STAMP".to_string())?.clone();
            }
            s if s.starts_with("--backup=") => c.backup = s["--backup=".len()..].to_string(),
            "--export" => {
                i += 1;
                c.export = args.get(i).ok_or("--export needs a file".to_string())?.clone();
            }
            s if s.starts_with("--export=") => c.export = s["--export=".len()..].to_string(),
            "-i" | "--input" => {
                i += 1;
                c.input = args.get(i).ok_or("-i needs a vendor_boot image".to_string())?.clone();
            }
            s if s.starts_with("--input=") => c.input = s["--input=".len()..].to_string(),
            "-c" | "--cpio" => {
                i += 1;
                c.cpio = args.get(i).ok_or("-c needs a recovery cpio payload".to_string())?.clone();
            }
            s if s.starts_with("--cpio=") => c.cpio = s["--cpio=".len()..].to_string(),
            "-o" | "--output" => {
                i += 1;
                c.output = args.get(i).ok_or("-o needs an output path".to_string())?.clone();
            }
            s if s.starts_with("--output=") => c.output = s["--output=".len()..].to_string(),
            "--log" => {
                i += 1;
                c.log = args.get(i).ok_or("--log needs a file".to_string())?.clone();
            }
            s if s.starts_with("--log=") => c.log = s["--log=".len()..].to_string(),
            "--recovery-is-platform" => {
                i += 1;
                if !c.recovery_is_platform.is_empty() {
                    return Err("--recovery-is-platform given twice".to_string());
                }
                c.recovery_is_platform = args.get(i).ok_or("--recovery-is-platform needs var1|var2".to_string())?.clone();
            }
            s if s.starts_with("--recovery-is-platform=") => {
                if !c.recovery_is_platform.is_empty() {
                    return Err("--recovery-is-platform given twice".to_string());
                }
                c.recovery_is_platform = s["--recovery-is-platform=".len()..].to_string();
            }
            "--transport" => {
                i += 1;
                c.transport = args.get(i).ok_or("--transport needs fastboot|adb".to_string())?.clone();
            }
            s if s.starts_with("--transport=") => c.transport = s["--transport=".len()..].to_string(),
            other => return Err(format!("unknown arg: {other}")),
        }
        i += 1;
    }
    if c.demo {
        if c.force || c.file || !c.slot.is_empty() || !c.mode.is_empty() || !c.backup.is_empty()
            || !c.export.is_empty() || !c.input.is_empty() || !c.cpio.is_empty()
            || !c.output.is_empty() || !c.log.is_empty() || !c.recovery_is_platform.is_empty()
            || !c.transport.is_empty()
        {
            return Err("--demo takes no other flags".to_string());
        }
        return Ok(c);
    }
    if c.file {
        if c.force || !c.slot.is_empty() || !c.mode.is_empty() || !c.backup.is_empty() || !c.export.is_empty() || !c.transport.is_empty() {
            return Err("--file takes no --force/--slot/--mode/--backup/--export/--transport".to_string());
        }
        if c.input.is_empty() || c.cpio.is_empty() || c.output.is_empty() {
            return Err("--file needs -i INPUT -c CPIOPAYLOAD -o OUTPUT".to_string());
        }
        // Validate the layout value early (the only extra flag --file takes).
        layout_variant(&c.recovery_is_platform)?;
        return Ok(c);
    }
    if !c.log.is_empty() {
        return Err("--log needs --file".to_string());
    }
    if !c.input.is_empty() || !c.cpio.is_empty() || !c.output.is_empty() {
        return Err("-i/-c/-o need --file".to_string());
    }
    match c.slot.as_str() {
        "" | "a" | "b" | "both" => {}
        _ => return Err("--slot needs a|b|both".to_string()),
    }
    match c.mode.as_str() {
        "" | "install" | "restore" => {}
        _ => return Err("--mode needs install|restore".to_string()),
    }
    match c.transport.as_str() {
        "" | "fastboot" | "adb" => {}
        _ => return Err("--transport needs fastboot|adb".to_string()),
    }
    if !c.backup.is_empty() && c.mode != "restore" {
        return Err("--backup needs --mode restore".to_string());
    }
    Ok(c)
}

// ------------------------------------------------------------- export ---

/// Resolved installer paths. Relative export.txt values resolve
/// against the directory holding export.txt itself.
struct Config {
    recovery_img: PathBuf,
    backup_dir: PathBuf,
    /// Per-run logs live apart from image backups: logs/<stamp>/.
    log_dir: PathBuf,
    tools: Tools,
    /// Minimum free bytes a flashed image must leave in the partition
    /// (export.txt MIN_FREE_MB, default 7).
    min_free: u64,
    min_free_mb: u64,
}

fn strip_quotes(v: &str) -> &str {
    let b = v.as_bytes();
    if b.len() >= 2 && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\'')) {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

fn parse_export(path: &Path) -> Result<HashMap<String, String>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut map = HashMap::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("{}:{}: bad line (want KEY=VALUE): {raw}", path.display(), n + 1))?;
        map.insert(k.trim().to_string(), strip_quotes(v.trim()).to_string());
    }
    Ok(map)
}

/// A bare `fastboot`/`adb` value on Windows means the .exe.
#[cfg(windows)]
fn with_exe(mut p: PathBuf) -> PathBuf {
    if p.extension().is_none() {
        p.set_extension("exe");
    }
    p
}
#[cfg(not(windows))]
fn with_exe(p: PathBuf) -> PathBuf {
    p
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn find_export(cli_export: &str) -> Result<PathBuf, String> {
    if !cli_export.is_empty() {
        let p = PathBuf::from(cli_export);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!("--export file not found: {cli_export}"));
    }
    for cand in [exe_dir().join("export.txt"), PathBuf::from("export.txt")] {
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err("no export.txt (put it next to the binary or run from its directory; --export FILE overrides)".to_string())
}

fn resolve_config(export_path: &Path) -> Result<Config, String> {
    let map = parse_export(export_path)?;
    let base = export_path.parent().map(|d| d.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    let get = |k: &str, dflt: &str| map.get(k).cloned().unwrap_or_else(|| dflt.to_string());
    let rel = |v: &str| {
        let p = PathBuf::from(v);
        if p.is_absolute() { p } else { base.join(p) }
    };
    let recovery_img = rel(&get("RECOVERY_IMG", "OrangeFox-R12.0-test5-aio.ramdisk.lz4"));
    let backup_dir = rel(&get("BACKUP_DIR", "backup"));
    let log_dir = rel(&get("LOG_DIR", "logs"));
    let pt_key = if cfg!(windows) { "PLATFORM_TOOLS_WINDOWS" } else { "PLATFORM_TOOLS_LINUX" };
    let pt_dflt = if cfg!(windows) { "platform-tools-windows" } else { "platform-tools-linux" };
    let pt = rel(&get(pt_key, pt_dflt));
    let fastboot = with_exe(pt.join(get("FASTBOOT_BIN", "fastboot")));
    let adb = with_exe(pt.join(get("ADB_BIN", "adb")));
    if !recovery_img.is_file() {
        return Err(format!("missing recovery payload: {}", recovery_img.display()));
    }
    if !fastboot.is_file() {
        return Err(format!("missing fastboot binary: {}", fastboot.display()));
    }
    if !adb.is_file() {
        return Err(format!("missing adb binary: {}", adb.display()));
    }
    let min_free_mb: u64 = match map.get("MIN_FREE_MB") {
        None => 7,
        Some(v) => v.parse().map_err(|_| format!("bad MIN_FREE_MB in export.txt: '{v}' (want MiB integer)"))?,
    };
    Ok(Config { recovery_img, backup_dir, log_dir, tools: Tools { fastboot, adb }, min_free: min_free_mb * 1024 * 1024, min_free_mb })
}

// ------------------------------------------------------------ session ---

/// Launcher name for the --force hint (rerun goes through it).
#[cfg(windows)]
const LAUNCHER: &str = "install.bat";
#[cfg(not(windows))]
const LAUNCHER: &str = "./install.sh";

const EXIT_ITEM: &str = "Exit";
const BACK_ITEM: &str = "Back";

struct Ctx {
    cfg: Config,
    log: File,
    log_path: PathBuf,
    stamp: String,
    backup: PathBuf,
    /// Fresh-backup folder name (default: the run stamp; restore flow
    /// may set a custom one). The dir is only created when `fresh`.
    backup_name: String,
    /// Take a fresh stock backup before flashing (install: always;
    /// restore: asked, --force assumes yes).
    fresh: bool,
    pass: u32,
    fail: u32,
    policy_fail: bool,
    force: bool,
    serial: String,
    mode: String,
    layout: String,
    /// How the device is reached: classic fastboot (bootloader) or
    /// rooted adb (system/recovery shell, with or without `su`).
    transport: Transport,
    slots: Vec<String>,
    restore_src: Option<PathBuf>,
    work: PathBuf,
}

/// How the installer talks to the device. Fastboot is the classic
/// bootloader path; Adb is the rooted-shell path (system or recovery
/// adb, `su` when the shell itself is not root).
#[derive(Clone, Copy, PartialEq)]
enum Transport {
    Fastboot,
    Adb { recovery: bool, su: bool },
}

/// True on the rooted-adb transport (block access via `dd`, files via
/// push/pull — no fastboot getvar/fetch/flash).
fn via_adb(ctx: &Ctx) -> bool {
    matches!(ctx.transport, Transport::Adb { .. })
}

/// Scratch dir on the device for the adb transport: block dumps land
/// here before `pull`, staged images before the `dd` write.
const DEV_TMP: &str = "/data/local/ofox_installer";

/// Root probe over adb: try reading the first bytes of vendor_boot_a.
/// Direct shell first (adbd root, typical in recovery); on refusal a
/// `su -c` attempt (system with Magisk/KernelSU — grant on the device
/// if it asks). Returns (have_root, needs_su).
fn probe_adb_root(ctx: &mut Ctx) -> (bool, bool) {
    const PROBE: &str = "dd if=/dev/block/by-name/vendor_boot_a of=/dev/null bs=8 count=1";
    fn check(out: &std::process::Output) -> bool {
        if !out.status.success() {
            return false;
        }
        let blob = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        let low = blob.to_lowercase();
        !(low.contains("denied") || low.contains("permission") || low.contains("not found") || low.contains("no such"))
    }
    match fb::adb_shell(&ctx.cfg.tools.adb, &ctx.serial, false, PROBE, &mut ctx.log) {
        Ok(o) if check(&o) => {
            ctx.say("  • root: yes (adb shell)");
            return (true, false);
        }
        Ok(_) => {}
        Err(_) => {}
    }
    ctx.say("  • shell has no block access, trying su (grant on the device if asked)...");
    match fb::adb_shell(&ctx.cfg.tools.adb, &ctx.serial, true, PROBE, &mut ctx.log) {
        Ok(o) if check(&o) => {
            ctx.say("  • root: yes (via su)");
            (true, true)
        }
        _ => {
            ctx.say("  • root: no (block read refused)");
            (false, false)
        }
    }
}
/// Light terminal background? Auto from $COLORFGBG (last field is the
/// bg color: "0;15" is light, "15;0" is dark), manual override with
/// BOOTSMASHER_THEME=light|dark. Dark is the default (dev terminals,
/// Windows console).
fn light_bg() -> bool {
    match std::env::var("BOOTSMASHER_THEME").ok().as_deref() {
        Some("light") => return true,
        Some("dark") => return false,
        _ => {}
    }
    if let Ok(v) = std::env::var("COLORFGBG") {
        if let Some(bg) = v.rsplit(';').next().and_then(|b| b.trim().parse::<u8>().ok()) {
            return matches!(bg, 7 | 11 | 12 | 13 | 14 | 15);
        }
    }
    false
}

/// Palette markers (plain text when the console does no colors:
/// piped output, NO_COLOR). Yellow/cyan die on a white background,
/// so the light theme swaps them for magenta/blue.
fn tick() -> String {
    format!("{}", style("✔").green().bold())
}
fn cross() -> String {
    format!("{}", style("✘").red().bold())
}
fn dot() -> String {
    if light_bg() {
        format!("{}", style("•").magenta().bold())
    } else {
        format!("{}", style("•").yellow().bold())
    }
}
fn rule(what: &str) -> String {
    let s = format!("── {what} ─────────────────────");
    if light_bg() {
        format!("{}", style(s).blue().bold())
    } else {
        format!("{}", style(s).cyan().bold())
    }
}

impl Ctx {
    fn say(&mut self, line: &str) {
        // Console gets the painted line, the log file the plain one
        // (ANSI stripped) so install.log stays greppable.
        println!("{}", paint(line));
        let _ = writeln!(self.log, "{}", console::strip_ansi_codes(line));
        let _ = self.log.flush();
    }
    fn ok(&mut self, what: &str) {
        self.pass += 1;
        self.say(&format!("  {} {what}", tick()));
    }
    fn bad(&mut self, what: &str) {
        self.fail += 1;
        self.say(&format!("  {} FAIL: {what}", cross()));
    }
    fn step(&mut self, what: &str) {
        self.say("");
        self.say(&rule(what));
    }
}

/// Paints one console line: the `•` bullet marker turns yellow+bold.
/// Everything else passes through untouched (ticks/crosses/headers are
/// styled by their callers). The log file never sees this — `say`
/// strips ANSI before writing.
fn paint(line: &str) -> String {
    line.replacen("  • ", &format!("  {} ", dot()), 1)
}

/// Remove the work dir on the way out (best effort).
struct WorkGuard(PathBuf);
impl Drop for WorkGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1048576.0)
}

/// Install menu theme: the active row is a tight light bar (just the
/// text plus a space of padding on each side — black on white for
/// dark terminals, inverted for light ones), everything else is
/// `ColorfulTheme`. No bold on the bar: bold black renders as grey
/// on some terminals.
struct BarTheme {
    inner: dialoguer::theme::ColorfulTheme,
}

impl Default for BarTheme {
    fn default() -> Self {
        let mut inner = dialoguer::theme::ColorfulTheme::default();
        if light_bg() {
            // Yellow `?` dies on a white background.
            inner.prompt_prefix = style("?".to_string()).for_stderr().blue();
        }
        Self { inner }
    }
}

impl dialoguer::theme::Theme for BarTheme {
    fn format_prompt(&self, f: &mut dyn fmt::Write, prompt: &str) -> fmt::Result {
        self.inner.format_prompt(f, prompt)
    }
    fn format_error(&self, f: &mut dyn fmt::Write, err: &str) -> fmt::Result {
        self.inner.format_error(f, err)
    }
    fn format_input_prompt(&self, f: &mut dyn fmt::Write, prompt: &str, default: Option<&str>) -> fmt::Result {
        self.inner.format_input_prompt(f, prompt, default)
    }
    fn format_confirm_prompt(&self, f: &mut dyn fmt::Write, prompt: &str, default: Option<bool>) -> fmt::Result {
        self.inner.format_confirm_prompt(f, prompt, default)
    }
    fn format_confirm_prompt_selection(&self, f: &mut dyn fmt::Write, prompt: &str, sel: Option<bool>) -> fmt::Result {
        self.inner.format_confirm_prompt_selection(f, prompt, sel)
    }
    fn format_input_prompt_selection(&self, f: &mut dyn fmt::Write, prompt: &str, sel: &str) -> fmt::Result {
        self.inner.format_input_prompt_selection(f, prompt, sel)
    }
    fn format_multi_select_prompt_selection(&self, f: &mut dyn fmt::Write, prompt: &str, sel: &[&str]) -> fmt::Result {
        self.inner.format_multi_select_prompt_selection(f, prompt, sel)
    }
    fn format_select_prompt_item(&self, f: &mut dyn fmt::Write, text: &str, active: bool) -> fmt::Result {
        if !active {
            return self.inner.format_select_prompt_item(f, text, false);
        }
        // Tight bar: one space past each edge of the text, nothing more.
        let bar = format!(" ❯ {text}  ");
        if light_bg() {
            write!(f, "{}", style(bar).for_stderr().white().on_black())
        } else {
            write!(f, "{}", style(bar).for_stderr().black().on_white())
        }
    }
    fn format_multi_select_prompt_item(&self, f: &mut dyn fmt::Write, text: &str, checked: bool, active: bool) -> fmt::Result {
        self.inner.format_multi_select_prompt_item(f, text, checked, active)
    }
    fn format_sort_prompt_item(&self, f: &mut dyn fmt::Write, text: &str, picked: bool, active: bool) -> fmt::Result {
        self.inner.format_sort_prompt_item(f, text, picked, active)
    }
}

/// Arrow-key menu (BarTheme: active row is a full-width light bar,
/// same widget as `pick`).
/// Err(true) is user walk-away (Esc/q), Err(false) is an I/O error.
fn menu(prompt: &str, def_1based: usize, options: &[String]) -> Result<String, bool> {
    let theme = BarTheme::default();
    let mut sel = dialoguer::Select::with_theme(&theme).with_prompt(prompt).items(options);
    if def_1based > 0 && def_1based <= options.len() {
        sel = sel.default(def_1based - 1);
    }
    match sel.interact_opt() {
        Ok(Some(i)) => Ok(options[i].clone()),
        Ok(None) => Err(true),
        Err(e) if e.to_string().contains("interrupted") => Err(true),
        Err(_) => Err(false),
    }
}

/// A backup folder name is one path segment: no slashes, no dots,
/// no control characters, not empty.
fn clean_backup_name(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() || t == "." || t == ".." {
        return None;
    }
    if t.contains('/') || t.contains('\\') || t.contains('\0') {
        return None;
    }
    if t.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(t.to_string())
}

fn dir_has_files(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut r) => r.next().is_some(),
        Err(_) => false,
    }
}

/// Custom backup folder prompt (restore flow). Empty input falls back
/// to the run stamp. `None` = user walked away (already announced).
fn ask_backup_name(ctx: &mut Ctx) -> Option<(String, PathBuf)> {
    let theme = BarTheme::default();
    loop {
        let raw: String = match dialoguer::Input::with_theme(&theme)
            .with_prompt("Backup folder name (empty = run stamp)")
            .interact_text()
        {
            Ok(a) => a,
            Err(_) => {
                ctx.say("  aborted by user, nothing flashed");
                return None;
            }
        };
        let want = if raw.trim().is_empty() { ctx.stamp.clone() } else { raw.trim().to_string() };
        match clean_backup_name(&want) {
            Some(n) => {
                let dir = ctx.cfg.backup_dir.join(&n);
                if dir_has_files(&dir) {
                    ctx.say(&format!("  backup/{n} already holds files — pick another name"));
                    continue;
                }
                return Some((n, dir));
            }
            None => ctx.say("  bad name (one folder, no slashes) — try again"),
        }
    }
}

fn read_pause() {
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// Menu display for a serial: `serial (label)` when the label is
/// known, bare serial otherwise.
fn tag(serial: &str, label: Option<String>) -> String {
    match label {
        Some(l) => format!("{serial} ({l})"),
        None => serial.to_string(),
    }
}

/// Sets ops::QUIET while alive: the rebuild diagnostics stay out of
/// the installer console (the verdict still lands in the run log).
struct QuietGuard;
impl QuietGuard {
    fn on() -> Self {
        ops::QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
        Self
    }
}
impl Drop for QuietGuard {
    fn drop(&mut self) {
        ops::QUIET.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// UTC stamp YYYYMMDD-HHMMSS (days-to-civil, Hinnant's algorithm).
fn stamp_utc() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86400) as i64;
    let sod = (secs % 86400) as i64;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", year, m, d, sod / 3600, (sod % 3600) / 60, sod % 60)
}

/// Backup dirs holding vendor_boot images, newest stamp first.
/// Each entry: (stamp, "a" | "b" | "a, b").
fn list_backups(backup_dir: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let rd = match std::fs::read_dir(backup_dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if !p.is_dir() {
            continue;
        }
        let stamp = match p.file_name().and_then(|n| n.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let mut slots = Vec::new();
        if p.join("vendor_boot_a.img").is_file() {
            slots.push("a");
        }
        if p.join("vendor_boot_b.img").is_file() {
            slots.push("b");
        }
        if !slots.is_empty() {
            out.push((stamp, slots.join(", ")));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out
}

// --------------------------------------------------------------- vboot ---

/// Same self-consistency rule as `vboot --verify` (stale table, table
/// sum, offsets chain, every fragment valid), without console output:
/// the verdict text goes to the run log.
fn verdict_text(img: &[u8], label: &str) -> Result<String, String> {
    let a = ops::analyze(img).map_err(|e| format!("INVALID: {e}"))?;
    let broken = a.stale_table
        || a.table_sum != a.header_ramdisk_size as u64
        || !a.offsets_chain_ok
        || a.frags.iter().any(|f| !f.valid);
    let mut t = format!(
        "{} {label}: header v{}, page {}, ramdisk {} (table sum {}), {} fragment(s), dtb {} FDT(s), bootconfig {} bytes{}",
        if broken { "INVALID" } else { "OK" },
        a.header_version,
        a.page_size,
        a.header_ramdisk_size,
        a.table_sum,
        a.frags.len(),
        a.dtb_fdts,
        a.bootconfig_len,
        if a.stale_table { " [STALE TABLE, single stream]" } else { "" },
    );
    for f in &a.frags {
        t.push_str(&format!(
            "\n  frag {} name={:?} type={} size={} offset={} fmt={} valid={} {}",
            f.index,
            f.name,
            type_name(f.etype),
            f.size,
            f.offset,
            f.kind.name(),
            if f.valid { "yes" } else { "NO" },
            f.detail,
        ));
    }
    if broken {
        return Err(t);
    }
    Ok(t)
}

/// Decompressed cpio bytes inside the image (what `unpack` sums into
/// vendor_ramdisk/*.cpio; raw blob bytes when undecodable).
fn cpio_payload(img: &[u8]) -> Result<u64, String> {
    let im = ops::Image::load(img).map_err(|e| format!("cannot parse image: {e}"))?;
    let mut sum: u64 = 0;
    for i in 0..im.table.len() {
        let blob = im.frag_bytes(i).map_err(|e| format!("frag {i}: {e}"))?;
        // Raw passes through; undecodable blobs count as-is.
        match codec::decompress(codec::sniff(blob), blob).ok() {
            Some(d) if cpio::parse(&d).is_ok() => sum += d.len() as u64,
            _ => sum += blob.len() as u64,
        }
    }
    Ok(sum)
}

// ---------------------------------------------------------------- stages ---

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Mode,
    Layout,
    Slot,
    Backup,
    Fresh,
    BackupName,
    Pacing,
}

struct StagePlan {
    mode_menu: bool,
    layout_menu: bool,
    slot_menu: bool,
    backup_menu: bool,
    fresh_menu: bool,
}

fn build_stages(plan: &StagePlan, ctx: &Ctx) -> Vec<Stage> {
    let mut v = Vec::new();
    if plan.mode_menu {
        v.push(Stage::Mode);
    }
    if ctx.mode == "install" && plan.layout_menu {
        v.push(Stage::Layout);
    }
    if plan.slot_menu {
        v.push(Stage::Slot);
    }
    if ctx.mode == "restore" && plan.backup_menu {
        v.push(Stage::Backup);
    }
    if ctx.mode == "restore" && plan.fresh_menu {
        v.push(Stage::Fresh);
    }
    if ctx.mode == "restore" && plan.fresh_menu && ctx.fresh {
        v.push(Stage::BackupName);
    }
    if !ctx.force {
        v.push(Stage::Pacing);
    }
    v
}

/// Interactive selections with back-navigation. False = user abort
/// (already announced); true leaves mode/slots/restore set.
fn run_selections(ctx: &mut Ctx, plan: &StagePlan, backups: &[(String, String)], mut idx: usize) -> bool {
    let mut stages = build_stages(plan, ctx);
    if !stages.is_empty() {
        ctx.step("[2/5] Plan");
    }
    while idx < stages.len() {
        match stages[idx] {
            Stage::Mode => {
                let opts = vec!["Install OrangeFox".to_string(), "Restore backup".to_string(), EXIT_ITEM.to_string()];
                match menu("Action", 1, &opts) {
                    Ok(c) if c == "Install OrangeFox" => {
                        ctx.mode = "install".to_string();
                        ctx.restore_src = None;
                    }
                    Ok(c) if c == "Restore backup" => ctx.mode = "restore".to_string(),
                    _ => {
                        ctx.say("  aborted by user, nothing flashed");
                        return false;
                    }
                }
                ctx.say(&format!("  • Action: {}", if ctx.mode == "install" { "Install OrangeFox" } else { "Restore backup" }));
                stages = build_stages(plan, ctx);
                idx = 1; // mode is always stages[0] when asked
            }
            Stage::Layout => {
                let mut opts = vec![
                    "Type A — classic (recovery fragment)".to_string(),
                    "Type B — all-in-platform (var1)".to_string(),
                    "Type C — payload-only platform (var2)".to_string(),
                ];
                if idx > 0 {
                    opts.push(BACK_ITEM.to_string());
                }
                opts.push(EXIT_ITEM.to_string());
                ctx.say("  Type A: payload -> recovery fragment, native first-stage wins (stock behavior).");
                ctx.say("  Type B: native first-stage + payload merged into platform, no recovery fragment.");
                ctx.say("  Type C: native platform fully replaced by the payload (needs a var2-AIO payload");
                ctx.say("          built with first_stage inside); modules split to dlkm when present.");
                match menu("Install layout:", 1, &opts).as_deref() {
                    Ok("Type A — classic (recovery fragment)") => ctx.layout = String::new(),
                    Ok("Type B — all-in-platform (var1)") => ctx.layout = "var1".to_string(),
                    Ok("Type C — payload-only platform (var2)") => ctx.layout = "var2".to_string(),
                    Ok(c) if c == BACK_ITEM => {
                        idx -= 1;
                        continue;
                    }
                    _ => {
                        ctx.say("  aborted by user, nothing flashed");
                        return false;
                    }
                }
                ctx.say(&format!("  layout: {}", layout_name(&ctx.layout)));
                idx += 1;
            }
            Stage::Slot => {
                let def = match ctx.slots.iter().map(|s| s.as_str()).collect::<Vec<_>>().as_slice() {
                    ["a"] => 1,
                    ["b"] => 2,
                    _ => 3,
                };
                let mut opts = vec!["Slot a only".to_string(), "Slot b only".to_string(), "Both (a+b)".to_string()];
                if idx > 0 {
                    opts.push(BACK_ITEM.to_string());
                }
                opts.push(EXIT_ITEM.to_string());
                match menu("Slots", def, &opts).as_deref() {
                    Ok("Slot a only") => ctx.slots = vec!["a".to_string()],
                    Ok("Slot b only") => ctx.slots = vec!["b".to_string()],
                    Ok("Both (a+b)") => ctx.slots = vec!["a".to_string(), "b".to_string()],
                    Ok(c) if c == BACK_ITEM => {
                        idx -= 1;
                        continue;
                    }
                    _ => {
                        ctx.say("  aborted by user, nothing flashed");
                        return false;
                    }
                }
                ctx.say(&format!("  • Slots: {}", ctx.slots.join(" + ")));
                idx += 1;
            }
            Stage::Backup => {
                let labels: Vec<String> =
                    backups.iter().map(|(s, sl)| format!("{s} ({sl})")).collect();
                let prev = ctx.restore_src.as_ref().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("");
                let mut def = 1;
                for (i, (s, _)) in backups.iter().enumerate() {
                    if s == prev {
                        def = i + 1;
                        break;
                    }
                }
                let mut opts: Vec<String> = labels
                    .iter()
                    .enumerate()
                    .map(|(i, l)| if i == 0 { format!("latest: {l}") } else { l.clone() })
                    .collect();
                if idx > 0 {
                    opts.push(BACK_ITEM.to_string());
                }
                opts.push(EXIT_ITEM.to_string());
                match menu("Backup", def, &opts) {
                    Ok(c) if c == BACK_ITEM => {
                        idx -= 1;
                        continue;
                    }
                    Ok(c) if c == EXIT_ITEM => {
                        ctx.say("  aborted by user, nothing flashed");
                        return false;
                    }
                    Ok(c) => {
                        let stamp = c.trim_start_matches("latest: ").split(" (").next().unwrap_or("");
                        ctx.restore_src = Some(ctx.cfg.backup_dir.join(stamp));
                    }
                    Err(_) => {
                        ctx.say("  aborted by user, nothing flashed");
                        return false;
                    }
                }
                let shown = ctx.restore_src.as_ref().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("?");
                ctx.say(&format!("  • Backup: backup/{shown}"));
                idx += 1;
            }
            Stage::Fresh => {
                let mut opts = vec![
                    "Yes, back up current stock first".to_string(),
                    "No, flash without a fresh backup".to_string(),
                ];
                if idx > 0 {
                    opts.push(BACK_ITEM.to_string());
                }
                opts.push(EXIT_ITEM.to_string());
                match menu("Fresh backup", 1, &opts).as_deref() {
                    Ok("Yes, back up current stock first") => ctx.fresh = true,
                    Ok("No, flash without a fresh backup") => ctx.fresh = false,
                    Ok(c) if c == BACK_ITEM => {
                        idx -= 1;
                        continue;
                    }
                    _ => {
                        ctx.say("  aborted by user, nothing flashed");
                        return false;
                    }
                }
                ctx.say(&format!("  • Fresh backup: {}", if ctx.fresh { "yes" } else { "no (your choice, no safety copy)" }));
                // The name stage exists only when fresh == yes.
                stages = build_stages(plan, ctx);
                idx += 1;
            }
            Stage::BackupName => {
                let use_label = format!("Use {}", ctx.stamp);
                let mut opts = vec![use_label.clone(), "Custom name...".to_string()];
                if idx > 0 {
                    opts.push(BACK_ITEM.to_string());
                }
                opts.push(EXIT_ITEM.to_string());
                match menu("Backup folder", 1, &opts).as_deref() {
                    Ok(c) if c == use_label => {
                        ctx.backup_name = ctx.stamp.clone();
                        ctx.backup = ctx.cfg.backup_dir.join(&ctx.backup_name);
                    }
                    Ok(c) if c == "Custom name..." => match ask_backup_name(ctx) {
                        Some((name, dir)) => {
                            ctx.backup_name = name;
                            ctx.backup = dir;
                        }
                        None => return false, // aborted inside
                    },
                    Ok(c) if c == BACK_ITEM => {
                        idx -= 1;
                        continue;
                    }
                    _ => {
                        ctx.say("  aborted by user, nothing flashed");
                        return false;
                    }
                }
                // The default stamp may already exist (two runs in one
                // second); a non-empty dir is never reused silently.
                if dir_has_files(&ctx.backup) {
                    ctx.say(&format!("  backup/{} already holds files — pick another name", ctx.backup_name));
                    continue; // re-ask, same stage
                }
                ctx.say(&format!("  • Backup folder: backup/{}", ctx.backup_name));
                idx += 1;
            }
            Stage::Pacing => {
                let mut opts = vec!["Continue".to_string()];
                if idx > 0 {
                    opts.push(BACK_ITEM.to_string());
                }
                opts.push(EXIT_ITEM.to_string());
                match menu("Ready to proceed", 1, &opts).as_deref() {
                    Ok("Continue") => idx += 1,
                    Ok(c) if c == BACK_ITEM => idx -= 1,
                    _ => {
                        ctx.say("  aborted by user, nothing flashed");
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// One reachable endpoint: classic fastboot, or adb in system or
/// recovery mode (`adb devices` state travels along so the branch is
/// explicit even with several devices attached).
#[derive(Clone)]
enum Endpoint {
    Fb(String),
    Adb(String, String),
}

/// [1/5] device outcome: Ok = transport ready, Abort = user walked
/// away (exit 0, nothing flashed), Fail = no device / reboot failed
/// (exit 1). Matches the reference script where abort_user always
/// exits 0.
#[derive(PartialEq)]
enum DevStage {
    Ok,
    Abort,
    Fail,
}

/// Menu label for an endpoint: serial + human model + mode, so two
/// adb devices or an adb+bootloader pair can never be confused.
fn endpoint_label(ctx: &mut Ctx, ep: &Endpoint) -> String {
    match ep {
        Endpoint::Fb(s) => {
            let label = fb::getvar(&ctx.cfg.tools.fastboot, s, "product", &mut ctx.log);
            format!("{} (fastboot)", tag(s, label))
        }
        Endpoint::Adb(s, st) => {
            let mode = if st == "recovery" { "adb recovery" } else { "adb system" };
            let label = fb::adb_label(&ctx.cfg.tools.adb, s, &mut ctx.log);
            format!("{} ({mode})", tag(s, label))
        }
    }
}

/// Product + active slot line for the picked transport, then the log
/// path. Fastboot reads getvar; adb reads getprop (slot suffix `_a`
/// becomes `a`).
fn say_device_info(ctx: &mut Ctx) {
    match ctx.transport {
        Transport::Fastboot => {
            let product = fb::getvar(&ctx.cfg.tools.fastboot, &ctx.serial, "product", &mut ctx.log).unwrap_or_else(|| "?".to_string());
            let curslot = fb::getvar(&ctx.cfg.tools.fastboot, &ctx.serial, "current-slot", &mut ctx.log).unwrap_or_else(|| "?".to_string());
            ctx.say(&format!("  • product {product} · active slot {curslot}"));
        }
        Transport::Adb { .. } => {
            let product = fb::adb_prop(&ctx.cfg.tools.adb, &ctx.serial, "ro.product.device", &mut ctx.log).unwrap_or_else(|| "?".to_string());
            let raw = fb::adb_prop(&ctx.cfg.tools.adb, &ctx.serial, "ro.boot.slot_suffix", &mut ctx.log).unwrap_or_default();
            let slot = raw.trim_start_matches('_');
            let slot = if slot.is_empty() { "?" } else { slot };
            let where_ = match ctx.transport {
                Transport::Adb { recovery: true, .. } => "adb recovery",
                _ => "adb system",
            };
            ctx.say(&format!("  • product {product} · active slot {slot} (via {where_})"));
        }
    }
    ctx.say(&format!("  • log: {}", short_path(&ctx.log_path)));
}

/// "Ready to reboot to bootloader?" gate. Ok = reboot now (or the
/// caller runs unattended under --force). Err(true) = Back, Err(false)
/// = walk away.
fn ask_reboot_ready(ctx: &mut Ctx, can_back: bool) -> Result<(), bool> {
    if ctx.force {
        return Ok(());
    }
    let mut opts = vec!["Yes, reboot now".to_string()];
    if can_back {
        opts.push(BACK_ITEM.to_string());
    }
    opts.push(EXIT_ITEM.to_string());
    match menu("Ready to reboot to bootloader", 1, &opts).as_deref() {
        Ok("Yes, reboot now") => Ok(()),
        Ok(c) if c == BACK_ITEM => Err(true),
        _ => Err(false),
    }
}

/// Reboot the adb-reachable serial to the bootloader and wait for it.
/// True = in the bootloader now (transport already flipped).
fn adb_to_bootloader(ctx: &mut Ctx) -> bool {
    ctx.say(&format!("  • {} rebooting to bootloader...", ctx.serial));
    fb::adb_reboot_bootloader(&ctx.cfg.tools.adb, &ctx.serial, &mut ctx.log);
    match fb::wait_bootloader(&ctx.cfg.tools.fastboot, 120, &mut ctx.log) {
        Some(s) => {
            ctx.serial = s;
            ctx.transport = Transport::Fastboot;
            ctx.say(&format!("  • {} — bootloader", ctx.serial));
            true
        }
        None => {
            ctx.say("  device did not come back to bootloader");
            false
        }
    }
}

/// [1/5] device: pick one endpoint when several are attached (every
/// entry carries its mode, so adb+adb or adb+bootloader pairs stay
/// unambiguous), probe adb root, then pick the transport:
/// bootloader/fastboot or rooted adb. A bootloader choice is always
/// followed by a reboot-readiness question; without root the only
/// path is the bootloader.
fn stage_device(ctx: &mut Ctx, cli_transport: &str) -> DevStage {
    ctx.step("[1/5] Device");
    let fb_devs = fb::serials(&ctx.cfg.tools.fastboot, "fastboot", &mut ctx.log);
    // Recovery adb (`adb devices` state `recovery`) counts exactly
    // like system adb (`device`); anything else (unauthorized,
    // sideload, ...) is listed for awareness but never auto-picked.
    let mut adb_all = fb::adb_states(&ctx.cfg.tools.adb, &mut ctx.log);
    adb_all.retain(|(_, st)| st == "device" || st == "recovery");
    let mut endpoints: Vec<Endpoint> = Vec::new();
    for s in &fb_devs {
        endpoints.push(Endpoint::Fb(s.clone()));
    }
    for (s, st) in &adb_all {
        endpoints.push(Endpoint::Adb(s.clone(), st.clone()));
    }
    // One endpoint: take it silently. Several: every entry shows its
    // mode, and the choice fixes the installer branch right here.
    let mut picked: Option<Endpoint> = None;
    if endpoints.is_empty() {
        if ctx.force {
            ctx.say("  no device in fastboot or adb mode");
            return DevStage::Fail;
        }
        ctx.say("  no device detected.");
        ctx.say("  Reboot it to the bootloader manually (VolDown+Power, or: adb reboot bootloader),");
        let opts = vec!["Ready, check again".to_string(), EXIT_ITEM.to_string()];
        match menu("Device ready?", 1, &opts).as_deref() {
            Ok(EXIT_ITEM) | Err(_) => {
                ctx.say("  aborted by user, nothing flashed");
                return DevStage::Abort;
            }
            _ => {}
        }
        match fb::wait_bootloader(&ctx.cfg.tools.fastboot, 15, &mut ctx.log) {
            Some(s) => picked = Some(Endpoint::Fb(s)),
            None => {
                ctx.say("  Still nothing. Press Enter to exit and retry.");
                read_pause();
                ctx.say("  aborted");
                return DevStage::Abort;
            }
        }
    } else if endpoints.len() == 1 {
        picked = Some(endpoints.remove(0));
    }
    // Device-pick loop: Back from any adb sub-question returns here
    // (only reachable when several endpoints were listed).
    let multi = endpoints.len() > 1;
    loop {
        let ep = match picked.take() {
            Some(e) => e,
            None => {
                ctx.say("  attached endpoints (mode decides the installer branch):");
                let mut opts: Vec<String> = Vec::new();
                let mut labels: Vec<String> = Vec::new();
                for e in &endpoints {
                    let l = endpoint_label(ctx, e);
                    labels.push(l.clone());
                    opts.push(l);
                }
                opts.push(EXIT_ITEM.to_string());
                match menu("Device", 1, &opts) {
                    Ok(c) if c != EXIT_ITEM => match labels.iter().position(|l| *l == c) {
                        Some(i) => endpoints[i].clone(),
                        None => {
                            ctx.say("  aborted by user, nothing flashed");
                            return DevStage::Abort;
                        }
                    },
                    _ => {
                        ctx.say("  aborted by user, nothing flashed");
                        return DevStage::Abort;
                    }
                }
            }
        };
        match adb_branch(ctx, &ep, cli_transport, multi) {
            AdbNext::Done => {
                let _ = writeln!(ctx.log, "device={}", ctx.serial);
                let _ = writeln!(
                    ctx.log,
                    "transport={}",
                    match ctx.transport {
                        Transport::Fastboot => "fastboot".to_string(),
                        Transport::Adb { recovery, su } => format!("adb-{} su={}", if recovery { "recovery" } else { "system" }, su as u8),
                    }
                );
                say_device_info(ctx);
                return DevStage::Ok;
            }
            AdbNext::Repick => continue,
            AdbNext::Abort => {
                ctx.say("  aborted by user, nothing flashed");
                return DevStage::Abort;
            }
            AdbNext::Fail => return DevStage::Fail,
        }
    }
}

/// Outcome of one endpoint branch inside the device-pick loop.
enum AdbNext {
    Done,
    Repick,
    Abort,
    Fail,
}

/// "Ready to reboot to bootloader?" gate + the reboot itself.
/// Done = in the bootloader now; Repick/Abort bubble up; Fail = the
/// device never came back.
fn reboot_gate(ctx: &mut Ctx, multi: bool) -> AdbNext {
    match ask_reboot_ready(ctx, multi) {
        Ok(()) => {
            if !adb_to_bootloader(ctx) {
                return AdbNext::Fail;
            }
            AdbNext::Done
        }
        Err(true) => AdbNext::Repick,
        _ => AdbNext::Abort,
    }
}

/// One endpoint: fastboot straight through, adb via root probe +
/// transport choice. `multi` allows Back to the device list.
fn adb_branch(ctx: &mut Ctx, ep: &Endpoint, cli_transport: &str, multi: bool) -> AdbNext {
    match ep {
        Endpoint::Fb(serial) => {
            let serial = serial.clone();
            if fb::is_userspace(&ctx.cfg.tools.fastboot, &serial, &mut ctx.log) {
                ctx.say(&format!("  • {serial} is in fastbootd, rebooting to bootloader..."));
                fb::reboot_bootloader(&ctx.cfg.tools.fastboot, &serial, &mut ctx.log);
                match fb::wait_bootloader(&ctx.cfg.tools.fastboot, 90, &mut ctx.log) {
                    Some(s) => {
                        ctx.serial = s;
                        ctx.transport = Transport::Fastboot;
                        ctx.say(&format!("  • {} — bootloader", ctx.serial));
                    }
                    None => {
                        ctx.say("  device did not come back to bootloader");
                        return AdbNext::Fail;
                    }
                }
            } else {
                ctx.serial = serial.clone();
                ctx.transport = Transport::Fastboot;
                ctx.say(&format!("  • {serial} — bootloader"));
            }
            AdbNext::Done
        }
        Endpoint::Adb(serial, state) => {
            let serial = serial.clone();
            let recovery = state == "recovery";
            ctx.serial = serial.clone();
            let label = fb::adb_label(&ctx.cfg.tools.adb, &serial, &mut ctx.log);
            ctx.say(&format!("  • {} via adb {}", tag(&serial, label), if recovery { "(recovery)" } else { "(system)" }));
            // --force stays on the legacy rails unless --transport adb
            // pins the rooted path (no questions, no su popups).
            if ctx.force && cli_transport != "adb" {
                if !adb_to_bootloader(ctx) {
                    return AdbNext::Fail;
                }
                return AdbNext::Done;
            }
            let (root, su) = probe_adb_root(ctx);
            if cli_transport == "adb" {
                if !root {
                    ctx.say("  --transport adb needs root, and this device refused the block read");
                    return AdbNext::Fail;
                }
                ctx.transport = Transport::Adb { recovery, su };
                ctx.say(&format!("  • via adb root ({}; no reboot)", if recovery { "recovery" } else { "system" }));
                return AdbNext::Done;
            }
            if cli_transport == "fastboot" {
                return reboot_gate(ctx, multi);
            }
            if !root {
                ctx.say("  no root here — the only path is the bootloader.");
                return reboot_gate(ctx, multi);
            }
            // Rooted adb: let the user choose the installer branch.
            loop {
                let mut opts = vec![
                    "via bootloader (fastboot, recommended)".to_string(),
                    "via adb root (no reboot to bootloader)".to_string(),
                ];
                if multi {
                    opts.push(BACK_ITEM.to_string());
                }
                opts.push(EXIT_ITEM.to_string());
                match menu("How to install", 1, &opts).as_deref() {
                    Ok("via adb root (no reboot to bootloader)") => {
                        ctx.transport = Transport::Adb { recovery, su };
                        ctx.say(&format!("  • via adb root ({}; no reboot)", if recovery { "recovery" } else { "system" }));
                        return AdbNext::Done;
                    }
                    Ok("via bootloader (fastboot, recommended)") => {
                        match reboot_gate(ctx, multi) {
                            AdbNext::Done => return AdbNext::Done,
                            AdbNext::Fail => return AdbNext::Fail,
                            AdbNext::Abort => return AdbNext::Abort,
                            AdbNext::Repick => {
                                if multi {
                                    return AdbNext::Repick;
                                }
                                continue;
                            }
                        }
                    }
                    Ok(c) if c == BACK_ITEM => return AdbNext::Repick,
                    _ => return AdbNext::Abort,
                }
            }
        }
    }
}

/// Fresh-backup location for the final report.
fn stock_desc(ctx: &Ctx) -> String {
    if ctx.fresh {
        format!("backup/{}/", ctx.backup_name)
    } else {
        "none (fresh backup skipped)".to_string()
    }
}

/// Short display path: relative to cwd when possible, full otherwise.
fn short_path(p: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(r) = p.strip_prefix(&cwd) {
            return r.display().to_string();
        }
    }
    p.display().to_string()
}

/// [3/5] fetch current stock into the fresh backup dir (created here,
/// so a skipped backup leaves no empty folder behind).
/// Fastboot transport: `fastboot fetch`. Adb-root transport: `dd`
/// block -> /data/local/ofox_installer/ on the device, then `pull`.
/// False = exit 2.
fn stage_fetch(ctx: &mut Ctx) -> bool {
    if !ctx.fresh {
        ctx.step("[3/5] Backup stock");
        ctx.say("  • Fresh backup skipped — flashing without a safety copy");
        return true;
    }
    ctx.step(&format!("[3/5] Backup stock → backup/{}/", ctx.backup_name));
    if let Err(e) = std::fs::create_dir_all(&ctx.backup) {
        ctx.say(&format!("  cannot create {}: {e}", ctx.backup.display()));
        return false;
    }
    if via_adb(ctx) {
        let su = match ctx.transport {
            Transport::Adb { su, .. } => su,
            _ => false,
        };
        if fb::adb_shell(&ctx.cfg.tools.adb, &ctx.serial, su, &format!("mkdir -p {DEV_TMP}"), &mut ctx.log).is_err() {
            ctx.say("  cannot create device scratch dir (see log)");
            return false;
        }
        for s in ctx.slots.clone() {
            let blk = format!("/dev/block/by-name/vendor_boot_{s}");
            let tmp = format!("{DEV_TMP}/vendor_boot_{s}.img");
            let out = ctx.backup.join(format!("vendor_boot_{s}.img"));
            let dd = fb::adb_shell(
                &ctx.cfg.tools.adb,
                &ctx.serial,
                su,
                &format!("dd if={blk} of={tmp} bs=1048576"),
                &mut ctx.log,
            );
            match dd {
                Ok(o) if o.status.success() => match fb::adb_pull(&ctx.cfg.tools.adb, &ctx.serial, &tmp, &out, &mut ctx.log) {
                    Ok(()) => {
                        let sz = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                        ctx.ok(&format!("vendor_boot_{s} saved ({})", mb(sz)));
                    }
                    Err(_) => ctx.bad(&format!("pull vendor_boot_{s} (see log)")),
                },
                _ => ctx.bad(&format!("dd vendor_boot_{s} on device (see log)")),
            }
        }
        // Device scratch copies are pulled already; drop them.
        let _ = fb::adb_shell(&ctx.cfg.tools.adb, &ctx.serial, su, &format!("rm -f {DEV_TMP}/vendor_boot_?.img"), &mut ctx.log);
        if ctx.fail > 0 {
            ctx.say("  fetch failed, nothing flashed");
            return false;
        }
        return true;
    }
    for s in ctx.slots.clone() {
        let part = format!("vendor_boot_{s}");
        let out = ctx.backup.join(format!("vendor_boot_{s}.img"));
        match fb::fetch(&ctx.cfg.tools.fastboot, &ctx.serial, &part, &out, &mut ctx.log) {
            Ok(()) => {
                let sz = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                ctx.ok(&format!("vendor_boot_{s} saved ({})", mb(sz)));
            }
            Err(_) => ctx.bad(&format!("fetch vendor_boot_{s} (see log)")),
        }
    }
    if ctx.fail > 0 {
        ctx.say("  fetch failed, nothing flashed");
        return false;
    }
    true
}

/// [4/5] prepare staged images + per-slot report. False = exit 2.
fn stage_prepare(ctx: &mut Ctx) -> bool {
    let _ = std::fs::remove_dir_all(&ctx.work);
    if let Err(e) = std::fs::create_dir_all(&ctx.work) {
        ctx.say(&format!("  cannot create work dir: {e}"));
        return false;
    }
    let src_desc: String;
    if ctx.mode == "install" {
        let fox_name = ctx.cfg.recovery_img.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        ctx.step(&format!("[4/5] Rebuild — {fox_name} (footer dropped)"));
        ctx.say("  footer dropped: grown ramdisk + old footer would overflow the partition");
        ctx.say(&format!("  • Layout: {}", layout_name(&ctx.layout)));
        let fox = match std::fs::read(&ctx.cfg.recovery_img) {
            Ok(b) => b,
            Err(e) => {
                ctx.say(&format!("  cannot read recovery payload: {e}"));
                return false;
            }
        };
        let _quiet = QuietGuard::on();
        for s in ctx.slots.clone() {
            let stock = match std::fs::read(ctx.backup.join(format!("vendor_boot_{s}.img"))) {
                Ok(b) => b,
                Err(e) => {
                    ctx.bad(&format!("read stock slot {s}: {e}"));
                    continue;
                }
            };
            let rip = match layout_variant(&ctx.layout) {
                Ok(v) => v,
                Err(e) => {
                    ctx.bad(&format!("bad layout: {e}"));
                    continue;
                }
            };
            // Layout was announced once above (same for every slot).
            let opts = RepackOpts { mode: Mode::Keep, drop: Vec::new(), sets: Vec::new(), recovery: Some((fox_name.clone(), fox.clone())), recovery_is_platform: rip, drop_footer: true };
            match ops::repack_with_opts(&stock, None, opts) {
                Ok(out) => {
                    let dst = ctx.work.join(format!("flash_{s}.img"));
                    match std::fs::write(&dst, &out) {
                        Ok(()) => ctx.ok(&format!("slot {s} rebuilt ({})", mb(out.len() as u64))),
                        Err(e) => ctx.bad(&format!("write slot {s}: {e}")),
                    }
                }
                Err(e) => ctx.bad(&format!("rebuild slot {s} (see log): {e}")),
            }
        }
        if ctx.fail > 0 {
            ctx.say("  rebuild failed, nothing flashed");
            return false;
        }
        src_desc = "rebuilt images".to_string();
    } else {
        let src = match ctx.restore_src.clone() {
            Some(p) => p,
            None => {
                ctx.say("  no backup selected");
                return false;
            }
        };
        let shown = src.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        // Restore is a plain flash: the picked backup images go to the
        // device byte-identical — no validity gates, no free-space
        // policy, no repack. The only refusal is a missing file
        // (there would be nothing to flash).
        ctx.step(&format!("[4/5] Stage restore — backup/{shown} (as-is, no checks)"));
        for s in ctx.slots.clone() {
            let from = src.join(format!("vendor_boot_{s}.img"));
            if !from.is_file() {
                ctx.bad(&format!("slot {s}: no vendor_boot_{s}.img in backup/{shown}"));
                continue;
            }
            let bytes = match std::fs::read(&from) {
                Ok(b) => b,
                Err(e) => {
                    ctx.bad(&format!("read slot {s}: {e}"));
                    continue;
                }
            };
            match std::fs::write(ctx.work.join(format!("flash_{s}.img")), &bytes) {
                Ok(()) => ctx.ok(&format!("slot {s} ready to flash ({})", mb(bytes.len() as u64))),
                Err(e) => ctx.bad(&format!("stage slot {s}: {e}")),
            }
        }
        if ctx.fail > 0 {
            ctx.say("  staging failed, nothing flashed");
            return false;
        }
        src_desc = format!("backup/{shown}");
    }
    if ctx.mode != "install" {
        // Restore flashes exactly what is stored: skip the install-only
        // validity + free-space report below entirely.
        ctx.say("  • no checks on restore — images go to the device as stored");
        return true;
    }
    ctx.say("");
        ctx.say(&format!("  Report ({src_desc}; details in install.log):"));
    for s in ctx.slots.clone() {
        let img_path = ctx.work.join(format!("flash_{s}.img"));
        let bytes = match std::fs::read(&img_path) {
            Ok(b) => b,
            Err(e) => {
                ctx.bad(&format!("read staged slot {s}: {e}"));
                continue;
            }
        };
        let isz = bytes.len() as u64;
        let part = format!("vendor_boot_{s}");
        // Partition size: fastboot getvar on the bootloader transport,
        // `blockdev --getsize64` over adb (64 MB fallback, noted in the
        // log, when the block tool is missing).
        let psz: u64 = if via_adb(ctx) {
            let su = match ctx.transport {
                Transport::Adb { su, .. } => su,
                _ => false,
            };
            match fb::adb_block_size(&ctx.cfg.tools.adb, &ctx.serial, su, &part, &mut ctx.log) {
                Some(n) if n > 0 => n,
                _ => {
                    let _ = writeln!(ctx.log, "  (partition size unknown over adb, assuming 64 MB)");
                    67108864
                }
            }
        } else {
            fb::getvar(&ctx.cfg.tools.fastboot, &ctx.serial, &format!("partition-size:{part}"), &mut ctx.log)
                .and_then(|v| {
                    let h = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")).unwrap_or(&v);
                    u64::from_str_radix(h, 16).ok()
                })
                .unwrap_or(0)
        };
        match verdict_text(&bytes, &format!("staged slot {s}")) {
            Ok(t) => {
                let _ = writeln!(ctx.log, "{t}");
            }
            Err(t) => {
                let _ = writeln!(ctx.log, "{t}");
                ctx.bad(&format!("slot {s}: image INVALID, refusing (see log)"));
                continue;
            }
        }
        let csz = match cpio_payload(&bytes) {
            Ok(n) => n,
            Err(e) => {
                ctx.bad(&format!("report: unpack slot {s} (see log): {e}"));
                continue;
            }
        };
        let _ = writeln!(ctx.log, "  slot {s}: cpio payload inside {}", mb(csz));
        let free = psz as i64 - isz as i64;
        let free_u = free.max(0) as u64;
        if psz == 0 || free < 0 {
            ctx.say(&format!("  Slot {s}: {} / {} · free {} — DOES NOT FIT", mb(isz), mb(psz), mb(free_u)));
            ctx.bad(&format!("slot {s}: image does not fit partition"));
        } else if !ctx.force && (free as u64) < ctx.cfg.min_free {
            ctx.say(&format!("  Slot {s}: {} / {} · free {} — BELOW {} MB POLICY", mb(isz), mb(psz), mb(free_u), ctx.cfg.min_free_mb));
            ctx.bad(&format!("slot {s}: less than {} MB free left in partition", ctx.cfg.min_free_mb));
            ctx.policy_fail = true;
        } else if ctx.force && (free as u64) < ctx.cfg.min_free {
            ctx.say(&format!("  Slot {s}: {} / {} · free {} — OK (--force: {} MB policy waived)", mb(isz), mb(psz), mb(free_u), ctx.cfg.min_free_mb));
            let _ = writeln!(ctx.log, "  ({} MB free-space policy waived under --force)", ctx.cfg.min_free_mb);
        } else {
            ctx.say(&format!("  Slot {s}: {} / {} · free {} — OK", mb(isz), mb(psz), mb(free_u)));
        }
    }
    if ctx.fail > 0 {
        if ctx.policy_fail {
            let fslots = if ctx.slots.len() == 2 { "both".to_string() } else { ctx.slots.join(" ") };
            let mut fcmd = format!("{LAUNCHER} --force --slot {fslots} --mode {}", ctx.mode);
            if ctx.mode == "restore" {
                let shown = ctx.restore_src.as_ref().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("?");
                fcmd.push_str(&format!(" --backup {shown}"));
            }
            ctx.say("");
            ctx.say(&format!("  !! Partition would be left with less than {} MB free.", ctx.cfg.min_free_mb));
            ctx.say(&format!("  !! Please send {} to the OrangeFox Pixel group:", short_path(&ctx.log_path)));
            ctx.say("  !!   @OFRPforTensorDiscussion (https://t.me/OFRPforTensorDiscussion)");
            ctx.say("  !! If you know what you are doing, rerun from a terminal:");
            ctx.say(&format!("  !!   {fcmd}"));
            ctx.say(&format!("  !! (--force skips the {} MB check; the fit check stays)", ctx.cfg.min_free_mb));
        }
        ctx.say("  report failed, nothing flashed");
        return false;
    }
    true
}

/// Flash confirmation. Ok(true) = flash, Ok(false) = abort,
/// Err(idx) = back to selections at stage idx.
fn flash_menu(ctx: &mut Ctx, plan: &StagePlan) -> Result<bool, usize> {
    if ctx.force {
        ctx.say("  --force: flashing without asking");
        return Ok(true);
    }
    let stages = build_stages(plan, ctx);
    let mut last = None;
    for (i, st) in stages.iter().enumerate() {
        if *st != Stage::Pacing {
            last = Some(i);
        }
    }
    let mut opts = vec!["Yes, flash".to_string()];
    if last.is_some() {
        opts.push(BACK_ITEM.to_string());
    }
    opts.push(EXIT_ITEM.to_string());
    match menu("Flash these images", 1, &opts).as_deref() {
        Ok("Yes, flash") => Ok(true),
        Ok(c) if c == BACK_ITEM => Err(last.unwrap_or(0)),
        _ => Ok(false),
    }
}

/// Post-install reboot offer (interactive runs only — never under
/// --force/--file): reboot the just-flashed device to recovery, over
/// fastboot on the bootloader transport or over adb on the rooted
/// transport. Default is No; the launchers must not ask again.
fn offer_reboot(ctx: &mut Ctx) {
    if ctx.force || !std::io::stdin().is_terminal() {
        return;
    }
    let opts = vec!["No, stay where it is".to_string(), "Yes, reboot to recovery".to_string()];
    match menu("Reboot to recovery", 1, &opts).as_deref() {
        Ok("Yes, reboot to recovery") => {
            ctx.say(&format!("  rebooting {} to recovery...", ctx.serial));
            let rc = match ctx.transport {
                Transport::Fastboot => fb::reboot_recovery(&ctx.cfg.tools.fastboot, &ctx.serial, &mut ctx.log),
                Transport::Adb { .. } => fb::adb_reboot_recovery(&ctx.cfg.tools.adb, &ctx.serial, &mut ctx.log),
            };
            match rc {
                Ok(()) => ctx.say("  reboot command sent"),
                Err(_) => ctx.say("  reboot failed: boot Recovery mode manually"),
            }
        }
        _ => match ctx.transport {
            Transport::Fastboot => ctx.say("  (leaving the device in the bootloader)"),
            Transport::Adb { .. } => ctx.say("  (leaving the device as-is)"),
        },
    }
}

/// [5/5] flash + fetch-back proof. False = exit 2.
/// Fastboot transport: `fastboot flash` + `fetch` proof. Adb-root
/// transport: `push` the staged image, `blockdev --setrw` (best
/// effort), `dd` into the block, then `dd` the block back out and
/// `pull` for the same byte-prefix comparison.
fn stage_flash(ctx: &mut Ctx) -> bool {
    ctx.step("[5/5] Flash");
    if via_adb(ctx) {
        let su = match ctx.transport {
            Transport::Adb { su, .. } => su,
            _ => false,
        };
        for s in ctx.slots.clone() {
            let part = format!("vendor_boot_{s}");
            let img = ctx.work.join(format!("flash_{s}.img"));
            let tmp = format!("{DEV_TMP}/flash_{s}.img");
            if fb::adb_push(&ctx.cfg.tools.adb, &ctx.serial, &img, &tmp, &mut ctx.log).is_err() {
                ctx.bad(&format!("push vendor_boot_{s} (see log)"));
                continue;
            }
            fb::adb_setrw(&ctx.cfg.tools.adb, &ctx.serial, su, &part, &mut ctx.log);
            let blk = format!("/dev/block/by-name/{part}");
            match fb::adb_shell(&ctx.cfg.tools.adb, &ctx.serial, su, &format!("dd if={tmp} of={blk} bs=1048576"), &mut ctx.log) {
                Ok(o) if o.status.success() => ctx.ok(&format!("vendor_boot_{s} flashed")),
                _ => ctx.bad(&format!("dd vendor_boot_{s} to block (see log)")),
            }
        }
        if ctx.fail > 0 {
            ctx.say(&format!("  flash failed (device left in adb mode, stock: {})", stock_desc(&ctx)));
            return false;
        }
        for s in ctx.slots.clone() {
            let img = ctx.work.join(format!("flash_{s}.img"));
            let check = ctx.work.join(format!("check_{s}.img"));
            let flashed = match std::fs::read(&img) {
                Ok(b) => b,
                Err(e) => {
                    ctx.bad(&format!("read staged slot {s}: {e}"));
                    continue;
                }
            };
            let part = format!("vendor_boot_{s}");
            let blk = format!("/dev/block/by-name/{part}");
            let tmp = format!("{DEV_TMP}/check_{s}.img");
            let ok = fb::adb_shell(&ctx.cfg.tools.adb, &ctx.serial, su, &format!("dd if={blk} of={tmp} bs=1048576"), &mut ctx.log)
                .map(|o| o.status.success())
                .unwrap_or(false)
                && fb::adb_pull(&ctx.cfg.tools.adb, &ctx.serial, &tmp, &check, &mut ctx.log).is_ok()
                && std::fs::read(&check).map(|back| back.len() >= flashed.len() && back[..flashed.len()] == flashed[..]).unwrap_or(false);
            if ok {
                ctx.ok(&format!("slot {s} on device matches"));
            } else {
                ctx.bad(&format!("slot {s} fetch-back mismatch (see log)"));
            }
        }
        let _ = fb::adb_shell(&ctx.cfg.tools.adb, &ctx.serial, su, &format!("rm -f {DEV_TMP}/flash_?.img {DEV_TMP}/check_?.img"), &mut ctx.log);
        return true;
    }
    for s in ctx.slots.clone() {
        let part = format!("vendor_boot_{s}");
        let img = ctx.work.join(format!("flash_{s}.img"));
        match fb::flash(&ctx.cfg.tools.fastboot, &ctx.serial, &part, &img, &mut ctx.log) {
            Ok(()) => ctx.ok(&format!("vendor_boot_{s} flashed")),
            Err(_) => ctx.bad(&format!("flash vendor_boot_{s} (see log)")),
        }
    }
    if ctx.fail > 0 {
        ctx.say(&format!("  flash failed (device left in bootloader, stock: {})", stock_desc(&ctx)));
        return false;
    }
    for s in ctx.slots.clone() {
        let img = ctx.work.join(format!("flash_{s}.img"));
        let check = ctx.work.join(format!("check_{s}.img"));
        let flashed = match std::fs::read(&img) {
            Ok(b) => b,
            Err(e) => {
                ctx.bad(&format!("read staged slot {s}: {e}"));
                continue;
            }
        };
        let part = format!("vendor_boot_{s}");
        let ok = fb::fetch(&ctx.cfg.tools.fastboot, &ctx.serial, &part, &check, &mut ctx.log).is_ok()
            && std::fs::read(&check).map(|back| back.len() >= flashed.len() && back[..flashed.len()] == flashed[..]).unwrap_or(false);
        if ok {
            ctx.ok(&format!("slot {s} on device matches"));
        } else {
            ctx.bad(&format!("slot {s} fetch-back mismatch (see log)"));
        }
    }
    true
}

/// `install --file`: recovery install into a plain image file, no
/// device, no backup, no menus (built for recovery use).
/// Verify input -> rebuild with the cpio payload (footer dropped,
/// same layout as the device flow) -> verify output -> write.
/// Short status lines go to stdout, verdict details to stderr; with
/// --log FILE both streams are additionally tee'd into the file.
/// Exit codes: 0 ok, 1 usage, 2 verify/build failure.
fn run_file(cli: &Cli) -> i32 {
    // Optional tee log (--log FILE); console behavior never changes.
    let mut log: Option<File> = None;
    if !cli.log.is_empty() {
        match File::create(&cli.log) {
            Ok(f) => log = Some(f),
            Err(e) => {
                println!("  FAIL: cannot create log file: {e}");
                return 2;
            }
        }
    }
    // Status line: stdout + log. Detail: stderr + log.
    // Console lines may carry ANSI (RESULT); the log gets them stripped.
    macro_rules! say {
        ($($t:tt)*) => {{
            let line = format!($($t)*);
            println!("{line}");
            if let Some(f) = log.as_mut() {
                let _ = writeln!(f, "{}", console::strip_ansi_codes(&line));
            }
        }};
    }
    macro_rules! detail {
        ($t:expr) => {{
            let text: String = $t;
            eprintln!("{text}");
            if let Some(f) = log.as_mut() {
                let _ = writeln!(f, "{}", console::strip_ansi_codes(&text));
            }
        }};
    }
    say!("OrangeFox file install");
    let same = std::path::absolute(&cli.input).ok() == std::path::absolute(&cli.output).ok();
    if same {
        eprintln!("usage error: input and output are the same file");
        return 1;
    }
    let img = match std::fs::read(&cli.input) {
        Ok(b) => b,
        Err(e) => {
            say!("  FAIL: cannot read input: {e}");
            return 2;
        }
    };
    let payload = match std::fs::read(&cli.cpio) {
        Ok(b) => b,
        Err(e) => {
            say!("  FAIL: cannot read cpio payload: {e}");
            return 2;
        }
    };
    match verdict_text(&img, "input") {
        Ok(t) => {
            detail!(t);
            match ops::analyze(&img) {
                Ok(a) => say!("  ok: input valid ({} fragments, ramdisk {})", a.frags.len(), mb(a.header_ramdisk_size as u64)),
                Err(e) => {
                    say!("  FAIL: input unreadable: {e}");
                    return 2;
                }
            }
        }
        Err(t) => {
            detail!(t);
            say!("  FAIL: input image INVALID, nothing written");
            return 2;
        }
    }
    let label = Path::new(&cli.cpio).file_name().and_then(|n| n.to_str()).unwrap_or("payload").to_string();
    let rip = match layout_variant(&cli.recovery_is_platform) {
        Ok(v) => v,
        Err(e) => {
            say!("  FAIL: {e}");
            return 2;
        }
    };
    match rip {
        None => say!("  rebuild ({label}, footer dropped, Type A classic)"),
        Some(RecoveryInPlatform::Var1) => say!("  rebuild ({label}, footer dropped, Type B var1: all-in-platform)"),
        Some(RecoveryInPlatform::Var2) => say!("  rebuild ({label}, footer dropped, Type C var2: payload-only platform)"),
    }
    let opts = RepackOpts { mode: Mode::Keep, drop: Vec::new(), sets: Vec::new(), recovery: Some((label, payload)), recovery_is_platform: rip, drop_footer: true };
    let _quiet = QuietGuard::on();
    let out = match ops::repack_with_opts(&img, None, opts) {
        Ok(o) => o,
        Err(e) => {
            say!("  FAIL: rebuild failed, nothing written: {e}");
            return 2;
        }
    };
    match verdict_text(&out, "output") {
        Ok(t) => detail!(t),
        Err(t) => {
            detail!(t);
            say!("  FAIL: rebuilt image INVALID, nothing written");
            return 2;
        }
    }
    match std::fs::write(&cli.output, &out) {
        Ok(()) => say!("  ok: wrote {} ({})", cli.output, mb(out.len() as u64)),
        Err(e) => {
            say!("  FAIL: cannot write output: {e}");
            return 2;
        }
    }
    say!("{}", style("RESULT: OK").green().bold());
    0
}

// ----------------------------------------------------------------- demo ---

/// Canned sizes for the demo walk-through (slot a/b like a real shiba).
fn demo_sizes(s: &str) -> (&'static str, &'static str) {
    match s {
        "a" => ("30.1 MB", "33.9 MB"),
        _ => ("47.0 MB", "17.0 MB"),
    }
}

/// `install --demo`: UI preview with a canned device. No export.txt,
/// no fastboot/adb, no filesystem writes — the same menus and the same
/// screen layout as the real flow, fed with fixed numbers. Nothing is
/// read, written or flashed; abort and completion both exit 0.
fn run_demo() -> i32 {
    use std::time::Duration;
    const STAMP: &str = "20260921-234120";
    const SERIAL: &str = "38311FDJH006TB";
    const FOX: &str = "OrangeFox-R12.0-test8.1-aio.ramdisk.lz4";
    fn hdr(t: &str) {
        println!();
        println!("{}", rule(t));
    }
    fn wait(ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
    println!("OrangeFox vendor_boot installer");
    println!("{}", style("(demo — no device touched, nothing flashed)").dim());
    macro_rules! dabort {
        () => {{
            println!("  aborted by user, nothing flashed");
            return 0;
        }};
    }

    hdr("[1/5] Device");
    // Canned adb-system endpoint with root, so the demo walks the new
    // device questions too: root probe, branch choice, reboot gate.
    println!("  {} {SERIAL} via adb (Pixel 8) (system)", dot());
    println!("  {} root: yes (adb shell)", dot());
    let mut via_adb = false;
    match menu("How to install", 1, &["via bootloader (fastboot, recommended)".to_string(), "via adb root (no reboot to bootloader)".to_string(), EXIT_ITEM.to_string()]).as_deref() {
        Ok("via adb root (no reboot to bootloader)") => {
            via_adb = true;
            println!("  {} via adb root (system; no reboot)", dot());
        }
        Ok("via bootloader (fastboot, recommended)") => {
            println!("  {} via bootloader (fastboot)", dot());
            match menu("Ready to reboot to bootloader", 1, &["Yes, reboot now".to_string(), EXIT_ITEM.to_string()]).as_deref() {
                Ok("Yes, reboot now") => println!("  {} {SERIAL} — bootloader", dot()),
                _ => dabort!(),
            }
        }
        _ => dabort!(),
    }
    println!("  {} product shiba · active slot b{}", dot(), if via_adb { " (via adb system)" } else { "" });
    println!("  {} log: logs/{STAMP}/install.log", dot());

    // [2/5] the same interactive menus as the real flow.
    hdr("[2/5] Plan");
    #[derive(Clone, Copy, PartialEq)]
    enum Ds { Mode, Slots, Backup, Fresh, Name, Pacing }
    let mut stages = vec![Ds::Mode, Ds::Slots, Ds::Pacing];
    let mut mode = String::new();
    let mut slots: Vec<String> = Vec::new();
    let mut stamp = String::new();
    let mut fresh = true;
    let mut bname = STAMP.to_string();
    let mut idx = 0;
    loop {
        match stages[idx] {
            Ds::Mode => {
                match menu("Action", 1, &["Install OrangeFox".to_string(), "Restore backup".to_string(), EXIT_ITEM.to_string()]).as_deref() {
                    Ok("Install OrangeFox") => mode = "install".to_string(),
                    Ok("Restore backup") => mode = "restore".to_string(),
                    _ => dabort!(),
                }
                println!("  {} Action: {}", dot(), if mode == "install" { "Install OrangeFox" } else { "Restore backup" });
                if mode == "restore" {
                    stages = vec![Ds::Mode, Ds::Slots, Ds::Backup, Ds::Fresh, Ds::Pacing];
                }
                idx = 1;
            }
            Ds::Slots => {
                let mut opts = vec!["Slot a only".to_string(), "Slot b only".to_string(), "Both (a+b)".to_string()];
                if idx > 0 {
                    opts.push(BACK_ITEM.to_string());
                }
                opts.push(EXIT_ITEM.to_string());
                match menu("Slots", 3, &opts).as_deref() {
                    Ok("Slot a only") => slots = vec!["a".to_string()],
                    Ok("Slot b only") => slots = vec!["b".to_string()],
                    Ok("Both (a+b)") => slots = vec!["a".to_string(), "b".to_string()],
                    Ok(c) if c == BACK_ITEM => {
                        idx -= 1;
                        continue;
                    }
                    _ => dabort!(),
                }
                println!("  {} Slots: {}", dot(), slots.join(" + "));
                idx += 1;
            }
            Ds::Backup => {
                let opts = vec![
                    format!("latest: {STAMP} (a, b)"),
                    "20260921-120405 (b)".to_string(),
                    BACK_ITEM.to_string(),
                    EXIT_ITEM.to_string(),
                ];
                match menu("Backup", 1, &opts).as_deref() {
                    Ok(c) if c == BACK_ITEM => {
                        idx -= 1;
                        continue;
                    }
                    Ok(c) if c == EXIT_ITEM => dabort!(),
                    Ok(c) => {
                        stamp = c.trim_start_matches("latest: ").split(" (").next().unwrap_or("?").to_string();
                    }
                    Err(_) => dabort!(),
                }
                println!("  {dot} Backup: backup/{stamp}", dot = dot());
                idx += 1;
            }
            Ds::Fresh => {
                let opts = vec![
                    "Yes, back up current stock first".to_string(),
                    "No, flash without a fresh backup".to_string(),
                    BACK_ITEM.to_string(),
                    EXIT_ITEM.to_string(),
                ];
                match menu("Fresh backup", 1, &opts).as_deref() {
                    Ok("Yes, back up current stock first") => fresh = true,
                    Ok("No, flash without a fresh backup") => fresh = false,
                    Ok(c) if c == BACK_ITEM => {
                        idx -= 1;
                        continue;
                    }
                    _ => dabort!(),
                }
                println!("  {} Fresh backup: {}", dot(), if fresh { "yes" } else { "no (your choice, no safety copy)" });
                stages.retain(|s| *s != Ds::Name);
                if fresh {
                    let at = stages.iter().position(|s| *s == Ds::Pacing).unwrap_or(stages.len());
                    stages.insert(at, Ds::Name);
                }
                idx += 1;
            }
            Ds::Name => {
                let use_label = format!("Use {STAMP}");
                let opts = vec![use_label.clone(), "Custom name...".to_string(), BACK_ITEM.to_string(), EXIT_ITEM.to_string()];
                match menu("Backup folder", 1, &opts).as_deref() {
                    Ok(c) if c == use_label => bname = STAMP.to_string(),
                    Ok(c) if c == "Custom name..." => {
                        let theme = BarTheme::default();
                        let raw: String = match dialoguer::Input::with_theme(&theme)
                            .with_prompt("Backup folder name (empty = run stamp)")
                            .interact_text()
                        {
                            Ok(a) => a,
                            Err(_) => dabort!(),
                        };
                        let want = if raw.trim().is_empty() { STAMP.to_string() } else { raw.trim().to_string() };
                        match clean_backup_name(&want) {
                            Some(n) => bname = n,
                            None => {
                                println!("  bad name (one folder, no slashes) — try again");
                                continue;
                            }
                        }
                    }
                    Ok(c) if c == BACK_ITEM => {
                        idx -= 1;
                        continue;
                    }
                    _ => dabort!(),
                }
                println!("  {} Backup folder: backup/{bname}", dot());
                idx += 1;
            }
            Ds::Pacing => {
                let mut opts = vec!["Continue".to_string()];
                if idx > 0 {
                    opts.push(BACK_ITEM.to_string());
                }
                opts.push(EXIT_ITEM.to_string());
                match menu("Ready to proceed", 1, &opts).as_deref() {
                    Ok("Continue") => break,
                    Ok(c) if c == BACK_ITEM => idx -= 1,
                    _ => dabort!(),
                }
            }
        }
    }

    if mode == "install" || fresh {
        hdr(&format!("[3/5] Backup stock → backup/{bname}/"));
        for s in &slots {
            wait(350);
            if via_adb {
                println!("  {} dd /dev/block/by-name/vendor_boot_{s} → /data/local/ofox_installer/ + pull", dot());
            }
            println!("  {} vendor_boot_{s} saved (64.0 MB)", tick());
        }
    } else {
        hdr("[3/5] Backup stock");
        println!("  {} Fresh backup skipped — flashing without a safety copy", dot());
    }

    if mode == "install" {
        hdr(&format!("[4/5] Rebuild — {FOX} (footer dropped)"));
        println!("  footer dropped: grown ramdisk + old footer would overflow the partition");
        println!("  {} Layout: Type A (classic)", dot());
        for s in &slots {
            wait(450);
            let (img, _) = demo_sizes(s);
            println!("  {} slot {s} rebuilt ({img})", tick());
        }
    } else {
        hdr(&format!("[4/5] Stage restore — backup/{stamp} (as-is, no checks)"));
        for s in &slots {
            wait(350);
            println!("  {} slot {s} ready to flash (64.0 MB)", tick());
        }
    }
    if mode == "install" {
        println!();
        println!("  Report (rebuilt images; details in install.log):");
        for s in &slots {
            let (img, free) = demo_sizes(s);
            println!("  Slot {s}: {img} / 64.0 MB · free {free} · OK");
        }
    } else {
        println!("  {} no checks on restore — images go to the device as stored", dot());
    }

    // Flash confirmation with Back (re-runs the fake prepare).
    loop {
        let opts = vec!["Yes, flash".to_string(), BACK_ITEM.to_string(), EXIT_ITEM.to_string()];
        match menu("Flash these images", 1, &opts).as_deref() {
            Ok("Yes, flash") => break,
            Ok(c) if c == BACK_ITEM => {
                println!("  (demo: plan kept, report re-shown)");
                continue;
            }
            _ => {
                println!("  aborted by user, nothing flashed");
                return 0;
            }
        }
    }

    hdr("[5/5] Flash");
    for s in &slots {
        wait(400);
        if via_adb {
            println!("  {} push flash_{s}.img + blockdev --setrw + dd → block", dot());
        }
        println!("  {} vendor_boot_{s} flashed", tick());
    }
    for s in &slots {
        wait(250);
        println!("  {} slot {s} on device matches", tick());
    }
    println!();
    println!("{}", style("RESULT: OK (demo — nothing flashed)").green().bold());
    // The real flow asks the reboot question here (fake in demo).
    match menu("Reboot to recovery", 1, &["No, stay where it is".to_string(), "Yes, reboot to recovery".to_string()]).as_deref() {
        Ok("Yes, reboot to recovery") => println!("  (demo — reboot not sent)"),
        _ => println!("  (leaving the device as-is)"),
    }
    0
}

// ----------------------------------------------------------------- run ---

/// Run `install`. Exit code (0 ok / clean abort, 1 usage / no device,
/// 2 build/verify/flash failure).
pub fn run(args: &[String], prog: &str) -> i32 {
    if args.iter().any(|a| a == "--help") {
        println!("{}", help::short(prog));
        return 0;
    }
    if args.iter().any(|a| a == "--expand") {
        println!("{}", help::expand(prog));
        return 0;
    }
    let cli = match parse_cli(args) {
        Ok(c) => c,
        Err(m) => {
            eprintln!("usage error: {m}\n{}", help::short(prog));
            return 1;
        }
    };
    if cli.file {
        return run_file(&cli);
    }
    if !cli.force && !std::io::stdin().is_terminal() {
        eprintln!("no terminal on stdin: rerun in a terminal or pass --force");
        return 1;
    }
    if cli.demo {
        return run_demo();
    }
    let export_path = match find_export(&cli.export) {
        Ok(p) => p,
        Err(m) => {
            eprintln!("{m}");
            return 1;
        }
    };
    let cfg = match resolve_config(&export_path) {
        Ok(c) => c,
        Err(m) => {
            eprintln!("{m}");
            return 1;
        }
    };

    let stamp = stamp_utc();
    // Logs live apart from image backups: logs/<stamp>/install.log
    // (the launchers find the run log there for the reboot offer).
    let log_dir = cfg.log_dir.join(&stamp);
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!("cannot create {}: {e}", log_dir.display());
        return 1;
    }
    let log_path = log_dir.join("install.log");
    let mut log = match File::create(&log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot create {}: {e}", log_path.display());
            return 1;
        }
    };
    let _ = writeln!(log, "install {stamp} force={} slot={} mode={} backup={}", cli.force as u8, cli.slot, cli.mode, cli.backup);
    let _ = writeln!(log, "export: {}", export_path.display());
    let _ = writeln!(log, "recovery: {}", cfg.recovery_img.display());
    let _ = writeln!(log, "{} {}", prog, env!("CARGO_PKG_VERSION"));
    fb::version(&cfg.tools.fastboot, &mut log);

    let work = std::env::temp_dir().join(format!("bootsmasher-install-{stamp}-{}", std::process::id()));
    let _guard = WorkGuard(work.clone());

    let mut ctx = Ctx {
        cfg,
        log,
        log_path: log_path.clone(),
        stamp: stamp.clone(),
        backup: PathBuf::new(), // set below once mode/fresh are known
        backup_name: stamp.clone(),
        fresh: true,
        pass: 0,
        fail: 0,
        policy_fail: false,
        force: cli.force,
        serial: String::new(),
        mode: String::new(),
        layout: String::new(),
        transport: Transport::Fastboot,
        slots: Vec::new(),
        restore_src: None,
        work,
    };

    ctx.say("OrangeFox vendor_boot installer");
    match stage_device(&mut ctx, &cli.transport) {
        DevStage::Abort => return 0,
        DevStage::Fail => return 1,
        DevStage::Ok => {}
    }

    // --- [2/5] static plan: flags/force known upfront ---
    let backups = list_backups(&ctx.cfg.backup_dir);
    if !cli.mode.is_empty() {
        ctx.mode = cli.mode.clone();
        ctx.say(&format!("  • Action: {} (flag)", if ctx.mode == "install" { "Install OrangeFox" } else { "Restore backup" }));
    }
    if !cli.slot.is_empty() {
        ctx.slots = if cli.slot == "both" { vec!["a".to_string(), "b".to_string()] } else { vec![cli.slot.clone()] };
        ctx.say(&format!("  • Slots: {} (flag)", ctx.slots.join(" + ")));
    }
    if !cli.recovery_is_platform.is_empty() {
        if layout_variant(&cli.recovery_is_platform).is_err() {
            ctx.say("  bad --recovery-is-platform (want var1|var2)");
            return 1;
        }
        ctx.layout = cli.recovery_is_platform.clone();
        ctx.say(&format!("  • Layout: {} (flag)", layout_name(&ctx.layout)));
    }
    if cli.force {
        if ctx.mode.is_empty() {
            ctx.mode = "install".to_string();
            ctx.say("  • Action: Install OrangeFox (--force)");
        }
        if ctx.slots.is_empty() {
            ctx.slots = vec!["a".to_string(), "b".to_string()];
            ctx.say("  • Slots: a + b (--force)");
        }
    }
    if ctx.mode.is_empty() && backups.is_empty() {
        ctx.mode = "install".to_string();
        ctx.say("  • Action: Install OrangeFox (no backups stored yet)");
    }
    // Default fresh-backup home (install always; restore keeps it
    // unless the Fresh/BackupName stages say otherwise).
    ctx.backup = ctx.cfg.backup_dir.join(&ctx.backup_name);
    if cli.force && ctx.mode == "restore" {
        ctx.say("  • Fresh backup: yes (--force)");
    }
    if ctx.mode == "restore" {
        if !cli.backup.is_empty() {
            if cli.backup == "latest" {
                match backups.first() {
                    Some((s, _)) => ctx.restore_src = Some(ctx.cfg.backup_dir.join(s)),
                    None => {
                        ctx.say("  no backups stored yet");
                        return 1;
                    }
                }
            } else {
                let cand = ctx.cfg.backup_dir.join(&cli.backup);
                if cand.is_dir() {
                    ctx.restore_src = Some(cand);
                } else {
                    ctx.say(&format!("  backup not found: {}", cli.backup));
                    return 1;
                }
            }
            let shown = ctx.restore_src.as_ref().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("?");
            ctx.say(&format!("  • Backup: backup/{shown} (flag)"));
        } else if cli.force {
            ctx.say("  restore needs --backup in --force mode");
            return 1;
        }
    }
    let plan = StagePlan {
        mode_menu: cli.mode.is_empty() && !cli.force && !backups.is_empty() && ctx.mode.is_empty(),
        // Layout choice is temporarily disabled: install always goes
        // Type A (classic), the question is skipped. The flag path
        // (--recovery-is-platform) and --file keep working, and the
        // menu can be brought back with BOOTSMASHER_LAYOUT_MENU=1.
        layout_menu: cli.recovery_is_platform.is_empty() && !cli.force
            && std::env::var_os("BOOTSMASHER_LAYOUT_MENU").is_some(),
        slot_menu: cli.slot.is_empty() && !cli.force,
        backup_menu: cli.backup.is_empty() && !cli.force,
        // Restore asks "fresh backup first?" + folder name (install
        // always backs up; --force restore assumes yes into the stamp).
        fresh_menu: !cli.force,
    };

    // --- rounds: selections -> fetch -> prepare/report -> flash menu ---
    let mut resume: Option<usize> = None;
    loop {
        if !run_selections(&mut ctx, &plan, &backups, resume.take().unwrap_or(0)) {
            return 0;
        }
        if !stage_fetch(&mut ctx) {
            return 2;
        }
        if !stage_prepare(&mut ctx) {
            return 2;
        }
        match flash_menu(&mut ctx, &plan) {
            Ok(true) => break,
            Ok(false) => {
                ctx.say("  aborted by user, nothing flashed");
                return 0;
            }
            Err(idx) => {
                resume = Some(idx);
                continue;
            }
        }
    }

    if !stage_flash(&mut ctx) {
        ctx.say("");
        ctx.say(&format!("{}", style(format!("RESULT: FAIL (pass={} fail={}; see {})", ctx.pass, ctx.fail, short_path(&ctx.log_path))).red().bold()));
        return 2;
    }
    ctx.say("");
    if ctx.fail == 0 {
        ctx.say(&format!("{}", style(format!("RESULT: OK (pass={}; stock: {}; log: {})", ctx.pass, stock_desc(&ctx), short_path(&ctx.log_path))).green().bold()));
        offer_reboot(&mut ctx);
        0
    } else {
        ctx.say(&format!("{}", style(format!("RESULT: FAIL (pass={} fail={}; see {})", ctx.pass, ctx.fail, short_path(&ctx.log_path))).red().bold()));
        2
    }
}
