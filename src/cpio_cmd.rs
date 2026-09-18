//! `bootsmasher cpio` — faithful port of magiskboot's cpio subcommand.
//!
//! Same grammar, same semantics, same exit codes: in-place newc archive
//! surgery (`exists/ls/rm/mkdir/ln/mv/add/extract/test/patch/backup/
//! restore`), BTreeMap-backed entries, inode renumbering from 300000 on
//! dump. Pure Rust, no C code; Unix-only bits (mknod, device rdev,
//! permission modes) are cfg-gated so Windows targets still build.

use std::collections::BTreeMap;
use std::path::Path;

use crate::codec::{self, Format};
use crate::error::{Error, Result};

const HELP: &str = "bootsmasher cpio — newc archive surgery (magiskboot port)
Alias: c.

Usage:
  bootsmasher cpio <incpio> [commands...]
  bootsmasher cpio --help

Each command is a single argument; quote it in the shell.
Modifications are done in-place: the file is rewritten after the last
command (except ls/test/exists, which report and exit without writing).
A missing <incpio> starts an empty archive. Input must be a raw newc
cpio (070701) — unpack without -n first, or decompress the .lz4.

Supported commands (magiskboot parity):
  exists ENTRY
    Exit 0 if ENTRY exists, else 1 (nothing is written).
  ls [-r] [PATH]
    List PATH (\"/\" by default); -r lists recursively. Prints
    '<mode> <uid> <gid> <size> <rdev>\\t<name>' per entry to stdout.
  rm [-r] ENTRY
    Remove ENTRY; -r removes the whole subtree.
  mkdir MODE ENTRY
    Create directory ENTRY with octal permissions MODE (e.g. 755).
  ln TARGET ENTRY
    Create a symlink to TARGET named ENTRY.
  mv SOURCE DEST
    Move (rename) SOURCE to DEST. Error if SOURCE is missing.
  add MODE ENTRY INFILE
    Add host file INFILE as ENTRY with octal MODE; replaces ENTRY if it
    exists. Symlinks are stored as regular files (use ln for links);
    block/char devices keep their device numbers; anything else is an
    error. ENTRY must not end with '/'.
  extract [ENTRY OUT]
    Extract ENTRY to OUT (parents created, unix modes applied); with no
    args extracts every entry into the current directory.
  test
    Exit code only, nothing written:
    0 = stock, 1 = Magisk-patched, 2 = unsupported (SuperSU/xposed).
  patch
    Apply ramdisk patches: strip verify/avb/forceencrypt flags from
    fstab files (honors KEEPVERITY / KEEPFORCEENCRYPT env, \"true\" keeps).
  backup ORIG [-n]
    Diff ORIG against <incpio>: changed/removed ORIG entries land in
    .backup/ (xz-compressed unless -n), new entries go to .backup/.rmlist.
  restore
    Restore from the embedded .backup (xz entries decompressed).

Exit codes: 0 ok (test: 0 stock / 1 Magisk; exists: 0 found),
1 usage error (also: exists missing, test Magisk/unsupported),
2 broken input (bad magic/header, unreadable file).";

// newc file-type bits (same values as libc S_IF*).
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;
const S_IFBLK: u32 = 0o060000;
const S_IFCHR: u32 = 0o020000;

const HDR_LEN: usize = 110;

#[derive(Debug, Clone)]
struct CpioEntry {
    mode: u32,
    uid: u32,
    gid: u32,
    rdevmajor: u64,
    rdevminor: u64,
    data: Vec<u8>,
}

struct Cpio {
    entries: BTreeMap<String, CpioEntry>,
}

fn align4(x: usize) -> usize {
    (x + 3) & !3
}

fn norm_path(path: &str) -> String {
    path.split('/').filter(|x| !x.is_empty()).collect::<Vec<_>>().join("/")
}

fn parse_mode(s: &str) -> Result<u32> {
    u32::from_str_radix(s.trim(), 8)
        .map_err(|_| Error::Usage(format!("bad MODE '{s}' (want octal, e.g. 755)")))
}

fn hex8(b: &[u8]) -> Result<u32> {
    if b.len() != 8 {
        return Err(Error::Parse("bad cpio header (truncated field)".to_string()));
    }
    let s = std::str::from_utf8(b).map_err(|_| Error::Parse("bad cpio header (not ASCII)".to_string()))?;
    let mut ret = 0u32;
    for c in s.chars() {
        let d = c.to_digit(16).ok_or_else(|| Error::Parse("bad cpio header (not hex)".to_string()))?;
        ret = ret.saturating_mul(16).saturating_add(d);
    }
    Ok(ret)
}

fn human_size(n: usize) -> String {
    // Base-10 abbreviated, mirroring magiskboot's Size display.
    const UNITS: &[&str] = &["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u + 1 < UNITS.len() {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n}B")
    } else if v >= 100.0 {
        format!("{v:.0}{}", UNITS[u])
    } else {
        format!("{v:.1}{}", UNITS[u])
    }
}

impl Cpio {
    fn new() -> Cpio {
        Cpio { entries: BTreeMap::new() }
    }

    fn load_from_data(data: &[u8]) -> Result<Cpio> {
        let mut cpio = Cpio::new();
        let mut pos = 0usize;
        while pos < data.len() {
            if data.len() - pos < HDR_LEN {
                return Err(Error::Parse("invalid cpio magic (truncated header)".to_string()));
            }
            if &data[pos..pos + 6] != b"070701" {
                return Err(Error::Parse("invalid cpio magic (want 070701)".to_string()));
            }
            let mode = hex8(&data[pos + 14..pos + 22])?;
            let uid = hex8(&data[pos + 22..pos + 30])?;
            let gid = hex8(&data[pos + 30..pos + 38])?;
            let filesize = hex8(&data[pos + 54..pos + 62])? as usize;
            let rdevmajor = hex8(&data[pos + 70..pos + 78])? as u64;
            let rdevminor = hex8(&data[pos + 78..pos + 86])? as u64;
            let namesize = hex8(&data[pos + 94..pos + 102])? as usize;
            pos += HDR_LEN;
            if data.len() - pos < namesize {
                return Err(Error::Parse("invalid cpio (truncated name)".to_string()));
            }
            let raw_name = &data[pos..pos + namesize];
            let end = raw_name.iter().position(|&b| b == 0).unwrap_or(raw_name.len());
            let name = String::from_utf8_lossy(&raw_name[..end]).into_owned();
            pos += namesize;
            pos = align4(pos);
            if name == "." || name == ".." {
                continue;
            }
            if name == "TRAILER!!!" {
                // Concatenated archives: skip to the next magic.
                match data[pos..].windows(6).position(|w| w == b"070701") {
                    Some(x) => pos += x,
                    None => break,
                }
                continue;
            }
            if data.len() - pos < filesize {
                return Err(Error::Parse(format!("invalid cpio (truncated data of '{name}')")));
            }
            cpio.entries.insert(
                name,
                CpioEntry {
                    mode,
                    uid,
                    gid,
                    rdevmajor,
                    rdevminor,
                    data: data[pos..pos + filesize].to_vec(),
                },
            );
            pos += filesize;
            pos = align4(pos);
        }
        Ok(cpio)
    }

    fn load_from_file(path: &Path) -> Result<Cpio> {
        eprintln!("Loading cpio: [{}]", path.display());
        let data = std::fs::read(path).map_err(|e| Error::Io(format!("cannot read {}: {e}", path.display())))?;
        Cpio::load_from_data(&data)
    }

    fn dump(&self, path: &Path) -> Result<()> {
        eprintln!("Dumping cpio: [{}]", path.display());
        let mut out = Vec::new();
        let mut pos = 0usize;
        let mut inode: u32 = 300000;
        for (name, e) in &self.entries {
            let head = format!(
                "070701{inode:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
                e.mode,
                e.uid,
                e.gid,
                1u32,
                0u32,
                e.data.len() as u32,
                0u32,
                0u32,
                e.rdevmajor as u32,
                e.rdevminor as u32,
                name.len() as u32 + 1,
                0u32
            );
            out.extend_from_slice(head.as_bytes());
            pos += head.len();
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            pos += name.len() + 1;
            while pos % 4 != 0 {
                out.push(0);
                pos += 1;
            }
            out.extend_from_slice(&e.data);
            pos += e.data.len();
            while pos % 4 != 0 {
                out.push(0);
                pos += 1;
            }
            inode = inode.wrapping_add(1);
        }
        let head = format!("070701{inode:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}", 0o755u32, 0u32, 0u32, 1u32, 0u32, 0u32, 0u32, 0u32, 0u32, 0u32, 11u32, 0u32);
        out.extend_from_slice(head.as_bytes());
        pos += head.len();
        out.extend_from_slice(b"TRAILER!!!\0");
        pos += 11;
        while pos % 4 != 0 {
            out.push(0);
            pos += 1;
        }
        std::fs::write(path, &out).map_err(|e| Error::Io(format!("cannot write {}: {e}", path.display())))?;
        Ok(())
    }

    fn rm(&mut self, path: &str, recursive: bool) {
        let path = norm_path(path);
        if self.entries.remove(&path).is_some() {
            eprintln!("Removed entry [{path}]");
        }
        if recursive {
            let prefix = path + "/";
            let doomed: Vec<String> =
                self.entries.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
            for k in doomed {
                eprintln!("Removed entry [{k}]");
                self.entries.remove(&k);
            }
        }
    }

    fn extract_entry(&self, path: &str, out: &str) -> Result<()> {
        let entry = self
            .entries
            .get(path)
            .ok_or_else(|| Error::Parse(format!("extract: no such entry '{path}'")))?;
        eprintln!("Extracting entry [{path}] to [{out}]");
        let out_path = Path::new(out);
        if let Some(dir) = out_path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let mode = entry.mode & 0o777;
        match entry.mode & S_IFMT {
            S_IFDIR => {
                std::fs::create_dir_all(out_path)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(out_path, std::fs::Permissions::from_mode(mode))?;
                }
            }
            S_IFREG => {
                std::fs::write(out_path, &entry.data)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(out_path, std::fs::Permissions::from_mode(mode))?;
                }
            }
            S_IFLNK => {
                let target = String::from_utf8_lossy(&entry.data).into_owned();
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&target, out_path)?;
                }
                #[cfg(not(unix))]
                {
                    // Best effort: materialize the link target path as a file
                    // holding the target text.
                    let _ = target;
                    return Err(Error::Io(format!("extract: symlinks unsupported on this OS ('{path}')")));
                }
            }
            S_IFBLK | S_IFCHR => {
                #[cfg(unix)]
                {
                    // SAFETY: plain integer conversion for mknod(2).
                    unsafe {
                        if libc::mknod(
                            std::ffi::CString::new(out.as_bytes())
                                .map_err(|_| Error::Parse(format!("bad path '{out}'")))?
                                .as_ptr(),
                            entry.mode as libc::mode_t,
                            libc::makedev(entry.rdevmajor as u32, entry.rdevminor as u32),
                        ) != 0
                        {
                            return Err(Error::Io(format!(
                                "extract: mknod failed for '{out}': {}",
                                std::io::Error::last_os_error()
                            )));
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    return Err(Error::Io(format!("extract: device nodes unsupported on this OS ('{path}')")));
                }
            }
            _ => return Err(Error::Parse(format!("extract: unknown entry type of '{path}'"))),
        }
        Ok(())
    }

    fn extract(&self, path: Option<&str>, out: Option<&str>) -> Result<()> {
        match (path, out) {
            (Some(p), Some(o)) => self.extract_entry(&norm_path(p), o),
            _ => {
                for name in self.entries.keys().cloned().collect::<Vec<_>>() {
                    if name == "." || name == ".." {
                        continue;
                    }
                    self.extract_entry(&name, &name)?;
                }
                Ok(())
            }
        }
    }

    fn exists(&self, path: &str) -> bool {
        self.entries.contains_key(&norm_path(path))
    }

    fn add(&mut self, mode: u32, path: &str, file: &str) -> Result<()> {
        if path.ends_with('/') {
            return Err(Error::Parse("add: path cannot end with /".to_string()));
        }
        let meta = std::fs::symlink_metadata(file)
            .map_err(|e| Error::Io(format!("add: cannot stat {file}: {e}")))?;
        let ft = meta.file_type();
        let (rdevmajor, rdevminor, fmode, data) = if ft.is_file() || ft.is_symlink() {
            // Symlinks are stored as regular files (use ln for real links).
            let data = std::fs::read(file).map_err(|e| Error::Io(format!("add: cannot read {file}: {e}")))?;
            (0u64, 0u64, mode | S_IFREG, data)
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
                if ft.is_block_device() || ft.is_char_device() {
                    let rdev = meta.rdev();
                    let (maj, min) = (libc::major(rdev) as u64, libc::minor(rdev) as u64);
                    let fmode = if ft.is_block_device() { mode | S_IFBLK } else { mode | S_IFCHR };
                    (maj, min, fmode, Vec::new())
                } else {
                    return Err(Error::Parse(format!("add: unsupported file type '{file}'")));
                }
            }
            #[cfg(not(unix))]
            {
                return Err(Error::Parse(format!("add: unsupported file type '{file}'")));
            }
        };
        self.entries.insert(
            norm_path(path),
            CpioEntry { mode: fmode, uid: 0, gid: 0, rdevmajor, rdevminor, data },
        );
        eprintln!("Add file [{path}] ({mode:04o})");
        Ok(())
    }

    fn mkdir(&mut self, mode: u32, dir: &str) {
        self.entries.insert(
            norm_path(dir),
            CpioEntry { mode: mode | S_IFDIR, uid: 0, gid: 0, rdevmajor: 0, rdevminor: 0, data: vec![] },
        );
        eprintln!("Create directory [{dir}] ({mode:04o})");
    }

    fn ln(&mut self, src: &str, dst: &str) {
        self.entries.insert(
            norm_path(dst),
            CpioEntry {
                mode: S_IFLNK,
                uid: 0,
                gid: 0,
                rdevmajor: 0,
                rdevminor: 0,
                data: norm_path(src).as_bytes().to_vec(),
            },
        );
        eprintln!("Create symlink [{dst}] -> [{src}]");
    }

    fn mv(&mut self, from: &str, to: &str) -> Result<()> {
        let entry = self
            .entries
            .remove(&norm_path(from))
            .ok_or_else(|| Error::Parse(format!("mv: no such entry '{from}'")))?;
        self.entries.insert(norm_path(to), entry);
        eprintln!("Move [{from}] -> [{to}]");
        Ok(())
    }

    fn ls(&self, path: &str, recursive: bool) {
        let base = norm_path(path);
        let base = if base.is_empty() { base } else { "/".to_string() + &base };
        for (name, e) in &self.entries {
            let p = "/".to_string() + name;
            let rest = match p.strip_prefix(&base) {
                Some(r) => r,
                None => continue,
            };
            if !rest.is_empty() && !rest.starts_with('/') {
                continue;
            }
            if !recursive && !rest.is_empty() && rest.matches('/').count() > 1 {
                continue;
            }
            println!("{}\t{name}", entry_line(e));
        }
    }

    fn patch(&mut self) {
        let keep_verity = is_true_env("KEEPVERITY");
        let keep_encrypt = is_true_env("KEEPFORCEENCRYPT");
        eprintln!("Patch with flag KEEPVERITY=[{keep_verity}] KEEPFORCEENCRYPT=[{keep_encrypt}]");
        self.entries.retain(|name, entry| {
            let is_reg = entry.mode & S_IFMT == S_IFREG;
            let fstab = (!keep_verity || !keep_encrypt)
                && is_reg
                && !name.starts_with(".backup")
                && !name.starts_with("twrp")
                && !name.starts_with("recovery")
                && name.starts_with("fstab");
            if !keep_verity {
                if fstab {
                    eprintln!("Found fstab file [{name}]");
                    let len = patch_verity(&mut entry.data);
                    entry.data.truncate(len);
                } else if name == "verity_key" {
                    return false;
                }
            }
            if !keep_encrypt && fstab {
                let len = patch_encryption(&mut entry.data);
                entry.data.truncate(len);
            }
            true
        });
    }

    fn test(&self) -> i32 {
        for f in ["sbin/launch_daemonsu.sh", "sbin/su", "init.xposed.rc", "boot/sbin/launch_daemonsu.sh"] {
            if self.exists(f) {
                return 2;
            }
        }
        for f in [".backup/.magisk", "init.magisk.rc", "overlay/init.magisk.rc"] {
            if self.exists(f) {
                return 1;
            }
        }
        0
    }

    fn restore(&mut self) -> Result<()> {
        let mut backups: BTreeMap<String, CpioEntry> = BTreeMap::new();
        let mut rm_list = String::new();
        let doomed: Vec<String> =
            self.entries.keys().filter(|k| k.starts_with(".backup/")).cloned().collect();
        for name in doomed {
            let mut entry = self.entries.remove(&name).unwrap();
            if name == ".backup/.rmlist" {
                if let Ok(data) = std::str::from_utf8(&entry.data) {
                    rm_list.push_str(data);
                }
            } else if name != ".backup/.magisk" {
                let new_name = if name.ends_with(".xz") && entry_decompress(&mut entry) {
                    name[8..name.len() - 3].to_string()
                } else {
                    name[8..].to_string()
                };
                eprintln!("Restore [{name}] -> [{new_name}]");
                backups.insert(new_name, entry);
            }
        }
        self.rm(".backup", false);
        if rm_list.is_empty() && backups.is_empty() {
            self.entries.clear();
            return Ok(());
        }
        for rm in rm_list.split('\0') {
            if !rm.is_empty() {
                self.rm(rm, false);
            }
        }
        self.entries.extend(backups);
        Ok(())
    }

    fn backup(&mut self, origin_path: &str, skip_compress: bool) -> Result<()> {
        let mut backups: BTreeMap<String, CpioEntry> = BTreeMap::new();
        let mut rm_list = String::new();
        backups.insert(
            ".backup".to_string(),
            CpioEntry { mode: S_IFDIR, uid: 0, gid: 0, rdevmajor: 0, rdevminor: 0, data: vec![] },
        );
        let mut o = Cpio::load_from_file(Path::new(origin_path))?;
        o.rm(".backup", true);
        self.rm(".backup", true);

        // BTreeMap iteration is sorted on both sides, like magiskboot.
        let mut left: Vec<(String, CpioEntry)> = o.entries.into_iter().collect();
        let mut right: Vec<(&String, &CpioEntry)> = self.entries.iter().collect();
        left.reverse();
        right.reverse();
        let mut lhs = left.pop();
        let mut rhs = right.pop();
        loop {
            enum Action {
                Backup(String, CpioEntry),
                Record(String),
                Noop,
            }
            // Move the iterators forward when a side was consumed.
            if lhs.is_none() {
                lhs = left.pop();
            }
            if rhs.is_none() {
                rhs = right.pop();
            }
            let action = match (lhs.take(), rhs.take()) {
                (Some((ln, le)), Some((rn, re))) => match ln.as_str().cmp(rn.as_str()) {
                    std::cmp::Ordering::Less => {
                        rhs = Some((rn, re));
                        Action::Backup(ln, le)
                    }
                    std::cmp::Ordering::Greater => {
                        lhs = Some((ln, le));
                        Action::Record(rn.clone())
                    }
                    std::cmp::Ordering::Equal => {
                        if re.data != le.data {
                            Action::Backup(ln, le)
                        } else {
                            Action::Noop
                        }
                    }
                },
                (Some((ln, le)), None) => Action::Backup(ln, le),
                (None, Some((rn, _))) => Action::Record(rn.clone()),
                (None, None) => break,
            };
            match action {
                Action::Backup(name, mut entry) => {
                    let backup = if !skip_compress && entry_compress(&mut entry) {
                        format!(".backup/{name}.xz")
                    } else {
                        format!(".backup/{name}")
                    };
                    eprintln!("Backup [{name}] -> [{backup}]");
                    backups.insert(backup, entry);
                }
                Action::Record(name) => {
                    eprintln!("Record new entry: [{name}] -> [.backup/.rmlist]");
                    rm_list.push_str(&name);
                    rm_list.push('\0');
                }
                Action::Noop => {}
            }
        }
        if !rm_list.is_empty() {
            backups.insert(
                ".backup/.rmlist".to_string(),
                CpioEntry { mode: S_IFREG, uid: 0, gid: 0, rdevmajor: 0, rdevminor: 0, data: rm_list.as_bytes().to_vec() },
            );
        }
        self.entries.extend(backups);
        Ok(())
    }
}

fn entry_compress(e: &mut CpioEntry) -> bool {
    if e.mode & S_IFMT != S_IFREG {
        return false;
    }
    match codec::compress(Format::Xz, &e.data) {
        Ok(data) => {
            e.data = data;
            true
        }
        Err(err) => {
            eprintln!("xz compression failed: {err}");
            false
        }
    }
}

fn entry_decompress(e: &mut CpioEntry) -> bool {
    if e.mode & S_IFMT != S_IFREG {
        return false;
    }
    match codec::decompress(Format::Xz, &e.data) {
        Ok(data) => {
            e.data = data;
            true
        }
        Err(err) => {
            eprintln!("xz decompression failed: {err}");
            false
        }
    }
}

fn is_true_env(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "true")
}

// ---- fstab verity/encryption patching (magiskboot patch.rs port) ----

fn match_len(buf: &[u8], patterns: &[&[u8]]) -> Option<usize> {
    let mut len = if buf.first() == Some(&b',') { 1 } else { 0 };
    let b = buf.get(len..)?;
    let mut found = false;
    for p in patterns {
        if b.starts_with(p) {
            len += p.len();
            found = true;
            break;
        }
    }
    if !found {
        return None;
    }
    let rest = buf.get(len..)?;
    if rest.first() == Some(&b'=') {
        for c in rest {
            if b" \n,\0".contains(c) {
                break;
            }
            len += 1;
        }
    }
    Some(len)
}

fn remove_pattern(buf: &mut [u8], patterns: &[&[u8]]) -> usize {
    let mut write = 0usize;
    let mut read = 0usize;
    let mut sz = buf.len();
    while read < buf.len() {
        if let Some(len) = match_len(&buf[read..], patterns) {
            if let Ok(skipped) = std::str::from_utf8(&buf[read..read + len]) {
                eprintln!("Remove pattern [{skipped}]");
            }
            sz -= len;
            read += len;
        } else {
            buf[write] = buf[read];
            write += 1;
            read += 1;
        }
    }
    buf[write..].fill(0);
    sz
}

fn patch_verity(buf: &mut [u8]) -> usize {
    remove_pattern(buf, &[b"verifyatboot", b"verify", b"avb_keys", b"avb", b"support_scfs", b"fsverity"])
}

fn patch_encryption(buf: &mut [u8]) -> usize {
    remove_pattern(buf, &[b"forceencrypt", b"forcefdeorfbe", b"fileencryption"])
}

fn entry_line(e: &CpioEntry) -> String {
    let t = match e.mode & S_IFMT {
        S_IFDIR => "d",
        S_IFREG => "-",
        S_IFLNK => "l",
        S_IFBLK => "b",
        S_IFCHR => "c",
        _ => "?",
    };
    let bit = |m: u32| e.mode & m != 0;
    let rwx = |r: u32, w: u32, x: u32| {
        format!("{}{}{}", if bit(r) { "r" } else { "-" }, if bit(w) { "w" } else { "-" }, if bit(x) { "x" } else { "-" })
    };
    format!(
        "{}{}{}{}\t{}\t{}\t{}\t{}:{}",
        t,
        rwx(0o400, 0o200, 0o100),
        rwx(0o040, 0o020, 0o010),
        rwx(0o004, 0o002, 0o001),
        e.uid,
        e.gid,
        human_size(e.data.len()),
        e.rdevmajor,
        e.rdevminor
    )
}

/// Run one `cpio <incpio> [commands...]` invocation.
/// Returns the process exit code directly (test/exists use 1/2 too).
pub fn run(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help") {
        println!("{HELP}");
        return 0;
    }
    match run_inner(args) {
        Ok(code) => code,
        Err(Error::Usage(m)) => {
            eprintln!("usage error: {m}\n{HELP}");
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

fn run_inner(args: &[String]) -> std::result::Result<i32, Error> {
    let mut file: Option<String> = None;
    let mut cmds: Vec<String> = Vec::new();
    for a in args {
        if a == "-h" {
            return Err(Error::Usage(
                "cpio has no -h flag (it would clash with nothing, but magiskboot reserves it); use --help".to_string(),
            ));
        }
        if file.is_none() && !a.starts_with('-') {
            file = Some(a.clone());
        } else {
            cmds.push(a.clone());
        }
    }
    let file = file.ok_or_else(|| Error::Usage("need <incpio>".to_string()))?;
    let path = Path::new(&file);
    let mut cpio = if path.exists() { Cpio::load_from_file(path)? } else { Cpio::new() };

    for cmd in &cmds {
        if cmd.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = cmd.split(' ').filter(|x| !x.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        match parts[0] {
            "test" => {
                if parts.len() != 1 {
                    return Err(Error::Usage("test takes no arguments".to_string()));
                }
                return Ok(cpio.test());
            }
            "restore" => {
                if parts.len() != 1 {
                    return Err(Error::Usage("restore takes no arguments".to_string()));
                }
                cpio.restore()?;
            }
            "patch" => {
                if parts.len() != 1 {
                    return Err(Error::Usage("patch takes no arguments".to_string()));
                }
                cpio.patch();
            }
            "exists" => {
                let p = parts.get(1).ok_or_else(|| Error::Usage("exists needs ENTRY".to_string()))?;
                if parts.len() != 2 {
                    return Err(Error::Usage("exists needs exactly ENTRY".to_string()));
                }
                if cpio.exists(p) {
                    return Ok(0);
                }
                return Err(Error::Fail(format!("exists: entry '{p}' not found")));
            }
            "backup" => {
                // backup ORIG [-n]
                if parts.len() < 2 || parts.len() > 3 {
                    return Err(Error::Usage("backup needs ORIG [-n]".to_string()));
                }
                let skip = parts.get(2).is_some_and(|f| *f == "-n");
                if parts.len() == 3 && !skip {
                    return Err(Error::Usage("backup flag is [-n] only".to_string()));
                }
                cpio.backup(parts[1], skip)?;
            }
            "rm" => {
                // rm [-r] ENTRY
                let (rec, p) = match parts.as_slice() {
                    [_] => return Err(Error::Usage("rm needs ENTRY".to_string())),
                    [_, "-r", p] => (true, *p),
                    [_, p] => (false, *p),
                    _ => return Err(Error::Usage("rm needs [-r] ENTRY".to_string())),
                };
                cpio.rm(p, rec);
            }
            "mv" => {
                if parts.len() != 3 {
                    return Err(Error::Usage("mv needs SOURCE DEST".to_string()));
                }
                cpio.mv(parts[1], parts[2])?;
            }
            "mkdir" => {
                if parts.len() != 3 {
                    return Err(Error::Usage("mkdir needs MODE ENTRY".to_string()));
                }
                cpio.mkdir(parse_mode(parts[1])?, parts[2]);
            }
            "ln" => {
                if parts.len() != 3 {
                    return Err(Error::Usage("ln needs TARGET ENTRY".to_string()));
                }
                cpio.ln(parts[1], parts[2]);
            }
            "add" => {
                if parts.len() != 4 {
                    return Err(Error::Usage("add needs MODE ENTRY INFILE".to_string()));
                }
                cpio.add(parse_mode(parts[1])?, parts[2], parts[3])?;
            }
            "extract" => {
                if parts.len() != 1 && parts.len() != 3 {
                    return Err(Error::Usage("extract needs [ENTRY OUT]".to_string()));
                }
                cpio.extract(parts.get(1).copied(), parts.get(2).copied())?;
            }
            "ls" => {
                // ls [-r] [PATH]
                let (rec, p) = match parts.as_slice() {
                    [_] => (false, "/"),
                    [_, "-r"] => (true, "/"),
                    [_, p] => (false, *p),
                    [_, "-r", p] => (true, *p),
                    _ => return Err(Error::Usage("ls needs [-r] [PATH]".to_string())),
                };
                cpio.ls(p, rec);
                return Ok(0);
            }
            other => return Err(Error::Usage(format!("unknown cpio command '{other}'"))),
        }
    }
    cpio.dump(path)?;
    Ok(0)
}
