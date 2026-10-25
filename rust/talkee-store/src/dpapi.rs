//! Protecting a stored credential so only this user on this machine can read it.
//!
//! # Why DPAPI rather than a file permission
//!
//! The pairing key is a credential: whoever holds it can speak through this microphone. A file in
//! the user's profile is readable by *any* process running as that user — which includes every
//! other program they have ever installed, and every script that runs at login. DPAPI ties the
//! ciphertext to the user's login credentials, so a copy taken off the machine cannot be decrypted
//! at all, and reading it in place means being that user on that machine.
//!
//! `docs/PROTOCOL.md` states this as the rule for both ends — *"stored in the platform keystore
//! (Windows DPAPI / Android Keystore) — never in a plain config file"*. This is that rule.
//!
//! # Why this file carries `unsafe`
//!
//! `CryptProtectData` is the API for the job and there is no wrapper worth adding a dependency for
//! two calls. The crate root carries `#![deny(unsafe_code)]`, so the exception is asked for here,
//! explicitly, and kept to the FFI — the same shape as the desktop app's `message_pump` and
//! `shell` modules.
#![allow(unsafe_code)]

use anyhow::{bail, Result};

/// Encrypts `plain` so only this user on this machine can decrypt it.
#[cfg(windows)]
pub fn protect(plain: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    // CRYPTPROTECT_UI_FORBIDDEN: a stored key must never put a dialog on screen. A background
    // process that can be blocked on a prompt is a background process that hangs.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        bail!("Windows would not encrypt the pairing key");
    }

    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    // DPAPI allocated this buffer, so DPAPI's allocator has to be the one that frees it. Not
    // freeing it leaks a key-sized allocation on every start.
    unsafe { LocalFree(output.pbData as *mut core::ffi::c_void) };
    Ok(bytes)
}

/// Decrypts a blob produced by [`protect`] for this user on this machine.
#[cfg(windows)]
pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        // Deliberately not "corrupt file": the most likely cause is that the file was written by a
        // different user or on a different machine, and that is worth saying out loud.
        bail!("Windows could not decrypt the saved pairing (wrong user or machine?)");
    }

    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe { LocalFree(output.pbData as *mut core::ffi::c_void) };
    Ok(bytes)
}

/// There is no keystore here, and pretending otherwise would write a credential in the clear.
#[cfg(not(windows))]
pub fn protect(_plain: &[u8]) -> Result<Vec<u8>> {
    bail!("this build has no platform keystore, and a pairing key is not written unencrypted")
}

/// See [`protect`].
#[cfg(not(windows))]
pub fn unprotect(_blob: &[u8]) -> Result<Vec<u8>> {
    bail!("this build has no platform keystore")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_blob_round_trips() {
        let secret = b"thirty-two bytes of pairing key..";
        let blob = protect(secret).expect("DPAPI should encrypt this");
        assert_ne!(blob.as_slice(), secret.as_slice(), "the blob is the plaintext");
        let back = unprotect(&blob).expect("and decrypt it again");
        assert_eq!(back, secret);
    }

    #[test]
    fn the_ciphertext_does_not_contain_the_plaintext() {
        // The whole point of using DPAPI instead of a file permission. A `protect` that returned
        // its input would pass a round-trip test and fail this one, which is why both exist.
        let secret = b"SENTINEL-PAIRING-KEY-MATERIAL";
        let blob = protect(secret).unwrap();
        let leaks = blob
            .windows(secret.len())
            .any(|window| window == secret.as_slice());
        assert!(!leaks, "the plaintext is visible inside the stored blob");
    }

    #[test]
    fn a_tampered_blob_is_an_error_not_a_panic() {
        // A corrupt or truncated file must be recovered from, not crashed on: the host has to be
        // able to say "this pairing could not be read, make a new one" and carry on.
        let mut blob = protect(b"key").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0xFF;
        assert!(unprotect(&blob).is_err(), "a damaged blob must be rejected");
        assert!(unprotect(b"").is_err());
    }
}
