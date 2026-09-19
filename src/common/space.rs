//! Free-space accounting: human/byte size parsing and the pre-write check.
//!
//! Unix uses `statvfs` (f_bavail, i.e. what an unprivileged writer really
//! gets). Windows calls `GetDiskFreeSpaceExW` via a hand-rolled `extern`
//! block — kernel32 is OS-provided, no extra crates needed on either side.

use std::path::Path;

use crate::common::error::{Error, Result};

/// Parse `67108864`, `512M`, `1GiB`, `1.5G` (case-insensitive, optional
/// trailing B, K/M/G/T are binary: 1K = 1024).
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    if t.is_empty() {
        return Err(Error::Usage("--min-free needs a value like 512M or 67108864".to_string()));
    }
    let cut = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, suf) = t.split_at(cut);
    if num.is_empty() {
        return Err(Error::Usage(format!("bad size '{s}': need a number first")));
    }
    let val: f64 = num
        .parse()
        .map_err(|_| Error::Usage(format!("bad size '{s}': '{num}' is not a number")))?;
    if !val.is_finite() || val < 0.0 {
        return Err(Error::Usage(format!("bad size '{s}': must be >= 0")));
    }
    let mult: f64 = match suf.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return Err(Error::Usage(format!("bad size '{s}': unknown suffix '{suf}' (want K/M/G/T)"))),
    };
    let bytes = val * mult;
    if bytes > u64::MAX as f64 {
        return Err(Error::Usage(format!("bad size '{s}': too large")));
    }
    Ok(bytes as u64)
}

#[cfg(unix)]
pub fn free_bytes(dir: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let cstr = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| Error::Usage(format!("bad path: {}", dir.display())))?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs writes exactly one struct to our own initialized slot.
    if unsafe { libc::statvfs(cstr.as_ptr(), &mut st) } != 0 {
        return Err(Error::Io(format!(
            "cannot stat free space for {}: {}",
            dir.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetDiskFreeSpaceExW(
        lp_directory_name: *const u16,
        lp_free_bytes_available: *mut u64,
        lp_total_number_of_bytes: *mut u64,
        lp_total_number_of_free_bytes: *mut u64,
    ) -> i32;
}

#[cfg(windows)]
pub fn free_bytes(dir: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut avail: u64 = 0;
    // SAFETY: wide is NUL-terminated; avail is our own u64 slot.
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut())
    };
    if ok == 0 {
        return Err(Error::Io(format!("cannot stat free space for {}", dir.display())));
    }
    Ok(avail)
}

#[cfg(not(any(unix, windows)))]
pub fn free_bytes(dir: &Path) -> Result<u64> {
    Err(Error::Io(format!("free-space check unsupported on {}", dir.display())))
}

/// Refuse to write `image_len` bytes into `dir` unless
/// `free >= image_len + min_free`. Reports real numbers on failure.
pub fn ensure_space(dir: &Path, image_len: u64, min_free: u64) -> Result<()> {
    let free = free_bytes(dir)?;
    let need = image_len.saturating_add(min_free);
    if free < need {
        return Err(Error::Verify(format!(
            "not enough free space in {}: image {} bytes + reserve {} bytes = {} needed, {} available",
            dir.display(),
            image_len,
            min_free,
            need,
            free
        )));
    }
    Ok(())
}
