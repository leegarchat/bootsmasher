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
use std::fs::File;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::common::codec;
use crate::common::cpio;
use crate::common::vendor::type_name;
use crate::vboot::ops::{self, Mode, RepackOpts};

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
}

fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let mut c = Cli { force: false, slot: String::new(), mode: String::new(), backup: String::new(), export: String::new(), file: false, input: String::new(), cpio: String::new(), output: String::new(), log: String::new() };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--force" => c.force = true,
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
            other => return Err(format!("unknown arg: {other}")),
        }
        i += 1;
    }
    if c.file {
        if c.force || !c.slot.is_empty() || !c.mode.is_empty() || !c.backup.is_empty() || !c.export.is_empty() {
            return Err("--file takes no --force/--slot/--mode/--backup/--export".to_string());
        }
        if c.input.is_empty() || c.cpio.is_empty() || c.output.is_empty() {
            return Err("--file needs -i INPUT -c CPIOPAYLOAD -o OUTPUT".to_string());
        }
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
    Ok(Config { recovery_img, backup_dir, tools: Tools { fastboot, adb }, min_free: min_free_mb * 1024 * 1024, min_free_mb })
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
    stamp: String,
    backup: PathBuf,
    pass: u32,
    fail: u32,
    policy_fail: bool,
    force: bool,
    serial: String,
    mode: String,
    slots: Vec<String>,
    restore_src: Option<PathBuf>,
    work: PathBuf,
}

impl Ctx {
    fn say(&mut self, line: &str) {
        println!("{line}");
        let _ = writeln!(self.log, "{line}");
        let _ = self.log.flush();
    }
    fn ok(&mut self, what: &str) {
        self.pass += 1;
        self.say(&format!("  ok: {what}"));
    }
    fn bad(&mut self, what: &str) {
        self.fail += 1;
        self.say(&format!("  FAIL: {what}"));
    }
    fn step(&mut self, what: &str) {
        self.say("");
        self.say(&format!("== {what} =="));
    }
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

/// Arrow-key menu (dialoguer, same widget as `pick`). Err(true) is
/// user walk-away (Esc/q), Err(false) is an I/O error.
fn menu(prompt: &str, def_1based: usize, options: &[String]) -> Result<String, bool> {
    let mut sel = dialoguer::Select::new().with_prompt(prompt).items(options);
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
    Slot,
    Backup,
    Pacing,
}

struct StagePlan {
    mode_menu: bool,
    slot_menu: bool,
    backup_menu: bool,
}

fn build_stages(plan: &StagePlan, force: bool, mode: &str) -> Vec<Stage> {
    let mut v = Vec::new();
    if plan.mode_menu {
        v.push(Stage::Mode);
    }
    if plan.slot_menu {
        v.push(Stage::Slot);
    }
    if mode == "restore" && plan.backup_menu {
        v.push(Stage::Backup);
    }
    if !force {
        v.push(Stage::Pacing);
    }
    v
}

/// Interactive selections with back-navigation. False = user abort
/// (already announced); true leaves mode/slots/restore set.
fn run_selections(ctx: &mut Ctx, plan: &StagePlan, backups: &[(String, String)], mut idx: usize) -> bool {
    let mut stages = build_stages(plan, ctx.force, &ctx.mode);
    if !stages.is_empty() {
        ctx.step("[2/5] selection");
    }
    while idx < stages.len() {
        match stages[idx] {
            Stage::Mode => {
                let opts = vec!["Install OrangeFox".to_string(), "Restore a backup".to_string(), EXIT_ITEM.to_string()];
                match menu("What to do?", 1, &opts) {
                    Ok(c) if c == "Install OrangeFox" => {
                        ctx.mode = "install".to_string();
                        ctx.restore_src = None;
                    }
                    Ok(c) if c == "Restore a backup" => ctx.mode = "restore".to_string(),
                    _ => {
                        ctx.say("  aborted by user, nothing flashed");
                        return false;
                    }
                }
                ctx.say(&format!("  mode: {}", ctx.mode));
                stages = build_stages(plan, ctx.force, &ctx.mode);
                idx = 1; // mode is always stages[0] when asked
            }
            Stage::Slot => {
                let def = match ctx.slots.iter().map(|s| s.as_str()).collect::<Vec<_>>().as_slice() {
                    ["a"] => 1,
                    ["b"] => 2,
                    _ => 3,
                };
                let mut opts = vec!["only a".to_string(), "only b".to_string(), "both (a+b)".to_string()];
                if idx > 0 {
                    opts.push(BACK_ITEM.to_string());
                }
                opts.push(EXIT_ITEM.to_string());
                match menu("Slots to flash:", def, &opts).as_deref() {
                    Ok("only a") => ctx.slots = vec!["a".to_string()],
                    Ok("only b") => ctx.slots = vec!["b".to_string()],
                    Ok("both (a+b)") => ctx.slots = vec!["a".to_string(), "b".to_string()],
                    Ok(c) if c == BACK_ITEM => {
                        idx -= 1;
                        continue;
                    }
                    _ => {
                        ctx.say("  aborted by user, nothing flashed");
                        return false;
                    }
                }
                ctx.say(&format!("  slots: {}", ctx.slots.join(" ")));
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
                match menu("Backup to restore:", def, &opts) {
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
                ctx.say(&format!("  restore from: backup/{shown}"));
                idx += 1;
            }
            Stage::Pacing => {
                let mut opts = vec!["Continue".to_string()];
                if idx > 0 {
                    opts.push(BACK_ITEM.to_string());
                }
                opts.push(EXIT_ITEM.to_string());
                match menu("Continue?", 1, &opts).as_deref() {
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

/// [1/5] device: pick, then bring to the bootloader. False = exit 1.
fn stage_device(ctx: &mut Ctx) -> bool {
    ctx.step("[1/5] device");
    let fb_devs = fb::serials(&ctx.cfg.tools.fastboot, "fastboot", &mut ctx.log);
    let adb_devs = fb::serials(&ctx.cfg.tools.adb, "device", &mut ctx.log);
    // Overwritten on every path below; the initial value is never read.
    #[allow(unused_assignments)]
    let mut serial = String::new();
    if fb_devs.len() > 1 {
        ctx.say("  several fastboot devices attached, pick one:");
        let pairs: Vec<(String, String)> = fb_devs
            .iter()
            .map(|s| {
                let label = fb::getvar(&ctx.cfg.tools.fastboot, s, "product", &mut ctx.log);
                (tag(s, label), s.clone())
            })
            .collect();
        let mut opts: Vec<String> = pairs.iter().map(|(d, _)| d.clone()).collect();
        opts.push(EXIT_ITEM.to_string());
        match menu("Device:", 1, &opts) {
            Ok(c) if c != EXIT_ITEM => {
                serial = pairs.iter().find(|(d, _)| *d == c).map(|(_, s)| s.clone()).unwrap_or(c);
            }
            _ => {
                ctx.say("  aborted by user, nothing flashed");
                return false;
            }
        }
    } else if fb_devs.len() == 1 {
        serial = fb_devs[0].clone();
    } else if adb_devs.len() > 1 {
        ctx.say("  several adb devices attached, pick one:");
        let pairs: Vec<(String, String)> = adb_devs
            .iter()
            .map(|s| {
                let label = fb::adb_label(&ctx.cfg.tools.adb, s, &mut ctx.log);
                (tag(s, label), s.clone())
            })
            .collect();
        let mut opts: Vec<String> = pairs.iter().map(|(d, _)| d.clone()).collect();
        opts.push(EXIT_ITEM.to_string());
        match menu("Device:", 1, &opts) {
            Ok(c) if c != EXIT_ITEM => {
                serial = pairs.iter().find(|(d, _)| *d == c).map(|(_, s)| s.clone()).unwrap_or(c);
            }
            _ => {
                ctx.say("  aborted by user, nothing flashed");
                return false;
            }
        }
    } else if adb_devs.len() == 1 {
        serial = adb_devs[0].clone();
        let label = fb::adb_label(&ctx.cfg.tools.adb, &serial, &mut ctx.log);
        ctx.say(&format!("  {} via adb", tag(&serial, label)));
    } else if ctx.force {
        ctx.say("  no device in fastboot or adb mode");
        return false;
    } else {
        ctx.say("  no device detected.");
        ctx.say("  Reboot it to the bootloader manually (VolDown+Power, or: adb reboot bootloader),");
        let opts = vec!["Ready, check again".to_string(), EXIT_ITEM.to_string()];
        match menu("Device ready?", 1, &opts).as_deref() {
            Ok(EXIT_ITEM) | Err(_) => {
                ctx.say("  aborted by user, nothing flashed");
                return false;
            }
            _ => {}
        }
        match fb::wait_bootloader(&ctx.cfg.tools.fastboot, 15, &mut ctx.log) {
            Some(s) => serial = s,
            None => {
                ctx.say("  Still nothing. Press Enter to exit and retry.");
                read_pause();
                ctx.say("  aborted");
                return false;
            }
        }
    }
    let _ = writeln!(ctx.log, "device={serial}");
    // Transport by presence: `fastboot -s SERIAL ...` waits forever
    // for a missing serial, so fastboot is only probed when the serial
    // is listed in `fastboot devices` (an adb-sourced serial in system
    // goes straight to `adb reboot bootloader`).
    let in_fb = fb_devs.contains(&serial);
    if in_fb && fb::is_userspace(&ctx.cfg.tools.fastboot, &serial, &mut ctx.log) {
        ctx.say(&format!("  {serial} is in fastbootd, rebooting to bootloader..."));
        fb::reboot_bootloader(&ctx.cfg.tools.fastboot, &serial, &mut ctx.log);
        match fb::wait_bootloader(&ctx.cfg.tools.fastboot, 90, &mut ctx.log) {
            Some(s) => serial = s,
            None => {
                ctx.say("  device did not come back to bootloader");
                return false;
            }
        }
    } else if in_fb {
        // Reachable over fastboot already: in the bootloader
        // (fastbootd was ruled out above).
        ctx.say(&format!("  {serial} in bootloader"));
    } else if fb::adb_alive(&ctx.cfg.tools.adb, &serial, &mut ctx.log) {
        ctx.say(&format!("  {serial} in system, rebooting to bootloader..."));
        fb::adb_reboot_bootloader(&ctx.cfg.tools.adb, &serial, &mut ctx.log);
        match fb::wait_bootloader(&ctx.cfg.tools.fastboot, 120, &mut ctx.log) {
            Some(s) => serial = s,
            None => {
                ctx.say("  device did not come back to bootloader");
                return false;
            }
        }
        ctx.say(&format!("  {serial} in bootloader"));
    } else {
        ctx.say(&format!("  {serial} not reachable over fastboot or adb"));
        return false;
    }
    ctx.serial = serial.clone();
    let product = fb::getvar(&ctx.cfg.tools.fastboot, &serial, "product", &mut ctx.log).unwrap_or_else(|| "?".to_string());
    let curslot = fb::getvar(&ctx.cfg.tools.fastboot, &serial, "current-slot", &mut ctx.log).unwrap_or_else(|| "?".to_string());
    ctx.say(&format!("  product={product} active slot={curslot} serial={serial}"));
    ctx.say(&format!("  log: {}", ctx.backup.join("install.log").display()));
    true
}

/// [3/5] fetch current stock into the run backup dir. False = exit 2.
fn stage_fetch(ctx: &mut Ctx) -> bool {
    ctx.step(&format!("[3/5] fetch current stock -> backup/{}/", ctx.stamp));
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
        ctx.step(&format!("[4/5] rebuild ({fox_name}, footer dropped)"));
        ctx.say("  (vbmeta/AVB footer is dropped: grown ramdisk + old footer would overflow the partition)");
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
            let opts = RepackOpts { mode: Mode::Keep, drop: Vec::new(), sets: Vec::new(), recovery: Some((fox_name.clone(), fox.clone())), drop_footer: true };
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
        ctx.step(&format!("[4/5] stage restore images from backup/{shown}"));
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
            match verdict_text(&bytes, &format!("backup/{shown}/vendor_boot_{s}.img")) {
                Ok(t) => {
                    let _ = writeln!(ctx.log, "{t}");
                    match std::fs::write(ctx.work.join(format!("flash_{s}.img")), &bytes) {
                        Ok(()) => ctx.ok(&format!("slot {s} backup image valid ({})", mb(bytes.len() as u64))),
                        Err(e) => ctx.bad(&format!("stage slot {s}: {e}")),
                    }
                }
                Err(t) => {
                    let _ = writeln!(ctx.log, "{t}");
                    ctx.bad(&format!("slot {s} backup image INVALID, refusing (see log)"));
                }
            }
        }
        if ctx.fail > 0 {
            ctx.say("  staging failed, nothing flashed");
            return false;
        }
        src_desc = format!("backup/{shown}");
    }
    ctx.say("");
    ctx.say(&format!("  report ({src_desc}), details in log:"));
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
        let psz: u64 = fb::getvar(&ctx.cfg.tools.fastboot, &ctx.serial, &format!("partition-size:{part}"), &mut ctx.log)
            .and_then(|v| {
                let h = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")).unwrap_or(&v);
                u64::from_str_radix(h, 16).ok()
            })
            .unwrap_or(0);
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
            ctx.say(&format!("  slot {s}: {} / {}, free {} — DOES NOT FIT", mb(isz), mb(psz), mb(free_u)));
            ctx.bad(&format!("slot {s}: image does not fit partition"));
        } else if !ctx.force && (free as u64) < ctx.cfg.min_free {
            ctx.say(&format!("  slot {s}: {} / {}, free {} — BELOW {} MB POLICY", mb(isz), mb(psz), mb(free_u), ctx.cfg.min_free_mb));
            ctx.bad(&format!("slot {s}: less than {} MB free left in partition", ctx.cfg.min_free_mb));
            ctx.policy_fail = true;
        } else if ctx.force && (free as u64) < ctx.cfg.min_free {
            ctx.say(&format!("  slot {s}: {} / {}, free {} — OK (--force: {} MB policy waived)", mb(isz), mb(psz), mb(free_u), ctx.cfg.min_free_mb));
            let _ = writeln!(ctx.log, "  ({} MB free-space policy waived under --force)", ctx.cfg.min_free_mb);
        } else {
            ctx.say(&format!("  slot {s}: {} / {}, free {} — OK", mb(isz), mb(psz), mb(free_u)));
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
            ctx.say("  !! Please send backup/<stamp>/install.log to the OrangeFox Pixel group:");
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
    let stages = build_stages(plan, ctx.force, &ctx.mode);
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
    match menu("Flash these images?", 1, &opts).as_deref() {
        Ok("Yes, flash") => Ok(true),
        Ok(c) if c == BACK_ITEM => Err(last.unwrap_or(0)),
        _ => Ok(false),
    }
}

/// [5/5] flash + fetch-back proof. False = exit 2.
fn stage_flash(ctx: &mut Ctx) -> bool {
    ctx.step("[5/5] flash");
    for s in ctx.slots.clone() {
        let part = format!("vendor_boot_{s}");
        let img = ctx.work.join(format!("flash_{s}.img"));
        match fb::flash(&ctx.cfg.tools.fastboot, &ctx.serial, &part, &img, &mut ctx.log) {
            Ok(()) => ctx.ok(&format!("vendor_boot_{s} flashed")),
            Err(_) => ctx.bad(&format!("flash vendor_boot_{s} (see log)")),
        }
    }
    if ctx.fail > 0 {
        ctx.say(&format!("  flash failed (device left in bootloader, stock: backup/{}/)", ctx.stamp));
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
    macro_rules! say {
        ($($t:tt)*) => {{
            let line = format!($($t)*);
            println!("{line}");
            if let Some(f) = log.as_mut() {
                let _ = writeln!(f, "{line}");
            }
        }};
    }
    macro_rules! detail {
        ($t:expr) => {{
            let text: String = $t;
            eprintln!("{text}");
            if let Some(f) = log.as_mut() {
                let _ = writeln!(f, "{text}");
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
    say!("  rebuild ({label}, footer dropped)");
    let opts = RepackOpts { mode: Mode::Keep, drop: Vec::new(), sets: Vec::new(), recovery: Some((label, payload)), drop_footer: true };
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
    say!("RESULT: OK");
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
    let backup = cfg.backup_dir.join(&stamp);
    if let Err(e) = std::fs::create_dir_all(&backup) {
        eprintln!("cannot create {}: {e}", backup.display());
        return 1;
    }
    let log_path = backup.join("install.log");
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
        stamp,
        backup,
        pass: 0,
        fail: 0,
        policy_fail: false,
        force: cli.force,
        serial: String::new(),
        mode: String::new(),
        slots: Vec::new(),
        restore_src: None,
        work,
    };

    ctx.say("OrangeFox vendor_boot installer");
    if !stage_device(&mut ctx) {
        return 1;
    }

    // --- [2/5] static plan: flags/force known upfront ---
    let backups = list_backups(&ctx.cfg.backup_dir);
    if !cli.mode.is_empty() {
        ctx.mode = cli.mode.clone();
        ctx.say(&format!("  mode: {} (flag)", ctx.mode));
    }
    if !cli.slot.is_empty() {
        ctx.slots = if cli.slot == "both" { vec!["a".to_string(), "b".to_string()] } else { vec![cli.slot.clone()] };
        ctx.say(&format!("  slots: {} (flag)", ctx.slots.join(" ")));
    }
    if cli.force {
        if ctx.mode.is_empty() {
            ctx.mode = "install".to_string();
            ctx.say("  mode: install (--force)");
        }
        if ctx.slots.is_empty() {
            ctx.slots = vec!["a".to_string(), "b".to_string()];
            ctx.say("  slots: a b (--force)");
        }
    }
    if ctx.mode.is_empty() && backups.is_empty() {
        ctx.mode = "install".to_string();
        ctx.say("  mode: install (no backups stored yet)");
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
            ctx.say(&format!("  restore from: backup/{shown} (flag)"));
        } else if cli.force {
            ctx.say("  restore needs --backup in --force mode");
            return 1;
        }
    }
    let plan = StagePlan {
        mode_menu: cli.mode.is_empty() && !cli.force && !backups.is_empty() && ctx.mode.is_empty(),
        slot_menu: cli.slot.is_empty() && !cli.force,
        backup_menu: cli.backup.is_empty() && !cli.force,
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
        ctx.say(&format!("RESULT: FAIL (pass={} fail={}; see backup/{}/install.log)", ctx.pass, ctx.fail, ctx.stamp));
        return 2;
    }
    ctx.say("");
    if ctx.fail == 0 {
        ctx.say(&format!("RESULT: OK (pass={}; stock: backup/{}/; log: backup/{}/install.log)", ctx.pass, ctx.stamp, ctx.stamp));
        0
    } else {
        ctx.say(&format!("RESULT: FAIL (pass={} fail={}; see backup/{}/install.log)", ctx.pass, ctx.fail, ctx.stamp));
        2
    }
}
