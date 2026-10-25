//! Where the app's printed lines go now that it has no console window.
//!
//! # Why this exists
//!
//! The app is built as a Windows GUI program, so double-clicking it does not open a terminal. Every
//! state change still goes to stdout, because that log is the only complete record of what the tray
//! was showing — so stdout and stderr are pointed at a file instead of at nothing.
//!
//! * normally: `%APPDATA%\Talkee\talkee.log`, started fresh when it grows past [`MAX_BYTES`], with the
//!   previous one kept as `talkee.old.log`;
//! * with `--console`: attached to the terminal that started the app, or a new console window when
//!   there is none, for watching it live.
//!
//! Redirecting the *handles* rather than threading a logger through the code is deliberate: every
//! `println!` in `talkee-core` and `talkee-vmic` lands in the file without those crates knowing the
//! app exists.

#![cfg_attr(windows, allow(unsafe_code))]

use std::path::PathBuf;

/// A log past this size is moved aside at startup, so a tray app that runs for months does not fill
/// a disk with heartbeats.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Where the log lives.
pub fn log_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Talkee").join("talkee.log")
}

/// Sends stdout and stderr where a person can find them. Returns where, for the first line of the log.
#[cfg(windows)]
pub fn init(console: bool) -> String {
    use std::fs::{self, OpenOptions};
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{
        AllocConsole, AttachConsole, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    if console {
        // SAFETY: plain Win32 calls with no pointers. A failed attach (no parent console, as when
        // launched from Explorer) is followed by a new console, which is what was asked for.
        unsafe {
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                AllocConsole();
            }
        }
        return "the console".to_string();
    }

    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = fs::rename(&path, path.with_file_name("talkee.old.log"));
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        // Nowhere to write. `println!` ignores a missing handle, so the app still runs.
        return "nowhere (the log file could not be opened)".to_string();
    };
    let handle = file.as_raw_handle();
    // SAFETY: the handle is valid and is deliberately never closed — it has to outlive every later
    // print — which is why the `File` is forgotten rather than dropped.
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, handle as _);
        SetStdHandle(STD_ERROR_HANDLE, handle as _);
    }
    std::mem::forget(file);
    path.display().to_string()
}

/// Off Windows there is a terminal already, and nothing to redirect.
#[cfg(not(windows))]
pub fn init(_console: bool) -> String {
    "the terminal".to_string()
}
