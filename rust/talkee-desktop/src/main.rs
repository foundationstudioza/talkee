//! Talkee's desktop app: what a normal person runs instead of a terminal.
//!
//! # What it is
//!
//! A tray icon that runs the same receiver `talkee host` does, in the same process:
//! `talkee-core` for the pipeline, `talkee-vmic` for the device, `talkee-discovery` for
//! the announcement. Nothing about the audio path is reimplemented here — this file is a
//! loop, a menu and a lot of printing.
//!
//! # Why it prints
//!
//! Every state transition goes to stdout with an elapsed timestamp. That is not debug
//! output left in by accident: the app has no window, so the log *is* the only complete
//! record of what the tray was showing, and it is how a run can be verified without
//! looking at a taskbar. It is also the fastest thing to ask a user for when they report
//! that "it stopped working".
//!
//! # The loop
//!
//! Twenty times a second: drain the Win32 message queue (menu clicks arrive through it),
//! act on anything the menu asked for, start or stop the session, sample both level
//! meters, and push the result at the shell. Everything that decides *what* to show is in
//! `state`, everything that touches a device is in `engine`, and everything that talks to
//! the user is in `ui`.

// A GUI program on Windows, so double-clicking it does not open a terminal. Output goes to a log
// file instead: see `logging`. `--console` brings a console back for watching it live.
#![cfg_attr(windows, windows_subsystem = "windows")]
#![deny(unsafe_code)]

mod args;
mod engine;
mod firewall;
mod icon;
mod logging;
mod message_pump;
#[cfg(windows)]
mod pairing;
#[cfg(windows)]
mod setup;
#[cfg(windows)]
mod shell;
mod settings;
mod single;
mod state;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod window;
mod ui;
mod update;
mod wizard;

use anyhow::Result;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use args::{Args, Command, USAGE};
use engine::{Engine, EngineConfig};
use state::AppState;
use ui::{Ui, UiAction};

/// How often the loop wakes up.
///
/// 20 Hz: fast enough that a menu click feels instant and the level meter moves like a
/// meter rather than a slideshow, slow enough that an idle tray icon costs nothing worth
/// measuring.
const TICK: Duration = Duration::from_millis(20);

/// Shortest gap between two level lines on stdout.
///
/// The meter changes on almost every tick while audio is flowing, and printing at 20 lines
/// a second would bury the state transitions that matter. 120 ms is about a human refresh
/// rate; the level inside the tray icon still updates at the full 20 Hz.
const LEVEL_PRINT_INTERVAL: Duration = Duration::from_millis(120);

/// How often the app says something even when nothing has changed.
///
/// Without it, a long stretch of "looking for your phone" is indistinguishable from a
/// crashed process in the log.
const HEARTBEAT: Duration = Duration::from_secs(1);

/// Writes one timestamped line.
/// Opens the Talkee window, which is where the code and the current state live.
///
/// The tray item and the first-run path both come here. One surface: whatever the user opens, they see
/// the same window, saying the same thing about the same run. If the window cannot start, the page in
/// the browser is the fallback, so there is always somewhere to scan the code.
#[cfg(windows)]
fn show_window(
    wizard: Option<&wizard::Wizard>,
    child: &mut Option<std::process::Child>,
    started: Instant,
) {
    let Some(wizard) = wizard else {
        say(started, "the wizard is not running, so there is nothing to show");
        return;
    };
    if let Err(e) = window::spawn(wizard, child) {
        let url = wizard.url();
        say(started, &format!("{e:#}; opening {url} in your browser instead"));
        let _ = shell::open(&url);
    }
}

/// Puts a virtual microphone on this machine, using the package the app ships.
///
/// Everything the user is about to meet is named *first* — the permission prompt and the
/// restart — because both look like something going wrong if they arrive unannounced. `setup`
/// holds the licence position and the reason neither of them can be removed.
#[cfg(windows)]
fn install_cable(started: Instant) {
    say(started, setup::NOTICE);
    match setup::install() {
        Ok(setup::Outcome::Installed) => say(
            started,
            "VB-CABLE's setup finished. Restart Windows to finish the job — Talkee will find the \
             cable by itself when you come back, and there is nothing else to do until your \
             phone pairs.",
        ),
        Ok(setup::Outcome::Running) => say(
            started,
            "VB-CABLE's setup is running. Approve the Windows prompt if you have not already, \
             then restart Windows — Talkee will find the cable by itself when you come back.",
        ),
        Err(e) => say(
            started,
            &format!(
                "could not install the virtual microphone: {e:#}. You can get it straight from \
                 {} — install it, restart Windows, and Talkee will find it by itself.",
                state::VB_CABLE_URL
            ),
        ),
    }
}

/// The same action on a platform this app cannot install a device on.
#[cfg(not(windows))]
fn install_cable(started: Instant) {
    say(
        started,
        "installing the virtual microphone is Windows-only in this build — there is no tray and \
         no device for the receiver to open on this platform",
    );
}

fn say(started: Instant, text: &str) {
    println!("[{}] {text}", state::format_elapsed(started.elapsed()));
}

/// The extra sentence printed when the app enters the state it exists for.
fn silent_explanation() -> String {
    format!("         the phone connected and then went quiet — {}", AppState::Silent.detail())
}

/// The tooltip line, exactly as the tray would show it on hover.
fn tooltip_line(status: &engine::Status) -> String {
    format!(
        "tooltip  : \"{}\"",
        state::tray_tooltip(&status.reading, status.stopped, status.hint.as_deref())
    )
}

/// Starts a session and prints everything it opened.
///
/// A function rather than a block because it is called from two places: at startup, and
/// again when a cable appears while the app was waiting for one. The second path is easy to
/// forget when the reporting is written inline twice.
/// Starts a session and prints everything it opened.
///
/// A function rather than a block because it is called from two places: at startup, and again when a
/// cable appears while the app was waiting for one. The second path is easy to forget when the
/// reporting is written inline twice.
///
/// Returns true when this is the first run with a new key, which is the one time the window should open
/// by itself: nobody is paired, so the code is the only thing there is to do.
fn start_session(engine: &mut Engine, wizard: Option<&wizard::Wizard>, started: Instant) -> bool {
    engine.started_attempting();
    match engine.start() {
        Ok(report) => {
            for line in ui::startup_lines(&report) {
                say(started, &line);
            }
            announce_pairing(wizard, report.pairing_link.as_deref(), started);
            report
                .pairing_note
                .as_deref()
                .is_some_and(|n| n.contains("first key") || n.contains("new one was made"))
        }
        Err(e) => {
            say(started, &format!("cannot start: {e:#}"));
            false
        }
    }
}

/// Tells the wizard which code to show, and opens it.
///
/// The page is served rather than written to a file now: it is the same surface, except that it can
/// also report what is happening and cannot go stale. Opening it at startup rather than only from the
/// tray menu matters for the same reason it did before — a code that exists only behind a menu item is
/// a code that half the people who need it never find, and showing the code is the whole of the
/// desktop side's first-run job.
fn announce_pairing(wizard: Option<&wizard::Wizard>, link: Option<&str>, started: Instant) {
    let Some(wizard) = wizard else {
        return;
    };
    wizard.set_link(link);
    let url = wizard.url();
    if link.is_none() {
        say(
            started,
            "pairing  : off — the wizard will say so; no code is needed",
        );
        return;
    }
    say(started, &format!("wizard   : {url}"));
}

/// Reads what the wizard should say out of the app's own state.
///
/// The phone's name is not included, because nothing in this build knows it yet: the handshake
/// receives it and `talkee-core` does not report it back. The wizard has the sentence for when it
/// does — `Snapshot::phone` — and until then it says "your phone" rather than inventing a name.
fn snapshot_of(engine: &Engine, status: &engine::Status) -> wizard::Snapshot {
    let state = status.reading.state;
    wizard::Snapshot {
        cable: engine.cable().installed,
        // Audio being accepted at all means a phone got through: either it proved the key, or the
        // app was started with pairing off.
        paired: matches!(state, AppState::Live | AppState::Silent),
        live: state == AppState::Live,
        phone: None,
        // The engine's one-off message, not the reading: `Reading` describes a receiver, and this is
        // the reason there is not one.
        problem: status.problem.clone(),
        level_in: status.reading.level_in,
        level_out: match status.reading.out {
            state::OutMeter::Measured(level) => level,
            _ => 0.0,
        },
    }
}

/// Runs the app until it is asked to stop.
fn run(args: Args) -> Result<()> {
    let started = Instant::now();
    println!(
        "talkee-app {} — the Talkee receiver, in the tray",
        env!("CARGO_PKG_VERSION")
    );
    say(started, "starting; Ctrl+C or the menu's Quit stops it cleanly");

    // Installed before anything is opened, so a user who changes their mind during startup
    // is not left with a device open by a process that has been killed.
    if !message_pump::install_ctrl_c_handler() {
        say(
            started,
            "note: no Ctrl+C handler is available here, so Ctrl+C will end the process \
             without releasing the device",
        );
    }

    let config = args.apply_to(EngineConfig::default());
    say(started, &format!("mDNS name: {}", config.instance));
    let mut engine = Engine::new(config);
    if let Some(problem) = engine.cable().problem.clone() {
        say(started, &format!("no virtual microphone: {problem}"));
    }

    // The wizard: a page on loopback that shows the code and what is happening. Started before the
    // first session so the code exists the moment there is something to pair with, and a failure here
    // is not fatal — the tray still works, and the app says why rather than refusing to run.
    let wizard = match wizard::Wizard::start(engine.controls()) {
        Ok(wizard) => Some(wizard),
        Err(e) => {
            say(started, &format!("no wizard: {e:#}"));
            None
        }
    };

    // Zero configuration is the product requirement: the app listens whenever it can, so
    // there is nothing to press before a phone can connect. `--no-tray` and a menu Stop are
    // the only ways to be stopped on purpose.
    let user_stopped = false;
    let mut first_run = false;
    if engine.cable().installed {
        first_run = start_session(&mut engine, wizard.as_ref(), started);
    }
    #[cfg(windows)]
    let mut window_child: Option<std::process::Child> = None;
    #[cfg(windows)]
    if first_run {
        show_window(wizard.as_ref(), &mut window_child, started);
    }
    #[cfg(not(windows))]
    let _ = first_run;

    let first = engine.tick();
    let controls = engine.controls();
    // The switches as they were left. A flag given on the command line beats what was saved, because
    // typing it was a deliberate act for this run.
    let saved = settings::load();
    controls.set(
        talkee_core::live::Setting::NoiseSuppression,
        saved.noise && !args.no_noise_suppression,
    );
    controls.set(
        talkee_core::live::Setting::EchoCancel,
        saved.echo && !args.no_echo_cancel,
    );
    controls.set(talkee_core::live::Setting::AutoLevel, saved.auto_level);
    controls.set(talkee_core::live::Setting::Paused, saved.paused);
    controls.set(talkee_core::live::Setting::GuideDone, saved.guide_done);
    let mut saved_version = controls.version();
    let mut last_firewall = firewall::State::Unknown;
    if saved.paused {
        say(started, "settings : starting muted, as it was left");
    }
    // Asks the firewall what it thinks of this program, now and then. `netsh` takes a second or two,
    // so it runs on its own thread: often while a person is still setting up (the answer changes the
    // moment they click Allow), rarely once it says allowed.
    let firewall_state = std::sync::Arc::new(std::sync::Mutex::new(firewall::State::Unknown));
    {
        let shared = std::sync::Arc::clone(&firewall_state);
        let _ = thread::Builder::new().name("talkee-firewall".into()).spawn(move || loop {
            let state = firewall::check();
            if let Ok(mut held) = shared.lock() {
                *held = state;
            }
            let pause = if state == firewall::State::Allowed { 600 } else { 10 };
            thread::sleep(Duration::from_secs(pause));
        });
    }
    // Looks for a newer release in the background and fetches it quietly. See `update`.
    let updater = update::Updater::start(!args.no_update_check);
    if let Some(wizard) = wizard.as_ref() {
        wizard.set_updater(updater.clone());
        single::announce(wizard.port(), wizard.token());
    }
    let mut ui = Ui::new(&first.reading, first.stopped, first.hint.as_deref(), args.tray, &controls);
    if let Some(reason) = ui.headless_reason() {
        say(started, &format!("headless : {reason}"));
    } else {
        say(started, "tray     : icon created");
    }
    // Printing the tooltip is not decoration: with no window, this line is the only way to
    // see what the tray says on hover, and the tooltip is where the state is put into words.
    say(started, &tooltip_line(&first));
    say(started, &first.reading.state_line());
    if first.reading.state == AppState::Silent {
        say(started, &silent_explanation());
    }

    let mut last = first;
    let mut last_state = last.reading.state;
    let mut last_bars = String::new();
    let mut last_level_at = Instant::now() - LEVEL_PRINT_INTERVAL;
    let mut last_heartbeat = Instant::now();
    let mut quitting = false;

    loop {
        // Menu clicks and icon events reach us through this thread's message queue, so the
        // pump is what makes the menu work at all. Never blocks.
        message_pump::pump();

        if message_pump::quit_requested() {
            say(started, "Ctrl+C: stopping");
            break;
        }

        for action in ui.poll() {
            match action {
                UiAction::Quit => {
                    say(started, "menu: Quit");
                    quitting = true;
                }
                UiAction::Set(setting, value) => {
                    say(started, &format!("menu: {setting:?} -> {value}"));
                    controls.set(setting, value);
                }
                UiAction::Reconnect => {
                    say(started, "menu: Reconnect phone");
                    controls.request_reconnect();
                }
                UiAction::InstallUpdate => {
                    say(started, "menu: installing the update");
                    if let Err(e) = updater.install() {
                        say(started, &format!("could not start the update: {e:#}"));
                    }
                }
                UiAction::GetCable => install_cable(started),
                UiAction::PairPhone => {
                    #[cfg(windows)]
                    show_window(wizard.as_ref(), &mut window_child, started);
                }
            }
        }
        if updater.quit_requested() {
            say(started, "an update is installing: stepping aside");
            quitting = true;
        }
        if quitting {
            break;
        }

        if !user_stopped && !engine.is_running() && engine.cable().installed && engine.ready_to_start() {
            let _ = start_session(&mut engine, wizard.as_ref(), started);
        }

        let status = engine.tick();
        // Tell the page what changed. Every tick, because it polls: pushing the same value is free,
        // and the alternative is the page and the tray disagreeing about what is happening.
        if let Some(wizard) = wizard.as_ref() {
            wizard.publish(snapshot_of(&engine, &status));
        }
        if status.reading.state != last_state {
            say(started, &status.reading.state_line());
            say(started, &tooltip_line(&status));
            if status.reading.state == AppState::Silent {
                say(started, &silent_explanation());
            }
            last_state = status.reading.state;
        }
        if let Some(problem) = &status.problem {
            say(started, &format!("problem  : {problem}"));
        }

        let now = Instant::now();
        let bars = format!("{}|{}", status.reading.in_bar(), status.reading.out_bar());
        if bars != last_bars && now.duration_since(last_level_at) >= LEVEL_PRINT_INTERVAL {
            say(started, &status.reading.level_line());
            last_bars = bars;
            last_level_at = now;
            last_heartbeat = now;
        } else if now.duration_since(last_heartbeat) >= HEARTBEAT {
            // The line that proves the app is still there while nothing is happening. It
            // carries the state too, because "no output for a minute" and "no output for a
            // minute because it is still searching" are very different bugs to chase.
            say(
                started,
                &format!(
                    "heartbeat: {} · in [{}] out [{}]",
                    status.reading.state.title(),
                    status.reading.in_bar(),
                    status.reading.out_bar()
                ),
            );
            last_heartbeat = now;
        }

        ui.update(&status, &controls, &updater.state())?;

        // Remember the switches when one has changed. Compared by version, so this is one load per
        // tick and a file write only when something actually moved.
        if controls.version() != saved_version {
            saved_version = controls.version();
            settings::save(settings::Saved {
                noise: controls.get(talkee_core::live::Setting::NoiseSuppression),
                echo: controls.get(talkee_core::live::Setting::EchoCancel),
                auto_level: controls.get(talkee_core::live::Setting::AutoLevel),
                paused: controls.get(talkee_core::live::Setting::Paused),
                guide_done: controls.get(talkee_core::live::Setting::GuideDone),
            });
        }
        if let Ok(state) = firewall_state.lock() {
            if let Some(wizard) = wizard.as_ref() {
                wizard.set_firewall(*state);
            }
            if *state != last_firewall {
                last_firewall = *state;
                say(started, &format!("firewall : {}", state.as_str()));
            }
        }
        last = status;

        if let Some(limit) = args.exit_after {
            if started.elapsed() >= limit {
                say(started, &format!("--exit-after {}s reached", limit.as_secs_f64()));
                break;
            }
        }
        thread::sleep(TICK);
    }

    say(started, "shutting down: releasing the audio device and the network name");
    engine.stop();
    drop(ui);
    say(
        started,
        &format!(
            "session  : last state {} · {} datagram(s) · {} frame(s) lost · {} underrun(s)",
            last.reading.state.as_str(),
            last.reading.datagrams,
            last.reading.lost,
            last.reading.underruns
        ),
    );
    say(started, "stopped cleanly");
    Ok(())
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // `talkee-app --window <port> <token>`: the window, in its own process. See `window`.
    #[cfg(windows)]
    if argv.first().map(String::as_str) == Some("--window") {
        let _ = logging::init(false);
        // One window. A second request to open it just does nothing: the first is already on screen.
        if !single::acquire(r"Local\TalkeeWindow") {
            return ExitCode::SUCCESS;
        }
        let (Some(port), Some(token)) = (
            argv.get(1).and_then(|p| p.parse::<u16>().ok()),
            argv.get(2).cloned(),
        ) else {
            eprintln!("talkee-app: --window needs a port and a token");
            return ExitCode::FAILURE;
        };
        return match window::run(port, token) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("talkee-app: {e:#}");
                ExitCode::FAILURE
            }
        };
    }

    match Args::parse(&argv) {
        Ok(Command::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Command::Run(args)) => match {
            // Already running? Open its window and leave, instead of fighting it for the ports and
            // adding a second, dead icon to the tray.
            #[cfg(windows)]
            if !single::acquire(r"Local\TalkeeApp") {
                if let (Some((port, token)), Ok(exe)) = (single::running_window(), std::env::current_exe()) {
                    let _ = std::process::Command::new(exe)
                        .arg("--window")
                        .arg(port.to_string())
                        .arg(token)
                        .spawn();
                }
                return ExitCode::SUCCESS;
            }
            let log = logging::init(args.console);
            println!("talkee-app: logging to {log}");
            run(*args)
        } {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("talkee-app: {e:#}");
                ExitCode::FAILURE
            }
        },
        Err(message) => {
            eprintln!("talkee-app: {message}");
            ExitCode::FAILURE
        }
    }
}
