//! Process inspection used to stop only the Workspace server this
//! installation owns.

use crate::OsError;
use std::path::PathBuf;

/// Whether a process with this id is currently running.
pub fn is_alive(pid: u32) -> bool {
    imp::is_alive(pid)
}

/// Full image path of a running process, when it can be read.
pub fn image_path(pid: u32) -> Option<PathBuf> {
    imp::image_path(pid)
}

/// Terminate one process (not the whole tree). Returns whether it was alive.
pub fn terminate(pid: u32) -> Result<bool, OsError> {
    imp::terminate(pid)
}

/// Wait until the process exits or the timeout elapses.
pub fn wait_for_exit(pid: u32, timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;

    while std::time::Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    !is_alive(pid)
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStringExt;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const STILL_ACTIVE: u32 = 259;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> *mut c_void;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetExitCodeProcess(handle: *mut c_void, code: *mut u32) -> i32;
        fn TerminateProcess(handle: *mut c_void, code: u32) -> i32;
        fn QueryFullProcessImageNameW(
            handle: *mut c_void,
            flags: u32,
            buffer: *mut u16,
            size: *mut u32,
        ) -> i32;
    }

    struct Handle(*mut c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle was returned by OpenProcess and is owned here.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn open(pid: u32, access: u32) -> Option<Handle> {
        // SAFETY: plain FFI call with no pointers.
        let handle = unsafe { OpenProcess(access, 0, pid) };

        if handle.is_null() {
            None
        } else {
            Some(Handle(handle))
        }
    }

    pub fn is_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }

        let Some(handle) = open(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
            return false;
        };

        let mut code = 0u32;

        // SAFETY: the handle is open for query and `code` is a valid out-pointer.
        if unsafe { GetExitCodeProcess(handle.0, &mut code) } == 0 {
            return false;
        }

        code == STILL_ACTIVE
    }

    pub fn image_path(pid: u32) -> Option<PathBuf> {
        if pid == 0 {
            return None;
        }

        let handle = open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;

        let mut buffer = vec![0u16; 32_768];
        let mut size = buffer.len() as u32;

        // SAFETY: the buffer is valid for `size` UTF-16 units.
        let ok = unsafe { QueryFullProcessImageNameW(handle.0, 0, buffer.as_mut_ptr(), &mut size) };

        if ok == 0 {
            return None;
        }

        buffer.truncate(size as usize);

        Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
    }

    pub fn terminate(pid: u32) -> Result<bool, OsError> {
        if !is_alive(pid) {
            return Ok(false);
        }

        let Some(handle) = open(pid, PROCESS_TERMINATE) else {
            return Err(OsError::System(format!(
                "cannot open process {pid}: {}",
                std::io::Error::last_os_error()
            )));
        };

        // SAFETY: the handle has PROCESS_TERMINATE access.
        let ok = unsafe { TerminateProcess(handle.0, 1) };

        if ok == 0 {
            return Err(OsError::System(format!(
                "cannot terminate process {pid}: {}",
                std::io::Error::last_os_error()
            )));
        }

        Ok(true)
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn is_alive(pid: u32) -> bool {
        pid != 0 && std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    pub fn image_path(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }

    pub fn terminate(pid: u32) -> Result<bool, OsError> {
        if !is_alive(pid) {
            return Ok(false);
        }

        let status = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .map_err(|error| OsError::System(error.to_string()))?;

        Ok(status.success())
    }
}
