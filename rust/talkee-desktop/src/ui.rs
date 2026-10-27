//! How the app talks to the person using it: the tray when there is one, and the printed
//! lines always.
//!
//! # One type, two variants
//!
//! [`Ui`] is either a tray or an explanation of why there is not one. That means the main
//! loop has no platform conditions in it, and a headless run still says *why* it is
//! headless — which is the first thing anyone asked to debug it will want to know, and the
//! difference between "no tray was requested" and "the shell refused one".

use anyhow::{Context, Result};

use crate::engine::{self, StartReport};
use crate::state::Reading;
#[cfg(windows)]
use crate::tray;

/// The lines printed when a session starts.
///
/// Separated from the printing so the wording can be asserted on: the network line is the
/// only place a user can see which address the phone should reach, and the microphone line
/// is the only place they are told which endpoint to pick elsewhere. That line is a
/// sentence, not a button — the "select this in Discord" button is deliberately out of
/// scope for this milestone.
pub fn startup_lines(report: &StartReport) -> Vec<String> {
    let mut lines = vec![
        format!(
            "started  : playing into \"{}\" at {} Hz, {} channels ({})",
            report.endpoint, report.device_rate, report.device_channels, report.sample_format
        ),
        format!(
            "buffer   : {} frames (~{} ms of prefill); queue holds {}",
            report.target_frames,
            report.target_frames * 1000 / report.device_rate.max(1) as usize,
            report.queue_capacity
        ),
        format!("listening: {}", report.listen),
    ];
    if report.lan_addresses.is_empty() {
        lines.push(
            "network  : no LAN address found, so a phone cannot reach this PC yet — connect to \
             Wi-Fi or Ethernet"
                .to_string(),
        );
    } else {
        for address in &report.lan_addresses {
            lines.push(format!(
                "network  : a phone can reach {address}:{}",
                report.listen.port()
            ));
        }
    }
    match (&report.advertisement, &report.advertisement_problem) {
        (Some(name), _) => lines.push(format!("discoverable: {name}")),
        (None, Some(problem)) => lines.push(format!("discoverable: no ({problem})")),
        (None, None) => lines.push("discoverable: no (advertising is off)".to_string()),
    }
    if let Some(hint) = &report.capture_hint {
        lines.push(format!(
            "microphone: in other apps, select a microphone containing \"{hint}\""
        ));
    }
    // Which security mode this is running in, said plainly. The two failure modes are opposite and
    // both are invisible from the microphone end: plaintext means a stranger on this network can
    // speak through it, and `--secure` with no phone paired means it simply stays silent.
    match (&report.pairing_link, &report.pairing_note) {
        (Some(link), note) => {
            lines.push(format!("pairing  : {link}"));
            if let Some(note) = note {
                lines.push(format!("           ({note})"));
            }
        }
        (None, _) => lines.push(
            "pairing  : off — plaintext audio is accepted from anyone who can reach this port; \
             pass --secure to require a pairing"
                .to_string(),
        ),
    }
    lines
}

/// The tray, or the reason there is not one.
pub enum Ui {
    /// A live tray icon.
    #[cfg(windows)]
    Tray(Box<tray::Tray>),
    /// No tray: the printed state lines are the whole UI, and this says why.
    Headless(String),
}

/// What the UI asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAction {
    /// A live switch was turned on or off from the tray.
    Set(talkee_core::live::Setting, bool),
    /// Install the virtual microphone, because there is no cable to play into.
    GetCable,
    /// Show the code a phone scans in order to pair.
    PairPhone,
    /// Drop the phone's connection so it pairs again.
    Reconnect,
    /// Install the downloaded update.
    InstallUpdate,
    /// Shut the app down.
    Quit,
}

impl Ui {
    /// Creates the tray, or falls back to headless.
    ///
    /// A tray that cannot be created is not a reason to refuse to carry audio: the printed
    /// lines are a working fallback, and saying why is more useful than exiting.
    pub fn new(
        reading: &Reading,
        stopped: bool,
        hint: Option<&str>,
        wanted: bool,
        controls: &talkee_core::live::LiveControls,
    ) -> Ui {
        #[cfg(windows)]
        {
            if wanted {
                return match tray::Tray::new(reading, stopped, hint, controls) {
                    Ok(tray) => Ui::Tray(Box::new(tray)),
                    Err(e) => Ui::Headless(format!("no tray icon ({e:#})")),
                };
            }
            Ui::Headless("tray disabled with --no-tray".to_string())
        }
        #[cfg(not(windows))]
        {
            let _ = (reading, stopped, hint, wanted, controls);
            Ui::Headless(
                "this build has no tray backend, so the state lines below are the whole app"
                    .to_string(),
            )
        }
    }

    /// Why there is no tray, if there is not.
    pub fn headless_reason(&self) -> Option<&str> {
        match self {
            Ui::Headless(reason) => Some(reason.as_str()),
            #[cfg(windows)]
            Ui::Tray(_) => None,
        }
    }

    /// Pushes a reading at the tray, if there is one.
    pub fn update(
        &mut self,
        status: &engine::Status,
        controls: &talkee_core::live::LiveControls,
        update: &crate::update::State,
    ) -> Result<()> {
        match self {
            #[cfg(windows)]
            Ui::Tray(tray) => tray
                .update(&status.reading, status.stopped, status.hint.as_deref(), controls, update)
                .context("updating the tray icon"),
            Ui::Headless(_) => {
                let _ = (controls, update);
                Ok(())
            }
        }
    }

    /// What was clicked since the last call. Never blocks.
    pub fn poll(&self) -> Vec<UiAction> {
        match self {
            #[cfg(windows)]
            Ui::Tray(tray) => tray
                .poll()
                .into_iter()
                .map(|action| match action {
                    tray::Action::Set(setting, value) => UiAction::Set(setting, value),
                    tray::Action::GetCable => UiAction::GetCable,
                    tray::Action::PairPhone => UiAction::PairPhone,
                    tray::Action::Reconnect => UiAction::Reconnect,
                    tray::Action::InstallUpdate => UiAction::InstallUpdate,
                    tray::Action::Quit => UiAction::Quit,
                })
                .collect(),
            Ui::Headless(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{self, OutMeter};

    fn report() -> StartReport {
        StartReport {
            endpoint: "CABLE Input (VB-Audio Virtual Cable)".to_string(),
            device_rate: 48_000,
            device_channels: 2,
            sample_format: "F32".to_string(),
            target_frames: 2_880,
            queue_capacity: 11_520,
            listen: "0.0.0.0:47801".parse().expect("valid"),
            lan_addresses: vec!["192.168.0.24".parse().expect("valid")],
            capture_hint: Some("CABLE Output".to_string()),
            advertisement: Some("Talkee on DESKTOP._talkee._tcp.local.".to_string()),
            advertisement_problem: None,
            pairing_link: None,
            pairing_note: None,
        }
    }

    #[test]
    fn the_startup_lines_name_the_cable_the_address_and_the_microphone() {
        let text = startup_lines(&report()).join("\n");
        assert!(text.contains("CABLE Input"), "{text}");
        assert!(text.contains("48000"), "{text}");
        assert!(text.contains("~60 ms"), "the depth is shown in milliseconds: {text}");
        assert!(text.contains("192.168.0.24:47801"), "{text}");
        assert!(text.contains("Talkee on DESKTOP"), "{text}");
        assert!(text.contains("CABLE Output"), "{text}");
    }

    #[test]
    fn a_machine_with_no_network_says_so_rather_than_printing_nothing() {
        let broken = StartReport {
            lan_addresses: Vec::new(),
            capture_hint: None,
            advertisement: None,
            advertisement_problem: Some("no LAN address".to_string()),
            pairing_link: None,
            pairing_note: None,
            ..report()
        };
        let text = startup_lines(&broken).join("\n");
        assert!(text.contains("no LAN address"), "{text}");
        assert!(text.contains("discoverable: no"), "{text}");
        assert!(
            !text.contains("microphone:"),
            "no hint to give, so no line to give it on: {text}"
        );
    }

    #[test]
    fn a_headless_run_says_which_kind_of_headless_it_is() {
        // The fallback has to be visible in the log, or a reviewer sees an app that
        // "works" while the tray never appeared at all.
        let reading = Reading::sample(&state::idle_observation(true), OutMeter::Waiting);
        let controls = talkee_core::live::LiveControls::new(true, true, true);
        let mut asked_for_nothing = Ui::new(&reading, true, None, false, &controls);
        assert_eq!(asked_for_nothing.headless_reason(), Some("tray disabled with --no-tray"));
        assert!(
            asked_for_nothing.poll().is_empty(),
            "a headless UI has nothing to click"
        );
        assert!(asked_for_nothing
            .update(
                &crate::engine::Status {
                    reading,
                    stopped: true,
                    problem: None,
                    hint: None,
                },
                &controls,
                &crate::update::State::Current
            )
            .is_ok());
    }
}

