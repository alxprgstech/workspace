//! Free-space queries for the update preflight.

use crate::OsError;
use std::path::Path;

/// Bytes available to the current user on the volume that holds `path`.
pub fn free_bytes(path: &Path) -> Result<u64, OsError> {
    imp::free_bytes(path)
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            free_total: *mut u64,
        ) -> i32;
    }

    pub fn free_bytes(path: &Path) -> Result<u64, OsError> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut available = 0u64;
        let mut total = 0u64;
        let mut free = 0u64;

        // SAFETY: the path is a NUL-terminated UTF-16 buffer and the three
        // out-pointers are valid for the duration of the call.
        let ok =
            unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) };

        if ok == 0 {
            return Err(OsError::System(format!(
                "cannot read free space for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }

        Ok(available)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn free_bytes(path: &Path) -> Result<u64, OsError> {
        let output = std::process::Command::new("df")
            .arg("-Pk")
            .arg(path)
            .output()
            .map_err(|error| OsError::System(error.to_string()))?;

        if !output.status.success() {
            return Err(OsError::System("df failed".into()));
        }

        let text = String::from_utf8_lossy(&output.stdout);
        let line = text
            .lines()
            .nth(1)
            .ok_or_else(|| OsError::System("df output is empty".into()))?;
        let available_kib: u64 = line
            .split_whitespace()
            .nth(3)
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| OsError::System("cannot parse df output".into()))?;

        Ok(available_kib.saturating_mul(1024))
    }
}
