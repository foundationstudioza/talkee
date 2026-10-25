//! Getting a virtual microphone onto the machine.
//!
//! Windows has no user-mode way to create a capture device — a microphone only exists if a
//! signed kernel driver made it — so Talkee ships VB-Audio's VB-CABLE and offers to install it.
//!
//! # Why this is chattier than a silent install
//!
//! VB-Audio's distribution terms, from <https://vb-audio.com/Services/licensing.htm>, allow
//! exactly this: *"We allow to distribute VB-CABLE package ... with your Application (free or
//! commercial) and possibly embed it in your installation package (VB-CABLE silent
//! installation) if our donationware model is applicable."* The donationware model is
//! applicable when *"the End User can see and identify VB-CABLE as VB-Audio application AND
//! this End User must be in a position to donate/pay license if finding it useful"*, and the
//! distributor must state the origin and that it is a donationware.
//!
//! So [`NOTICE`] is not branding, it is the condition. An install that hides who wrote the
//! driver, and that they would like to be paid for it, is the one thing the licence rules out.
//!
//! # What we do, and what we do not
//!
//! We unpack the vendor's package **unmodified** and run the vendor's own signed setup with
//! `-i -h`: install, and hide the wizard. We do not install a driver ourselves, and we do not
//! modify what they shipped.
//!
//! # The two things that cannot be made silent
//!
//! A driver install needs the user's consent, and VB-Audio's author says as much: *"since you
//! install a driver, i don't think it's possible to do it without asking for user agreement."*
//! Windows will raise a permission prompt. And VB-Audio's instructions end with *"To finalize
//! installation, you must reboot your computer."* One prompt and one restart is the floor, so
//! the app says so before it starts rather than letting the user conclude the microphone is
//! broken.

use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use zip::ZipArchive;

/// The package, compiled into the binary.
///
/// 1.3 MB next to a binary that is already 1.8 MB is nothing, and it is the difference between
/// "download Talkee and you have what you need" and "download Talkee, then go and find a driver
/// on a website and hope it is the right one". Where it came from, what it hashes to and who
/// signed it are recorded in `assets/README.md`.
static PACKAGE: &[u8] = include_bytes!("../assets/VBCABLE_Driver_Pack45.zip");

/// The setup program for a 64-bit OS, per VB-Audio's readme. The 32-bit one beside it "will do
/// nothing on 64 bit O/S".
const SETUP_X64: &str = "VBCABLE_Setup_x64.exe";

/// The folder the package is unpacked into, under the user's temp directory.
const UNPACK_DIR: &str = "talkee-vbcable";

/// The switches: install, and hide the setup's own wizard.
///
/// Confirmed in VB-Audio's forum, where a user automating this reports that `-i -h` works and
/// the author replies about the remaining Windows prompt without disputing it. `-h` hides
/// VB-Audio's UI; it cannot hide Windows' consent dialog.
fn install_args() -> [&'static str; 2] {
    ["-i", "-h"]
}

/// What the app prints before it installs anything.
///
/// This is the licence condition made visible: who wrote it, that it is a donationware, and
/// where to find them. It also names the two things the user is about to meet, so that a
/// permission prompt and a restart read as expected rather than as the app misbehaving.
pub const NOTICE: &str = "Installing VB-CABLE, a virtual audio cable by VB-Audio \
     (vb-audio.com/Cable). It is a donationware — all participations are welcome. Windows will \
     ask permission, because this is an audio driver, and you will need to restart Windows \
     afterwards to finish. Nothing else on this PC is changed.";

/// What happened when the vendor's setup was asked to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It ran and exited reporting success.
    Installed,
    /// It was launched, and we cannot see it finish — elevation put it out of our reach.
    Running,
}

/// Unpacks the bundled package and runs VB-Audio's setup, elevated.
pub fn install() -> Result<Outcome> {
    let setup = unpack_into(&unpack_dir())?;
    let directory = setup
        .parent()
        .expect("the setup path always has a parent")
        .to_path_buf();

    // The primary path launches it and *waits*. VB-Audio's readme says to run this in
    // administrator mode, so the executable normally carries a manifest asking for elevation,
    // which means Windows raises the consent prompt itself and `status` blocks until the install
    // is over. Waiting matters more than it looks: with `-h` the setup reports failure in no
    // dialog at all, so its exit code is the only report we will ever get.
    let mut command = Command::new(&setup);
    command.args(install_args()).current_dir(&directory);
    match command.status() {
        Ok(status) if status.success() => Ok(Outcome::Installed),
        // It ran and said no, or it could not be started at all — both of which are what an
        // unelevated launch of a driver installer looks like. One attempt through the shell's
        // `runas` verb, which always elevates, at the cost of not being able to wait for it.
        Ok(_) | Err(_) => {
            crate::shell::run_elevated(&setup, &install_args().join(" "), &directory)?;
            Ok(Outcome::Running)
        }
    }
}

/// Where the package is unpacked.
fn unpack_dir() -> PathBuf {
    std::env::temp_dir().join(UNPACK_DIR)
}

/// Writes the bundled package into `dir` and returns the path of the setup program.
///
/// The package is written out rather than run from memory because the setup program expects to
/// find its own `.inf`, `.sys` and `.cat` files beside itself.
pub fn unpack_into(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating the unpack directory {}", dir.display()))?;

    let mut archive = ZipArchive::new(Cursor::new(PACKAGE))
        .context("the bundled VB-CABLE package is not a readable zip")?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("reading entry {index} of the bundled package"))?;

        // A zip is foreign data, and this one becomes files on disk. `enclosed_name` refuses
        // anything that would escape `dir` — a path with a separator or a `..` in it — which is
        // the difference between unpacking a package and letting it write wherever it likes.
        let name = entry.enclosed_name().ok_or_else(|| {
            anyhow::anyhow!("the package contains an unsafe path: {}", entry.name())
        })?;
        let destination = dir.join(name);

        if entry.is_dir() {
            std::fs::create_dir_all(&destination)
                .with_context(|| format!("creating {}", destination.display()))?;
            continue;
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut file = File::create(&destination)
            .with_context(|| format!("writing {}", destination.display()))?;
        std::io::copy(&mut entry, &mut file)
            .with_context(|| format!("unpacking {}", destination.display()))?;
    }

    let setup = dir.join(SETUP_X64);
    if !setup.is_file() {
        bail!("the bundled VB-CABLE package does not contain {SETUP_X64}");
    }
    Ok(setup)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file that must be in the archive, and the easiest way to tell the package we reviewed
    /// from something that got substituted. Only the tests name it: `unpack_into` only cares
    /// about the setup program, because that is the thing it is going to run.
    const README_IN_PACKAGE: &str = "readme.txt";

    #[test]
    fn the_bundled_package_is_intact_and_has_what_we_run() {
        // The point of embedding this is that the app carries what it needs. A truncated or
        // swapped archive would otherwise be discovered by a user whose microphone setup quietly
        // did nothing, and none of that is caught by the compiler.
        let mut archive =
            ZipArchive::new(Cursor::new(PACKAGE)).expect("the embedded package must be a zip");

        let mut names = Vec::new();
        for index in 0..archive.len() {
            let entry = archive.by_index(index).expect("every entry is readable");
            names.push(entry.name().to_string());
        }

        assert!(
            names.contains(&SETUP_X64.to_string()),
            "there is no setup program to run: {names:?}"
        );
        assert!(
            names.contains(&README_IN_PACKAGE.to_string()),
            "no readme, so this is not the package that was reviewed: {names:?}"
        );
        // The driver itself, for the OS we run on. Without it we would be launching an installer
        // that has nothing to install.
        assert!(
            names.iter().any(|n| n.ends_with("vbMmeCable64_win10.inf")),
            "no driver for 64-bit Windows: {names:?}"
        );
    }

    #[test]
    fn unpacking_puts_the_setup_beside_the_driver_it_installs() {
        // "It unpacked" is not enough: the setup program looks for its `.inf`, `.sys` and `.cat`
        // neighbours relative to itself, so where they land is part of whether this works.
        let dir = std::env::temp_dir().join("talkee-vbcable-test");
        let _ = std::fs::remove_dir_all(&dir);

        let setup = unpack_into(&dir).expect("unpacking the bundled package");
        assert_eq!(setup.file_name().and_then(|n| n.to_str()), Some(SETUP_X64));
        assert!(setup.is_file(), "the setup program must exist to be run");
        assert!(
            dir.join(README_IN_PACKAGE).is_file(),
            "the package's own readme should come along"
        );
        assert!(
            setup.parent().map(|p| p.join("readme.txt").is_file()) == Some(true),
            "the setup must find its files in its own directory"
        );

        let drivers: Vec<String> = std::fs::read_dir(&dir)
            .expect("the unpack directory exists")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".inf"))
            .collect();
        assert!(
            !drivers.is_empty(),
            "the inf files are what the setup installs from, and none were written"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_install_uses_the_switch_that_was_confirmed() {
        // Pinned deliberately. `-i` installs and `-h` hides the wizard, confirmed in VB-Audio's
        // forum thread on silent installation. This runs a driver installer, so a "tidied up"
        // argument list here changes what happens on somebody's machine.
        assert_eq!(install_args(), ["-i", "-h"]);
    }

    #[test]
    fn the_notice_satisfies_the_terms_we_rely_on() {
        // The permission to ship this package is conditional on the end user being able to see
        // and identify it as a VB-Audio application and being able to pay for it. This is that
        // condition, asserted, so the wording cannot be tidied away while the permission stays.
        assert!(
            NOTICE.contains("VB-Audio"),
            "the notice must name who wrote the driver"
        );
        assert!(
            NOTICE.contains("donationware"),
            "and say it is a donationware, which is what invites payment"
        );
        assert!(
            NOTICE.contains("vb-audio.com/Cable"),
            "and give the origin, which the terms require by name"
        );
        // And the two things the user must not be surprised by.
        assert!(NOTICE.contains("permission"), "warn about the Windows prompt");
        assert!(NOTICE.contains("restart"), "warn about the restart");
    }
}
