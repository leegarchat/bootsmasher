//! One-way cpio-to-directory extraction for `unpack --extract`.
//!
//! Restores files, dirs and symlinks (+ unix permission bits); device
//! nodes, sockets and fifos are skipped with a reason. Absolute paths
//! and `..` are refused (zip-slip style) without aborting the run.

use std::path::{Component, Path, PathBuf};

use crate::common::error::{Error, Result};
use crate::common::cpio::{self, name_str};

#[derive(Debug, Default)]
pub struct ExtractReport {
    pub files: u32,
    pub dirs: u32,
    pub symlinks: u32,
    pub skipped: Vec<String>,
}

fn safe_join(root: &Path, name: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for comp in Path::new(name).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                return Err(Error::Parse(format!("refusing unsafe cpio path '{name}'")));
            }
        }
    }
    Ok(out)
}

pub fn extract(blob: &[u8], dest: &Path) -> Result<ExtractReport> {
    let entries = cpio::parse(blob)?;
    let mut rep = ExtractReport::default();
    for e in &entries {
        let name = name_str(e);
        if name == "TRAILER!!!" {
            continue;
        }
        let path = match safe_join(dest, &name) {
            Ok(p) => p,
            Err(err) => {
                rep.skipped.push(format!("{name}: {err}"));
                continue;
            }
        };
        let mode = cpio::entry_mode(e).unwrap_or(0o100644);
        match mode & 0o170000 {
            0o040000 => {
                std::fs::create_dir_all(&path)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o7777));
                }
                rep.dirs += 1;
            }
            0o120000 => {
                let target = String::from_utf8_lossy(&e.data).into_owned();
                if let Some(par) = path.parent() {
                    std::fs::create_dir_all(par)?;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::symlink as symlink_impl;
                    match symlink_impl(&target, &path) {
                        Ok(()) => rep.symlinks += 1,
                        Err(err) => rep.skipped.push(format!("{name}: symlink failed: {err}")),
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = target;
                    rep.skipped.push(format!("{name}: symlinks not restored on this OS"));
                }
            }
            0o100000 | 0 => {
                if let Some(par) = path.parent() {
                    std::fs::create_dir_all(par)?;
                }
                std::fs::write(&path, &e.data)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o7777));
                }
                rep.files += 1;
            }
            other => {
                rep.skipped.push(format!("{name}: special file type {other:#o} not restored"));
            }
        }
    }
    Ok(rep)
}
