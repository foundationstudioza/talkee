//! Launching another program with Windows' permission prompt in front of it.
//!
//! # Why this exists at all
//!
//! Installing an audio driver requires elevation. There is no user-mode way around it — the
//! author of the driver we install says so plainly: *"since you install a driver, i don't think
//! it's possible to do it without asking for user agreement."* So the app has to be able to
//! raise that prompt deliberately, rather than hoping the child process does it for us.
//!
//! # Why this is one of only two modules with `unsafe`
//!
//! `ShellExecuteW` is the API for this: it resolves the executable, raises the consent prompt
//! through the shell, and reports failure instead of silently doing nothing. The alternatives
//! are worse — a `runas`-equivalent via `cmd` gets the quoting wrong often enough to generate
//! bug reports, and a crate to run one syscall is not a trade worth making. The crate root
//! carries `#![deny(unsafe_code)]`, so this file asks for the exception explicitly and keeps it
//! to one call, exactly as `message_pump` does.
#![allow(unsafe_code)]

use std::path::Path;

use anyhow::{bail, Result};

/// The value below which `ShellExecuteW` is reporting a failure rather than a handle.
///
/// The function is documented as returning an `HINSTANCE`, but failures come back as small
/// integers — 2 for a missing file, 5 for access denied, 31 for "no association" — so the
/// return value is really a tagged union and comparing against 32 is the accepted way to read
/// it. This is the one place where guessing wrong is silent: the installer would simply never
/// appear and the user would be told the install went fine.
const SHELL_FAILURE_CEILING: isize = 32;

/// NUL-terminated UTF-16, which is what every Win32 `W` function below the wrappers expects.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Opens a path or a URL with whatever the user has associated with it.
///
/// The `open` shell verb rather than a hard-coded browser: it resolves the user's own default, works
/// wherever that browser is installed, and reports failure instead of silently doing nothing. Takes a
/// string rather than a `Path` because the wizard is a URL and the installer is a path, and they are
/// opened by exactly the same call.
pub fn open(target: &str) -> Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let verb = wide("open");
    let file = wide(target);

    let code = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;

    if code <= SHELL_FAILURE_CEILING {
        bail!("Windows would not open {target} (code {code})");
    }
    Ok(())
}

/// Runs `exe` with `args`, elevated, and returns as soon as Windows has launched it.
///
/// The `runas` verb is what raises the consent prompt, and there is no way around it for
/// something that installs a driver — VB-Audio's author says as much. It is used as the
/// fallback when running the program directly does not work, which is exactly what an
/// unelevated launch of a driver installer looks like.
///
/// It cannot be waited on: an elevated process is not our child, so the caller reports that the
/// installer is *running* rather than that it finished. `working_dir` matters because VB-Audio's
/// setup looks for its `.inf` and `.sys` files beside itself rather than in the caller's cwd.
pub fn run_elevated(exe: &Path, args: &str, working_dir: &Path) -> Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let verb = wide("runas");
    let target = wide(&exe.to_string_lossy());
    let parameters = wide(args);
    let directory = wide(&working_dir.to_string_lossy());

    let code = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            parameters.as_ptr(),
            directory.as_ptr(),
            SW_SHOWNORMAL,
        )
    } as isize;

    if code <= SHELL_FAILURE_CEILING {
        bail!(
            "Windows would not start {} with permission (code {code})",
            exe.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_string_is_nul_terminated_and_utf16() {
        // An un-terminated buffer here reads off the end of the allocation, and the failure
        // is a browser opening on a garbage URL rather than a crash, which is the kind of
        // thing that never gets reported.
        let encoded = wide("aé");
        assert_eq!(encoded.len(), 3, "two characters plus the terminator");
        assert_eq!(encoded[0], 'a' as u16);
        assert_eq!(encoded[1], 'é' as u16, "not UTF-8, and not ASCII-truncated");
        assert_eq!(*encoded.last().expect("non-empty"), 0);
    }

    #[test]
    fn the_failure_threshold_is_the_documented_one() {
        // Pinned because the comparison is `<=` against a magic number that is easy to
        // "tidy" into a wrong shape. 32 is a failure, 33 is the smallest success.
        assert_eq!(SHELL_FAILURE_CEILING, 32);
        assert!(33 > SHELL_FAILURE_CEILING);
    }
}
