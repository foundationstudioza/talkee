//! The Win32 message pump, and the console Ctrl+C handler.
//!
//! # Why this is the only module in the crate with `unsafe`
//!
//! A tray icon on Windows *is* a window: `tray-icon` creates a hidden one and delivers
//! clicks and menu commands from its window procedure into a channel. Those events exist
//! only while somebody drains the thread's message queue, so a tray app that does not
//! pump messages shows an icon that ignores every click. The standard library has no
//! wrapper for `PeekMessageW`, and the `windows-sys` bindings are `unsafe` by
//! construction, so this is a genuine case of a Windows API needing it.
//!
//! The isolation is enforced by the compiler: the crate root carries
//! `#![deny(unsafe_code)]` and this module carries a single `#![allow(unsafe_code)]`, so
//! `unsafe` anywhere else in the app is a build error rather than a review question.
//!
//! What is actually called: `PeekMessageW` + `TranslateMessage` + `DispatchMessageW` for
//! the pump, and `SetConsoleCtrlHandler` so that Ctrl+C closes the audio device instead
//! of killing the process mid-frame. `PeekMessageW` is given a stack `MSG` that it fills
//! in, the pump loop never blocks, and no pointer is stored or handed back out. The
//! Ctrl+C handler has to be a plain `extern "system"` function, so the flag it sets is a
//! process-wide atomic.

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};

/// Set by the console handler, which runs on its own thread.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// True once Ctrl+C (or a console close) has been seen.
///
/// Checked once per loop iteration, which is at most ~20 ms later, so the device is
/// released promptly rather than when the process is torn down.
pub fn quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::Relaxed)
}

#[cfg(windows)]
mod win {
    use super::QUIT_REQUESTED;
    use std::sync::atomic::Ordering;
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{FALSE, TRUE};
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    /// Drains every message already queued for this thread, then returns.
    ///
    /// Deliberately `PeekMessage` rather than `GetMessage`: this runs inside the app's
    /// own loop, which also has to sample the level meter and update the icon. Blocking
    /// here would stop the meter, and `PeekMessage` returning zero is a normal, idle
    /// outcome rather than an error.
    pub fn pump() {
        // SAFETY: `message` is a stack value of the exact type the API fills in, and it
        // is only read by these three calls. `hwnd` is null, which asks for messages for
        // any window on this thread -- which is what a tray icon's hidden window needs.
        // `PM_REMOVE` removes each message as it is read, so the loop terminates.
        unsafe {
            let mut message: MSG = std::mem::zeroed();
            while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                // Both calls take a `&MSG` and dispatch into Win32; neither keeps it.
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    /// The console control handler.
    ///
    /// Returns `TRUE` for the events it handles, which tells Windows the process is
    /// shutting down on its own terms. That is what makes Ctrl+C a *clean* exit here:
    /// the main loop notices the flag, drops the writer, and the cable is released
    /// instead of being left open by a killed process.
    unsafe extern "system" fn on_console_event(ctrl_type: u32) -> BOOL {
        if ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT || ctrl_type == CTRL_CLOSE_EVENT {
            QUIT_REQUESTED.store(true, Ordering::Relaxed);
            return TRUE;
        }
        // Anything else (logoff, shutdown) is not ours to claim; the default handler
        // still runs, so the user cannot be locked out of ending the process.
        FALSE
    }

    /// Installs the handler. Returns false when Windows refuses.
    pub fn install() -> bool {
        // SAFETY: a `Some` function pointer with the exact `PHANDLER_ROUTINE` signature,
        // and `TRUE` to add rather than remove it. The handler touches nothing but an
        // atomic, so it is safe to run on the OS thread Windows calls it from.
        unsafe { SetConsoleCtrlHandler(Some(on_console_event), TRUE) != 0 }
    }
}

/// Drains every message already queued for this thread, then returns.
#[cfg(windows)]
pub fn pump() {
    win::pump();
}

/// Installs the Ctrl+C handler. Returns false when the platform refuses.
#[cfg(windows)]
pub fn install_ctrl_c_handler() -> bool {
    win::install()
}

/// No-op: this build has no message queue to drain.
#[cfg(not(windows))]
pub fn pump() {}

/// Always false: there is no console handler in this build, so Ctrl+C keeps its default
/// behaviour of ending the process.
#[cfg(not(windows))]
pub fn install_ctrl_c_handler() -> bool {
    false
}
