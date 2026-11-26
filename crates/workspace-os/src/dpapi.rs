//! Windows DPAPI wrappers.
//!
//! `CryptProtectData` binds the ciphertext to the current Windows user: only
//! that user (or an administrator with the same profile loaded) can decrypt it.
//! The updater uses it for the SSO client secret and the session encryption
//! key so an update never has to ask for them again. Plaintext never leaves the
//! process and is never logged.

use crate::OsError;

/// Encrypt `plaintext` for the current Windows user.
pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, OsError> {
    if plaintext.is_empty() {
        return Err(OsError::Invalid("empty secret".into()));
    }

    imp::protect(plaintext)
}

/// Decrypt a blob produced by [`protect`] for the current Windows user.
pub fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, OsError> {
    if ciphertext.is_empty() {
        return Err(OsError::Invalid("empty protected secret".into()));
    }

    imp::unprotect(ciphertext)
}

/// Whether DPAPI protection is available in this process.
pub fn is_supported() -> bool {
    imp::SUPPORTED
}

#[cfg(windows)]
mod imp {
    use crate::OsError;
    use std::ffi::c_void;
    use std::ptr;

    pub const SUPPORTED: bool = true;

    const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    unsafe extern "system" {
        fn CryptProtectData(
            data_in: *const DataBlob,
            description: *const u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;

        fn CryptUnprotectData(
            data_in: *const DataBlob,
            description: *mut *mut u16,
            entropy: *const DataBlob,
            reserved: *mut c_void,
            prompt: *mut c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    fn last_error() -> String {
        std::io::Error::last_os_error().to_string()
    }

    fn run(input: &[u8], protect: bool) -> Result<Vec<u8>, OsError> {
        let input_blob = DataBlob {
            cb_data: u32::try_from(input.len())
                .map_err(|_| OsError::Invalid("secret is too large".into()))?,
            pb_data: input.as_ptr().cast_mut(),
        };

        let mut output_blob = DataBlob {
            cb_data: 0,
            pb_data: ptr::null_mut(),
        };

        // SAFETY: both blobs point at valid slices/owned output for the whole
        // call; optional pointers are null; DPAPI writes into `output_blob`.
        let ok = unsafe {
            if protect {
                CryptProtectData(
                    &input_blob,
                    ptr::null(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output_blob,
                )
            } else {
                CryptUnprotectData(
                    &input_blob,
                    ptr::null_mut(),
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output_blob,
                )
            }
        };

        if ok == 0 {
            return Err(OsError::System(last_error()));
        }

        // SAFETY: on success DPAPI returned an owned buffer of cb_data bytes.
        let output = unsafe {
            let slice =
                std::slice::from_raw_parts(output_blob.pb_data, output_blob.cb_data as usize);
            let copy = slice.to_vec();
            LocalFree(output_blob.pb_data.cast::<c_void>());
            copy
        };

        if output.is_empty() {
            return Err(OsError::System("DPAPI returned an empty result".into()));
        }

        Ok(output)
    }

    pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, OsError> {
        run(plaintext, true)
    }

    pub fn unprotect(ciphertext: &[u8]) -> Result<Vec<u8>, OsError> {
        run(ciphertext, false)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn roundtrip_keeps_the_secret() {
            let secret = b"client-secret-and-aes-key";

            let protected = protect(secret).unwrap();

            assert_ne!(protected, secret);
            assert_eq!(unprotect(&protected).unwrap(), secret);
        }

        #[test]
        fn rejects_foreign_ciphertext() {
            assert!(unprotect(b"not a dpapi blob").is_err());
        }

        #[test]
        fn rejects_empty_input() {
            assert!(crate::dpapi::protect(b"").is_err());
            assert!(crate::dpapi::unprotect(b"").is_err());
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use crate::OsError;

    pub const SUPPORTED: bool = false;

    pub fn protect(_: &[u8]) -> Result<Vec<u8>, OsError> {
        Err(OsError::Unsupported)
    }

    pub fn unprotect(_: &[u8]) -> Result<Vec<u8>, OsError> {
        Err(OsError::Unsupported)
    }
}
