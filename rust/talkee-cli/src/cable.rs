//! `talkee cable` — the virtual cable as something the CLI can fix.
//!
//! # Why install is a verb of its own, with two gates in front of it
//!
//! Everything here follows from one constraint: on Windows only a signed
//! kernel-mode driver can create a microphone, so Talkee has to put somebody
//! else's driver on the user's machine. That is a large thing to do, and the
//! vendor's licence adds two demands on top of the user's consent — the user must
//! be able to see that it is VB-Audio and to donate, and the pack must not be
//! integrated into our own installer without the author's agreement.
//!
//! So the shape of this module is deliberate:
//!
//! * no other subcommand ever installs anything, under any flag, ever;
//! * `install` refuses unless it is told `--yes` **and** a human is at the
//!   console, so a script, a scheduled task or a future auto-updater cannot do
//!   this behind a person's back;
//! * `--dry-run` prints the exact commands, touches nothing, and leaves the URL
//!   in the user's hands. It is built by the *same* pure function that the real
//!   run executes, so the preview cannot drift from the thing itself — which is
//!   the only reason a preview is worth printing.
//!
//! The download and the unpack are `curl` and `tar` with argument vectors rather
//! than a shell string, so a URL or a path can never be re-parsed as code. Both
//! ship with Windows 10 1803 and later (`C:\Windows\System32\curl.exe`, and
//! bsdtar 3.5.2, which reads the zip), which is why this needs no HTTP or archive
//! crate in the dependency tree.

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use talkee_vmic::cable::{CableState, InstallPlan, InstallRecipe};
use talkee_vmic::cable_state;

/// Arguments for `talkee cable`.
#[derive(Args, Debug)]
pub struct CableArgs {
    #[command(subcommand)]
    pub command: CableCommand,
}

/// What `talkee cable` can do.
#[derive(Subcommand, Debug)]
pub enum CableCommand {
    /// Report whether a usable virtual cable is installed, and what to do if not.
    Status,
    /// Download VB-CABLE from the vendor and run its setup. Asks first.
    Install(InstallArgs),
}

/// Arguments for `talkee cable install`.
#[derive(Args, Debug)]
pub struct InstallArgs {
    /// I have read what this downloads and I want it installed now.
    ///
    /// Required for a real install. Without it, `install` prints exactly what it
    /// would do and stops, so nobody installs a kernel driver by accident.
    #[arg(long)]
    pub yes: bool,

    /// Print the exact commands and do nothing at all.
    ///
    /// This works whether or not a cable is already installed, because "what would
    /// you do on this machine?" is a question you ask most often about the machine
    /// you develop on — which already has one.
    #[arg(long)]
    pub dry_run: bool,

    /// Where to download and unpack the vendor pack.
    ///
    /// Defaults to a `talkee-cable` folder in the system temp directory. The
    /// vendor's readme asks for the setup to be launched *from* the unpacked
    /// folder, which is why this is a directory and not just a file name.
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

/// Runs `talkee cable`.
pub fn run(args: CableArgs) -> Result<()> {
    match args.command {
        CableCommand::Status => status(),
        CableCommand::Install(install_args) => install(install_args),
    }
}

/// `talkee cable status`.
fn status() -> Result<()> {
    let state = cable_state()?;

    println!("Talkee {} — virtual cable status", env!("CARGO_PKG_VERSION"));
    println!();
    println!("  {}", state.summary());

    // Both halves of the licensing question get their own line: "is it installed"
    // and "may we ship it" have different answers for most products, and the four
    // combinations of those answers are what the user has to think about.
    match state.may_redistribute() {
        Some(true) => println!("  redistributable : yes"),
        Some(false) => println!("  redistributable : no — usable, but yours to install, not ours to ship"),
        None => println!("  redistributable : unknown, because we do not recognise the device"),
    }
    match state.install_plan() {
        InstallPlan::WeInstall { family, recipe } => println!(
            "  install plan    : fetch {} from {} and run its setup",
            family.label, recipe.url
        ),
        InstallPlan::UserInstalls { family } => println!(
            "  install plan    : none — only you may install {}",
            family.label
        ),
        InstallPlan::NothingToDo => println!("  install plan    : nothing to do"),
    }

    println!();
    for line in state.next_steps() {
        println!("{line}");
    }
    Ok(())
}

/// `talkee cable install`.
fn install(args: InstallArgs) -> Result<()> {
    // A dry run is deliberately independent of what is installed: it is the only
    // way to see what a real run would do on a machine that already has a cable,
    // and that is the machine you develop on.
    if args.dry_run {
        return show_plan(&args, PlanMode::DryRun);
    }

    let state = cable_state()?;
    if state.is_usable() {
        println!("{}", state.summary());
        println!();
        println!("Nothing to install. A second copy of the same driver would only be a");
        println!("chance to break a machine that already works.");
        return Ok(());
    }

    let InstallPlan::WeInstall { recipe, .. } = state.install_plan() else {
        // Nothing is installed and we are not permitted to install anything: only
        // reachable if the recipe table lost its one entry, but the message has to
        // be useful rather than a panic.
        bail!(
            "there is no cable and nothing we are allowed to install. Run \
             `talkee cable status` for the products that work if you install them \
             yourself."
        );
    };

    // The gate. Both conditions are checked here, not at the call sites, so no
    // future caller can arrange to skip one of them.
    if let Some(reason) = refusal(args.yes, std::io::stdin().is_terminal(), false) {
        show_plan(&args, PlanMode::NeedsConsent)?;
        return Err(anyhow!(reason));
    }

    println!("{}", recipe.consent_note);
    println!();
    show_plan(&args, PlanMode::RealRun)?;

    let dir = args.dir.clone().unwrap_or_else(default_work_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let steps = install_steps(recipe, &dir);
    execute(&steps)
}

/// Whether a plan is being printed as a preview or is about to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanMode {
    /// Printed because the user asked for `--dry-run`. Nothing will run.
    DryRun,
    /// Printed because the user has to decide whether to pass `--yes`.
    NeedsConsent,
    /// Printed immediately before running.
    RealRun,
}

/// Prints what a real install would do, and in which order.
///
/// The steps come from [`install_steps`], the same function [`install`] feeds to
/// [`execute`], so this cannot describe something other than what runs.
fn show_plan(args: &InstallArgs, mode: PlanMode) -> Result<()> {
    let recipe = our_recipe()?;
    let dir = args.dir.clone().unwrap_or_else(default_work_dir);
    let steps = install_steps(recipe, &dir);

    match mode {
        PlanMode::DryRun => {
            println!("Dry run: nothing below will be executed.");
            println!();
            println!("  pack      : {}", recipe.url);
            println!("  unpack to : {}", dir.display());
            println!();
            println!("A real run would do this, in this order:");
        }
        PlanMode::NeedsConsent => {
            println!("This is what `--yes` would do:");
        }
        PlanMode::RealRun => {
            println!("Installing, in this order:");
        }
    }
    for line in plan_lines(&steps) {
        println!("{line}");
    }
    println!();
    match mode {
        PlanMode::DryRun => {
            println!("Nothing was downloaded, changed or run.");
            println!(
                "The vendor's readme says to launch the setup in administrator mode, so a real"
            );
            println!("run will also show a Windows UAC prompt, and needs a reboot to finish.");
        }
        PlanMode::NeedsConsent => {
            println!("Nothing has been downloaded or run yet. Re-run with --yes to go ahead.");
            println!("(`talkee cable install --yes`, from a terminal — none of this ever runs");
            println!("unattended, and none of it is bundled into Talkee.)");
        }
        PlanMode::RealRun => {}
    }
    Ok(())
}

/// The one recipe we are permitted to run.
///
/// It asks the state machine rather than reading the table directly, so the
/// licensing gate decides whether we may install anything — this call site cannot
/// get the answer wrong on its own.
fn our_recipe() -> Result<&'static InstallRecipe> {
    match CableState::Missing.install_plan() {
        InstallPlan::WeInstall { recipe, .. } => Ok(recipe),
        _ => Err(anyhow!(
            "we are not permitted to install any virtual cable — see `talkee cable status`"
        )),
    }
}

/// Why we must not install right now, or `None` if we may.
///
/// Pure, so the rule that matters most — this never runs unattended — is testable
/// without a terminal, a download or a driver.
fn refusal(consent: bool, interactive: bool, dry_run: bool) -> Option<String> {
    if dry_run {
        // A dry run downloads nothing and runs nothing, so it needs no consent.
        return None;
    }
    if !consent {
        return Some(
            "refusing: installing a kernel-mode audio driver takes an explicit --yes, \
             once you have read what it downloads"
                .to_string(),
        );
    }
    if !interactive {
        return Some(
            "refusing: this is not an interactive console, and Talkee never installs a \
             driver unattended. Run it yourself, from a terminal, with --yes"
                .to_string(),
        );
    }
    None
}

/// Where the pack is downloaded and unpacked when the user does not say.
fn default_work_dir() -> PathBuf {
    std::env::temp_dir().join("talkee-cable")
}

/// One step of a real install.
///
/// A list of these is the whole install, as data. Nothing here touches the
/// network, the filesystem or a process, which is what lets a test assert exactly
/// what `talkee cable install --yes` would run without running any of it, and lets
/// `--dry-run` print that same list rather than a second, drifting description of
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallStep {
    /// Spawn this argument vector, optionally in a given working directory.
    Run {
        /// Why this step exists, in the words the user will read.
        what: &'static str,
        /// The program and its arguments. Never a shell string.
        argv: Vec<String>,
        /// Working directory. `Some` only where the vendor's own instructions
        /// depend on it.
        cwd: Option<PathBuf>,
    },
    /// Refuse to continue unless a file is at least this large.
    RequireFile {
        /// Why this check exists.
        what: &'static str,
        /// File to measure.
        path: PathBuf,
        /// Minimum acceptable size in bytes.
        min_bytes: u64,
    },
}

/// The pack VB-Audio was serving when this was written is 1 318 877 bytes.
///
/// `curl --fail` catches HTTP error *codes* only; a CDN that answers 200 with an
/// HTML apology, or a transfer that stops halfway, still arrives looking like
/// success. This floor is what stops an error page being handed to `tar` and then
/// to an installer. Half the real size, because the pack will grow.
const MIN_PACK_BYTES: u64 = 512_000;

/// The exact steps a real install would run.
///
/// `curl` and `tar` rather than a crate: both are Windows components since 10
/// 1803, and both take an argument vector, so neither the URL nor a path is ever
/// re-parsed by a shell. The `tar` here is bsdtar, which reads the zip.
fn install_steps(recipe: &InstallRecipe, dir: &Path) -> Vec<InstallStep> {
    let pack = dir.join(url_file_name(recipe.url));
    let setup = dir.join(recipe.setup_exe);

    // The vendor's readme is explicit about how to invoke the setup, and both
    // parts of that instruction become fields below: the switches come from the
    // recipe, and the working directory is the folder it was unpacked into.
    let mut argv = vec![setup.display().to_string()];
    argv.extend(recipe.setup_args.iter().map(|arg| (*arg).to_string()));

    vec![
        InstallStep::Run {
            what: "download the pack from the vendor",
            argv: vec![
                "curl".to_string(),
                "--location".to_string(),
                "--fail".to_string(),
                "--output".to_string(),
                pack.display().to_string(),
                recipe.url.to_string(),
            ],
            cwd: None,
        },
        InstallStep::RequireFile {
            what: "check the download is a pack and not an error page",
            path: pack.clone(),
            min_bytes: MIN_PACK_BYTES,
        },
        InstallStep::Run {
            what: "unpack it into the same folder",
            argv: vec![
                "tar".to_string(),
                "-xf".to_string(),
                pack.display().to_string(),
                "-C".to_string(),
                dir.display().to_string(),
            ],
            cwd: None,
        },
        InstallStep::Run {
            what: "run VB-Audio's setup program",
            argv,
            cwd: Some(dir.to_path_buf()),
        },
    ]
}

/// The file name a URL saves as.
///
/// Derived from the URL instead of stored next to it, so the two cannot end up
/// disagreeing about which pack we fetched.
fn url_file_name(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

/// One step as a line the user can read, and copy.
fn render_line(step: &InstallStep) -> String {
    match step {
        InstallStep::Run { argv, cwd, .. } => {
            let line = argv
                .iter()
                .map(|part| quote(part))
                .collect::<Vec<_>>()
                .join(" ");
            match cwd {
                Some(dir) => format!("[in {}] {line}", dir.display()),
                None => line,
            }
        }
        InstallStep::RequireFile {
            path, min_bytes, ..
        } => format!("check {} is at least {min_bytes} bytes", path.display()),
    }
}

/// Quotes an argument only where a shell would need it.
///
/// For *display* only: what gets spawned is an argument vector, so this exists to
/// keep the preview readable and pasteable, not to make a command line safe.
fn quote(part: &str) -> String {
    if part.is_empty() || part.contains([' ', '\t', '"']) {
        format!("\"{}\"", part.replace('"', "\\\""))
    } else {
        part.to_string()
    }
}

/// The numbered lines the preview prints for a plan.
///
/// Separate from [`show_plan`] so a test can hold the printed preview next to the
/// plan that [`execute`] consumes: the two must be the same list, or `--dry-run`
/// is a story about the install rather than a description of it.
fn plan_lines(steps: &[InstallStep]) -> Vec<String> {
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| format!("  {}. {}", index + 1, render_line(step)))
        .collect()
}

/// Runs the steps, stopping at the first one that fails.
fn execute(steps: &[InstallStep]) -> Result<()> {
    for step in steps {
        match step {
            InstallStep::Run { what, argv, cwd } => {
                let Some((program, rest)) = argv.split_first() else {
                    bail!("internal error: a step with no program to run");
                };
                println!("  $ {}", render_line(step));
                let mut command = Command::new(program);
                command.args(rest);
                if let Some(dir) = cwd {
                    command.current_dir(dir);
                }
                let status = command
                    .status()
                    .with_context(|| format!("running {program} to {what}"))?;
                if !status.success() {
                    bail!("{what} failed ({status}): {}", render_line(step));
                }
            }
            InstallStep::RequireFile {
                what,
                path,
                min_bytes,
            } => {
                let size = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
                if size < *min_bytes {
                    bail!(
                        "{what}: {} is {size} bytes, less than the {min_bytes} a real pack has. \
                         Delete it and try again.",
                        path.display()
                    );
                }
            }
        }
    }
    println!();
    println!("VB-Audio's setup has finished. Reboot for the device to appear, then run");
    println!("`talkee probe` to see it and find the microphone to select.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recipe the CLI will actually use, reached the way [`our_recipe`]
    /// reaches it, so these tests exercise the gate as well as the plan.
    fn recipe() -> &'static InstallRecipe {
        our_recipe().expect("VB-CABLE is the one thing we may install")
    }

    fn dir() -> PathBuf {
        PathBuf::from(r"C:\tmp\talkee-cable")
    }

    fn run_step(index: usize) -> (&'static str, Vec<String>, Option<PathBuf>) {
        match install_steps(recipe(), &dir()).swap_remove(index) {
            InstallStep::Run { what, argv, cwd } => (what, argv, cwd),
            other => panic!("step {index} is not a Run step: {other:?}"),
        }
    }

    #[test]
    fn the_only_thing_we_offer_to_install_is_one_we_may_ship() {
        // The CLI must not carry its own opinion about which cable to fetch: it
        // asks the library, and the library only ever says yes to VB-CABLE.
        let recipe = recipe();
        let family = talkee_vmic::cable::family_by_id(recipe.family_id).expect("a curated family");
        assert!(
            family.redistributable,
            "the CLI would install {} for the user",
            family.id
        );
        assert_eq!(family.id, "vb-cable");
    }

    #[test]
    fn the_plan_downloads_the_recipe_url_to_the_working_directory() {
        let (what, argv, cwd) = run_step(0);
        assert!(what.contains("download"));
        assert_eq!(argv[0], "curl", "no shell, and a program that ships with Windows");
        assert!(
            argv.iter().any(|arg| arg == "--fail"),
            "an HTTP error must fail the step instead of leaving an error page on disk"
        );
        assert!(
            argv.contains(&recipe().url.to_string()),
            "the URL must come from the recipe, not from this file"
        );
        let expected = dir().join(url_file_name(recipe().url)).display().to_string();
        assert!(
            argv.contains(&expected),
            "expected to download to {expected}, got {argv:?}"
        );
        assert_eq!(cwd, None);
    }

    #[test]
    fn the_plan_measures_the_download_before_unpacking_it() {
        let steps = install_steps(recipe(), &dir());
        // The check must sit between the download and the unpack: running it
        // anywhere else would let a half-empty file through, or measure nothing.
        assert!(matches!(steps[0], InstallStep::Run { .. }));
        let InstallStep::RequireFile {
            path, min_bytes, ..
        } = &steps[1]
        else {
            panic!("the download must be measured before it is used: {steps:?}")
        };
        assert_eq!(
            path,
            &dir().join(url_file_name(recipe().url)),
            "measure the file that was actually downloaded"
        );
        assert!(
            *min_bytes > 0 && *min_bytes < 1_318_877,
            "{min_bytes} must reject an error page without rejecting the real pack"
        );
        assert!(matches!(steps[2], InstallStep::Run { .. }));
    }

    #[test]
    fn the_plan_unpacks_into_the_directory_it_downloaded_into() {
        let (what, argv, _) = run_step(2);
        assert!(what.contains("unpack"));
        assert_eq!(argv[0], "tar", "bsdtar reads the zip and ships with Windows");
        assert!(argv.contains(&"-xf".to_string()));
        assert!(argv.contains(&dir().display().to_string()));
    }

    #[test]
    fn the_plan_runs_the_vendors_setup_with_the_recipes_own_switches() {
        let (what, argv, cwd) = run_step(3);
        assert!(what.contains("setup"));
        let expected = dir().join(recipe().setup_exe).display().to_string();
        assert_eq!(
            argv[0], expected,
            "the installer must be the one inside the pack we just unpacked"
        );
        let passed: Vec<&str> = argv[1..].iter().map(|arg| arg.as_str()).collect();
        assert_eq!(
            passed.as_slice(),
            recipe().setup_args,
            "the switches live in the recipe, so the library is the single source"
        );
        assert_eq!(
            cwd,
            Some(dir()),
            "the pack's readme says to launch the setup from the unpacked folder"
        );
    }

    #[test]
    fn the_last_thing_that_runs_is_the_installer() {
        let steps = install_steps(recipe(), &dir());
        assert_eq!(steps.len(), 4, "the plan changed shape: {steps:?}");
        assert!(matches!(steps.last(), Some(InstallStep::Run { .. })));
    }

    #[test]
    fn no_step_goes_through_a_shell() {
        // A URL or a path handed to `cmd /c` or `powershell -Command` is a command
        // injection waiting for a quote character, and an unquoted URL is exactly
        // what makes a silent installer look like it worked while nothing happened.
        // Every step is an argument vector, so nothing is ever re-parsed.
        for step in install_steps(recipe(), &dir()) {
            let InstallStep::Run { argv, .. } = step else {
                continue;
            };
            let program = argv[0].to_ascii_lowercase();
            assert_ne!(program, "cmd", "{argv:?}");
            assert!(!program.ends_with("cmd.exe"), "{argv:?}");
            assert!(!program.contains("powershell"), "{argv:?}");
            assert!(
                !argv
                    .iter()
                    .any(|arg| arg == "-Command" || arg == "/c" || arg == "-c"),
                "a step that shells out: {argv:?}"
            );
        }
    }

    #[test]
    fn a_real_install_needs_consent_and_a_human_at_the_console() {
        // No flag: refuse, and name the flag that is missing.
        let missing_consent = refusal(false, true, false).expect("must refuse");
        assert!(missing_consent.contains("--yes"), "{missing_consent}");
        // `--yes` from a script, a scheduled task or a future auto-updater:
        // refuse anyway, which is what "never unattended" has to mean.
        let unattended = refusal(true, false, false).expect("must refuse");
        assert!(unattended.contains("unattended"), "{unattended}");
        // Neither one: still refuse.
        assert!(refusal(false, false, false).is_some());
        // Consent plus a person is the only way through.
        assert_eq!(refusal(true, true, false), None);
    }

    #[test]
    fn a_dry_run_is_never_blocked_because_it_changes_nothing() {
        // It downloads nothing and runs nothing, so it needs neither consent nor a
        // console — which is how a CI job, or a user piping the output, runs it.
        assert_eq!(refusal(false, false, true), None);
        assert_eq!(refusal(false, true, true), None);
        assert_eq!(refusal(true, false, true), None);
    }

    #[test]
    fn a_downloaded_file_is_named_after_the_url_it_came_from() {
        assert_eq!(
            url_file_name(recipe().url),
            "VBCABLE_Driver_Pack45.zip",
            "the pack name is derived, so it cannot disagree with the URL"
        );
        assert_eq!(url_file_name("https://example.test/a/b.bin"), "b.bin");
        assert_eq!(url_file_name(""), "");
    }

    #[test]
    fn the_preview_shows_every_path_it_will_use_even_when_they_have_spaces() {
        let steps = install_steps(recipe(), &PathBuf::from(r"C:\Users\Ana Ruiz\talkee-cable"));
        let printed = plan_lines(&steps).join("\n");
        assert!(
            printed.contains(r#""C:\Users\Ana Ruiz\talkee-cable"#),
            "a path with a space must still be readable and pasteable: {printed}"
        );
        assert!(printed.contains(recipe().url), "{printed}");
        assert!(printed.contains(recipe().setup_exe), "{printed}");
        assert!(printed.contains("1. curl"), "the steps are numbered: {printed}");
    }

    #[test]
    fn the_preview_has_one_line_per_step_so_it_cannot_hide_one() {
        // A preview that collapsed or skipped a step would be worse than no
        // preview, because it would be trusted.
        let steps = install_steps(recipe(), &dir());
        let lines = plan_lines(&steps);
        assert_eq!(lines.len(), steps.len());
        for line in &lines {
            assert!(!line.contains('\n'), "{line}");
            assert!(line.starts_with("  "), "{line}");
        }
    }

    #[test]
    fn arguments_are_quoted_only_where_a_shell_would_need_it() {
        assert_eq!(quote("curl"), "curl");
        assert_eq!(quote(r"C:\tmp\a.zip"), r"C:\tmp\a.zip");
        assert_eq!(quote(r"C:\Program Files\x"), r#""C:\Program Files\x""#);
        assert_eq!(quote(""), r#""""#);
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
    }
}
