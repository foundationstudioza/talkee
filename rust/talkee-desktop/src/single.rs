//! One Talkee at a time, and a way back to the one that is running.
//!
//! # Why this exists
//!
//! Talkee is a tray program, so starting it a second time looks like nothing happening — which is
//! exactly when a person starts it again. Two copies then fight over the same ports: one wins, the
//! other sits in the tray as a second icon that does nothing. A named mutex says "someone already has
//! this", and instead of dying silently the second launch does the useful thing: opens the window of
//! the copy that is already running.
//!
//! The running copy writes its window's address and token to a file in the user's own profile
//! ([`announce`]); the second launch reads it ([`running_window`]).

#![cfg_attr(windows, allow(unsafe_code))]

use std::path::PathBuf;

/// Where the running copy says how to reach its window.
fn instance_file() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Talkee").join("instance.txt")
}

/// Records the window's port and token for a later launch to use.
pub fn announce(port: u16, token: &str) {
    let path = instance_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{port} {token}"));
}

/// The port and token of the running copy, if it left them.
pub fn running_window() -> Option<(u16, String)> {
    let text = std::fs::read_to_string(instance_file()).ok()?;
    let (port, token) = text.trim().split_once(' ')?;
    Some((port.parse().ok()?, token.to_string()))
}

/// Takes the named lock. Returns false when another process already holds it.
///
/// The lock is held until the process exits, which is the point: the handle is deliberately never
/// closed.
#[cfg(windows)]
pub fn acquire(name: &str) -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 string that outlives the call, and a null
    // security descriptor is the documented way to ask for the default.
    unsafe {
        let handle = CreateMutexW(std::ptr::null(), 0, wide.as_ptr());
        if handle.is_null() {
            // Could not ask. Better to run twice than to refuse to run at all.
            return true;
        }
        GetLastError() != ERROR_ALREADY_EXISTS
    }
}

/// Off Windows there is nothing to lock.
#[cfg(not(windows))]
pub fn acquire(_name: &str) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_can_be_taken_once_and_not_twice() {
        let name = format!("Local\\TalkeeTest{}", std::process::id());
        assert!(acquire(&name), "the first taker gets it");
        #[cfg(windows)]
        assert!(!acquire(&name), "the second does not");
    }
}
