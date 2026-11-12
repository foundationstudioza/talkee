//! Putting dictated text where the cursor is, in whatever app has focus.
//!
//! * Text is always one line. Typed, a line break is an Enter key: in Slack, Discord, and Teams it
//!   sends the message half-written, and in a terminal it runs the command early. Pasted, many
//!   terminals mishandle several lines. So any line break left in the text becomes a space here.
//! * It is typed as Unicode keystrokes (`SendInput` with `KEYEVENTF_UNICODE`), which works in
//!   nearly every app and leaves the clipboard alone. Only very long text is pasted, because typing
//!   thousands of characters one by one is slow to watch; the previous clipboard text is put back.
//!
//! Text is never typed into a window other than the one that had focus when recording started:
//! if the user switched windows while it was being transcribed, it goes on the clipboard instead.
//! Windows also silently drops input sent to a program running as administrator (UIPI), so that
//! case is detected up front and handled the same way.

/// How the text was delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Typed at the cursor.
    Typed,
    /// Pasted at the cursor (the previous clipboard text was put back).
    Pasted,
    /// Left on the clipboard, because typing was not possible. Carries why.
    Clipboard(ClipboardReason),
}

/// Why text went to the clipboard rather than the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardReason {
    /// A different window has focus than when recording started.
    FocusChanged,
    /// The focused program runs as administrator and Talkee does not.
    Elevated,
    /// Typing failed for another reason.
    Failed,
}

impl Delivery {
    /// A short line for the overlay and the log.
    pub fn describe(self) -> &'static str {
        match self {
            Delivery::Typed => "typed",
            Delivery::Pasted => "pasted",
            Delivery::Clipboard(ClipboardReason::FocusChanged) => {
                "copied to the clipboard (a different window has focus now)"
            }
            Delivery::Clipboard(ClipboardReason::Elevated) => {
                "copied to the clipboard (that app runs as administrator, so Windows blocks typing into it)"
            }
            Delivery::Clipboard(ClipboardReason::Failed) => "copied to the clipboard (typing failed)",
        }
    }
}

/// Whether (one-line) text is pasted rather than typed.
pub fn should_paste(text: &str) -> bool {
    text.chars().count() > 400
}

/// An opaque handle to the window that had focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target(pub isize);

#[cfg(windows)]
pub use win::{deliver, foreground};

#[cfg(not(windows))]
pub fn foreground() -> Target {
    Target(0)
}

#[cfg(not(windows))]
pub fn deliver(_text: &str, _target: Target) -> Delivery {
    Delivery::Clipboard(ClipboardReason::Failed)
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win {
    use super::{should_paste, ClipboardReason, Delivery, Target};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ACCESS_DENIED, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
        KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT, VK_V,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    /// Clipboard format for UTF-16 text.
    const CF_UNICODETEXT: u32 = 13;

    /// The window with focus now.
    pub fn foreground() -> Target {
        // SAFETY: no arguments; returns a window handle or null.
        Target(unsafe { GetForegroundWindow() } as isize)
    }

    /// Puts `text` at the cursor of `target`, or on the clipboard when that is not possible.
    pub fn deliver(text: &str, target: Target) -> Delivery {
        let text = &talkee_dictate::cleanup::single_line(text);
        if text.is_empty() {
            return Delivery::Typed;
        }
        if foreground() != target {
            return to_clipboard(text, ClipboardReason::FocusChanged);
        }
        if target_is_elevated(target) && !self_is_elevated() {
            return to_clipboard(text, ClipboardReason::Elevated);
        }
        // Keys still held from the hotkey would turn typed letters into shortcuts (Ctrl+A, Win+D).
        wait_for_modifiers_released();
        if should_paste(text) {
            paste(text)
        } else if type_unicode(text) {
            Delivery::Typed
        } else {
            to_clipboard(text, ClipboardReason::Failed)
        }
    }

    fn to_clipboard(text: &str, reason: ClipboardReason) -> Delivery {
        set_clipboard(text);
        Delivery::Clipboard(reason)
    }

    fn wait_for_modifiers_released() {
        let deadline = Instant::now() + Duration::from_secs(2);
        let held = |vk: VIRTUAL_KEY| {
            // SAFETY: a plain query of the key's state.
            (unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000) != 0
        };
        while Instant::now() < deadline
            && [VK_CONTROL, VK_SHIFT, VK_MENU, VK_LWIN, VK_RWIN]
                .into_iter()
                .any(held)
        {
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn key(vk: VIRTUAL_KEY, scan: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn send(inputs: &[INPUT]) -> bool {
        // SAFETY: a pointer to initialised INPUTs, their count, and their exact size.
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        sent as usize == inputs.len()
    }

    fn type_unicode(text: &str) -> bool {
        let inputs: Vec<INPUT> = text
            .encode_utf16()
            .flat_map(|unit| {
                [
                    key(0, unit, KEYEVENTF_UNICODE),
                    key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                ]
            })
            .collect();
        send(&inputs)
    }

    fn paste(text: &str) -> Delivery {
        let previous = get_clipboard();
        if !set_clipboard(text) {
            return Delivery::Clipboard(ClipboardReason::Failed);
        }
        let ok = send(&[
            key(VK_CONTROL, 0, 0),
            key(VK_V, 0, 0),
            key(VK_V, 0, KEYEVENTF_KEYUP),
            key(VK_CONTROL, 0, KEYEVENTF_KEYUP),
        ]);
        if !ok {
            return Delivery::Clipboard(ClipboardReason::Failed);
        }
        // The target reads the clipboard when it handles Ctrl+V, which can take a moment in a busy
        // app. Putting the old text back too soon would paste that instead.
        std::thread::sleep(Duration::from_millis(400));
        if let Some(previous) = previous {
            set_clipboard(&previous);
        }
        Delivery::Pasted
    }

    fn open_clipboard() -> bool {
        // Another program may hold it for a moment; try a few times.
        for _ in 0..10 {
            // SAFETY: null owner window is allowed.
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn get_clipboard() -> Option<String> {
        if !open_clipboard() {
            return None;
        }
        // SAFETY: the clipboard is open; the handle is only read while locked, then unlocked, and
        // the clipboard is closed on every path.
        let text = unsafe {
            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                None
            } else {
                let ptr = GlobalLock(handle as _) as *const u16;
                if ptr.is_null() {
                    None
                } else {
                    let mut len = 0;
                    while *ptr.add(len) != 0 {
                        len += 1;
                    }
                    let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                    GlobalUnlock(handle as _);
                    Some(text)
                }
            }
        };
        // SAFETY: we opened it.
        unsafe { CloseClipboard() };
        text
    }

    fn set_clipboard(text: &str) -> bool {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        if !open_clipboard() {
            return false;
        }
        // SAFETY: the clipboard is open. The allocation is sized for `wide`, written while locked,
        // and handed to the system with SetClipboardData, which then owns it.
        let ok = unsafe {
            EmptyClipboard();
            let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2);
            if memory.is_null() {
                false
            } else {
                let ptr = GlobalLock(memory) as *mut u16;
                if ptr.is_null() {
                    false
                } else {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                    GlobalUnlock(memory);
                    !SetClipboardData(CF_UNICODETEXT, memory as HANDLE).is_null()
                }
            }
        };
        // SAFETY: we opened it.
        unsafe { CloseClipboard() };
        ok
    }

    fn token_elevated(process: HANDLE) -> Option<bool> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: a valid process handle and an out-pointer for the token, closed below.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            // Access denied to an ordinary user's process token usually means it is elevated.
            // SAFETY: reads the calling thread's last error.
            return (unsafe { GetLastError() } == ERROR_ACCESS_DENIED).then_some(true);
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0;
        // SAFETY: the buffer is a TOKEN_ELEVATION and its size is passed.
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                &mut elevation as *mut _ as *mut _,
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut size,
            )
        };
        // SAFETY: we opened it.
        unsafe { CloseHandle(token) };
        (ok != 0).then_some(elevation.TokenIsElevated != 0)
    }

    fn self_is_elevated() -> bool {
        // SAFETY: the pseudo-handle for this process needs no closing.
        token_elevated(unsafe { GetCurrentProcess() }).unwrap_or(false)
    }

    fn target_is_elevated(target: Target) -> bool {
        let mut pid = 0;
        // SAFETY: a window handle (possibly stale, which the call tolerates) and an out-pointer.
        unsafe { GetWindowThreadProcessId(target.0 as _, &mut pid) };
        if pid == 0 {
            return false;
        }
        // SAFETY: opens a query-only handle, closed below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            // SAFETY: reads the last error.
            return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
        }
        let elevated = token_elevated(process).unwrap_or(false);
        // SAFETY: we opened it.
        unsafe { CloseHandle(process) };
        elevated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_very_long_text_is_pasted() {
        assert!(!should_paste("Hey, can you check the build?"));
        assert!(should_paste(&"word ".repeat(100)));
    }
}
