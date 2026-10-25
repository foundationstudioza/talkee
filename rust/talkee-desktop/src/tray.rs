//! The Windows tray icon: the icon, the tooltip and the two menu items.
//!
//! # Why this is a thin shell
//!
//! Nothing here decides anything. It takes a [`Reading`] and pushes it at the shell, and
//! it reports clicks as [`Action`]s. Every rule about what the user should be told lives
//! in `state`, which is why the state machine can be tested without a taskbar — and why
//! this file is the only part of the app that a reviewer cannot check by reading.
//!
//! # The message pump
//!
//! A tray icon on Windows is a window with a window procedure, and the events this file
//! drains arrive through it. Nothing is delivered unless the thread pumps its message
//! queue, which is what `crate::message_pump::pump` does once per loop iteration. A tray
//! app whose main loop forgets to pump shows a perfectly good icon that ignores every
//! click, so the pump is part of this module's contract rather than an optional extra.

use anyhow::{Context, Result};
use talkee_core::live::{LiveControls, Setting};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::icon;
use crate::state::{AppState, Reading};

/// What a click asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// A switch was turned on or off. The tray flips its own check mark; this says to what.
    Set(Setting, bool),
    /// Install the virtual microphone, because there is no cable to play into.
    GetCable,
    /// Show the code a phone scans in order to pair.
    PairPhone,
    /// Install the update that has been downloaded.
    InstallUpdate,
    /// Shut the app down.
    Quit,
}

/// The label for the cable item.
///
/// A pure function so the wording can be asserted on. It is the only text in the app whose
/// meaning flips with the state, and getting it backwards would invite a user who already has
/// a cable to go and install one — or leave the user who has none with no invitation at all.
pub fn cable_item_text(state: AppState) -> &'static str {
    if state == AppState::NoCable {
        "Install VB-CABLE…"
    } else {
        "virtual microphone: ready"
    }
}

/// The footnote at the bottom of the menu.
///
/// Talkee is Talkee's work; VB-CABLE is a component it depends on, and this is deliberately the
/// quietest place it can live. It is here at all because the terms that let us ship the package
/// ask that the user be able to see and identify it as a VB-Audio application and be in a
/// position to donate — and because a donationware that nobody can find the author of is a
/// donationware that does not get paid. It is a line of text in a menu, not a badge on the
/// product.
pub fn credit_text() -> String {
    format!(
        "VB-CABLE by VB-Audio · donationware · {}",
        crate::state::VB_CABLE_HOST
    )
}

/// The tray icon, its menu, and the last thing we told the shell.
pub struct Tray {
    /// Held for the whole run: dropping this removes the icon from the notification area.
    icon: TrayIcon,
    /// Read-only state line, so the user can see the state without hovering.
    status_item: MenuItem,
    /// Which phone is connected, or that none is.
    phone_item: MenuItem,
    /// The four live switches. Check marks, because a switch should show what it is, not what
    /// pressing it would do. They are rewritten from [`LiveControls`] on every update, so a change
    /// made on the phone or in the dashboard shows up here without anyone touching the menu.
    mic_item: CheckMenuItem,
    noise_item: CheckMenuItem,
    echo_item: CheckMenuItem,
    level_item: CheckMenuItem,
    last_phone: String,
    /// "Up to date", "Downloading…" or "Restart to update". The only item whose enabled state and
    /// words both follow the updater.
    update_item: MenuItem,
    last_update_text: String,
    /// The way out of the first-run problem: no virtual microphone, and no idea where to get
    /// one. Enabled only while there is genuinely nothing to play into.
    cable_item: MenuItem,
    /// Mirrors the shell's idea of the cable item's enabled state, so a 20 Hz loop does not
    /// re-set it on every tick.
    cable_enabled: bool,
    /// The code a phone scans. Held and never read: always present, and always enabled, because
    /// whether there is a code to show depends on how the app was started, and a menu that changes
    /// shape is harder to learn than one line that answers the question.
    _pair_item: MenuItem,
    /// The footnote naming the driver's author. Held for the shell's lifetime and never read:
    /// it is text, not a command, and its wording never changes.
    _credit_item: MenuItem,
    /// Held so the shell keeps them alive. Never read.
    _separator_after_status: PredefinedMenuItem,
    _separator_before_quit: PredefinedMenuItem,
    _separator_after_phone: PredefinedMenuItem,
    _separator_after_switches: PredefinedMenuItem,
    quit_item: MenuItem,
    last_tooltip: String,
    last_icon_key: Option<IconKey>,
    last_status_text: String,
}

/// Everything the drawn icon depends on.
///
/// Used to avoid rebuilding and re-uploading an identical bitmap on every one of the ~20
/// ticks per second: each `set_icon` is a round trip through the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IconKey {
    state: AppState,
    in_cells: usize,
    out_cells: Option<usize>,
    stopped: bool,
}

impl IconKey {
    fn of(reading: &Reading, stopped: bool) -> Self {
        Self {
            state: reading.state,
            in_cells: crate::state::level_cells(reading.level_in),
            out_cells: reading.out.cells(),
            stopped,
        }
    }
}

/// Builds a shell icon from a reading.
fn bitmap(reading: &Reading, stopped: bool) -> Result<Icon> {
    let rgba = if stopped {
        icon::draw_stopped(reading)
    } else {
        icon::draw(reading)
    };
    Icon::from_rgba(rgba.pixels, rgba.width, rgba.height).context("building the tray bitmap")
}

/// What the update item says.
fn update_text(state: &crate::update::State) -> String {
    match state {
        crate::update::State::Current => format!("Talkee {} · up to date", crate::update::CURRENT),
        crate::update::State::Downloading(v) => format!("Downloading update {v}…"),
        crate::update::State::Ready { version, .. } => format!("Restart to update to {version}"),
    }
}

/// The second line of the menu: who is on the other end.
fn phone_text(controls: &LiveControls) -> String {
    match controls.phone() {
        Some(name) => format!("Phone: {name}"),
        None => "No phone connected".to_string(),
    }
}

/// Makes a check mark say `want`, touching the shell only when it is wrong.
fn sync_check(item: &CheckMenuItem, want: bool) {
    if item.is_checked() != want {
        item.set_checked(want);
    }
}

/// The tooltip for a reading. The state is the first thing in it.
pub fn tooltip(reading: &Reading, stopped: bool, hint: Option<&str>) -> String {
    // One implementation, shared with the log line the app prints for the same moment.
    crate::state::tray_tooltip(reading, stopped, hint)
}

/// The status line shown inside the menu.
pub fn status_text(reading: &Reading, stopped: bool) -> String {
    if stopped {
        return format!("Talkee is stopped ({})", reading.state.title());
    }
    match reading.state {
        AppState::Live | AppState::Silent => format!(
            "{}: in [{}] out [{}]",
            reading.state.title(),
            reading.in_bar(),
            reading.out_bar()
        ),
        other => format!("Talkee: {}", other.title()),
    }
}

impl Tray {
    /// Creates the icon. Fails only if the shell refuses to accept one.
    pub fn new(
        reading: &Reading,
        stopped: bool,
        hint: Option<&str>,
        controls: &LiveControls,
    ) -> Result<Self> {
        let status_item = MenuItem::new(status_text(reading, stopped), false, None);
        let phone = phone_text(controls);
        let phone_item = MenuItem::new(phone.clone(), false, None);
        let mic_item = CheckMenuItem::new("Microphone on", true, !controls.get(Setting::Paused), None);
        let noise_item = CheckMenuItem::new(
            "Noise suppression",
            true,
            controls.get(Setting::NoiseSuppression),
            None,
        );
        let echo_item = CheckMenuItem::new(
            "Echo cancellation",
            true,
            controls.get(Setting::EchoCancel),
            None,
        );
        let level_item =
            CheckMenuItem::new("Auto level", true, controls.get(Setting::AutoLevel), None);
        let update_text = update_text(&crate::update::State::Current);
        let update_item = MenuItem::new(update_text.clone(), false, None);
        let separator_after_phone = PredefinedMenuItem::separator();
        let separator_after_switches = PredefinedMenuItem::separator();
        // Enabled only when it is the answer to the question the user actually has. Greyed
        // out beside a status line, it doubles as "you already have one".
        let cable_item = MenuItem::new(
            cable_item_text(reading.state),
            reading.state == AppState::NoCable,
            None,
        );
        let quit_item = MenuItem::new("Quit", true, None);
        let credit_item = MenuItem::new(credit_text(), false, None);
        let pair_item = MenuItem::new("Open Talkee…", true, None);
        // Separators because the first two rows are things to read and the rest are things to
        // press, and because Quit should not sit directly under Stop.
        let separator_after_status = PredefinedMenuItem::separator();
        let separator_before_quit = PredefinedMenuItem::separator();
        let menu = Menu::new();
        menu.append_items(&[
            &status_item,
            &phone_item,
            &separator_after_status,
            &mic_item,
            &noise_item,
            &echo_item,
            &level_item,
            &separator_after_switches,
            &pair_item,
            &cable_item,
            &update_item,
            &separator_before_quit,
            &credit_item,
            &quit_item,
        ])
        .context("building the tray menu")?;

        let tooltip = tooltip(reading, stopped, hint);
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip.clone())
            .with_icon(bitmap(reading, stopped)?)
            .build()
            .context("creating the tray icon")?;

        Ok(Self {
            icon,
            status_item,
            phone_item,
            mic_item,
            noise_item,
            echo_item,
            level_item,
            last_phone: phone,
            update_item,
            last_update_text: update_text,
            _separator_after_phone: separator_after_phone,
            _separator_after_switches: separator_after_switches,
            cable_item,
            cable_enabled: reading.state == AppState::NoCable,
            _pair_item: pair_item,
            _credit_item: credit_item,
            _separator_after_status: separator_after_status,
            _separator_before_quit: separator_before_quit,
            quit_item,
            last_tooltip: tooltip,
            last_icon_key: Some(IconKey::of(reading, stopped)),
            last_status_text: status_text(reading, stopped),
        })
    }

    /// Pushes a new reading at the shell, touching only what changed.
    pub fn update(
        &mut self,
        reading: &Reading,
        stopped: bool,
        hint: Option<&str>,
        controls: &LiveControls,
        update: &crate::update::State,
    ) -> Result<()> {
        let text = update_text(update);
        if text != self.last_update_text {
            self.update_item.set_text(text.as_str());
            self.update_item
                .set_enabled(matches!(update, crate::update::State::Ready { .. }));
            self.last_update_text = text;
        }
        // Muted reads as stopped everywhere the icon and the tooltip are concerned: the microphone
        // is not carrying a voice, and that is the one fact the tray exists to show.
        let muted = controls.get(Setting::Paused);
        let stopped = stopped || muted;

        sync_check(&self.mic_item, !muted);
        sync_check(&self.noise_item, controls.get(Setting::NoiseSuppression));
        sync_check(&self.echo_item, controls.get(Setting::EchoCancel));
        sync_check(&self.level_item, controls.get(Setting::AutoLevel));
        let phone = phone_text(controls);
        if phone != self.last_phone {
            self.phone_item.set_text(phone.as_str());
            self.last_phone = phone;
        }

        let tooltip = tooltip(reading, stopped, hint);
        if tooltip != self.last_tooltip {
            self.icon
                .set_tooltip(Some(tooltip.as_str()))
                .context("setting the tray tooltip")?;
            self.last_tooltip = tooltip;
        }

        let key = IconKey::of(reading, stopped);
        if Some(key) != self.last_icon_key {
            self.icon
                .set_icon(Some(bitmap(reading, stopped)?))
                .context("setting the tray icon")?;
            self.last_icon_key = Some(key);
        }

        let status = status_text(reading, stopped);
        if status != self.last_status_text {
            self.status_item.set_text(status.as_str());
            self.last_status_text = status;
        }

        // The cable item is the only action that matters when there is nothing to play into,
        // and the only one that should look inert otherwise.
        let cable_text = cable_item_text(reading.state);
        if self.cable_item.text() != cable_text {
            self.cable_item.set_text(cable_text);
        }
        let cable_wanted = reading.state == AppState::NoCable;
        if self.cable_enabled != cable_wanted {
            self.cable_item.set_enabled(cable_wanted);
            self.cable_enabled = cable_wanted;
        }
        Ok(())
    }

    /// Reports what was clicked since the last call. Never blocks.
    pub fn poll(&self) -> Vec<Action> {
        let mut actions = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id() == self.quit_item.id() {
                actions.push(Action::Quit);
            } else if event.id() == self.cable_item.id() {
                actions.push(Action::GetCable);
            } else if event.id() == self.update_item.id() {
                actions.push(Action::InstallUpdate);
            } else if event.id() == self._pair_item.id() {
                actions.push(Action::PairPhone);
            } else if event.id() == self.mic_item.id() {
                // The check mark has already flipped itself by the time the event arrives.
                actions.push(Action::Set(Setting::Paused, !self.mic_item.is_checked()));
            } else if event.id() == self.noise_item.id() {
                actions.push(Action::Set(Setting::NoiseSuppression, self.noise_item.is_checked()));
            } else if event.id() == self.echo_item.id() {
                actions.push(Action::Set(Setting::EchoCancel, self.echo_item.is_checked()));
            } else if event.id() == self.level_item.id() {
                actions.push(Action::Set(Setting::AutoLevel, self.level_item.is_checked()));
            }
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Observation, OutMeter};
    use std::time::Duration;

    fn reading(state: AppState, level_in: f32, out: OutMeter) -> Reading {
        let (cable_present, datagrams, since) = match state {
            AppState::NoCable => (false, 0, Duration::ZERO),
            AppState::Searching => (true, 0, Duration::ZERO),
            AppState::Live => (true, 100, Duration::ZERO),
            AppState::Silent => (true, 100, Duration::from_secs(2)),
        };
        let observation = Observation {
            cable_present,
            datagrams,
            since_last_datagram: since,
            quiet_for: Duration::ZERO,
            level_in,
            level_out: 0.0,
            refused: 0,
        };
        let reading = Reading::sample(&observation, out);
        assert_eq!(reading.state, state, "fixture produced the wrong state");
        reading
    }

    #[test]
    fn a_stopped_app_never_claims_one_of_the_four_states() {
        // The four states are conditions of a running receiver. With nothing listening the
        // honest word is "stopped", followed by what the user would get if they pressed
        // Start.
        for state in AppState::ALL {
            let text = tooltip(&reading(state, 0.2, OutMeter::Measured(0.2)), true, None);
            assert!(text.starts_with("Talkee — stopped"), "{text}");
            assert!(text.contains(state.title()), "{text}");
            // The state name must be attributed to what Start *would* do, and must not
            // arrive before the word "stopped". Asserting that the text simply cannot
            // contain "live" was wrong: naming the state is the point of that clause.
            let stopped_at = text.find("stopped").expect("the stopped claim");
            let named_at = text.find(state.title()).expect("the state is named");
            assert!(
                named_at > stopped_at,
                "a stopped receiver must read as stopped before anything else: {text}"
            );
        }
    }

    #[test]
    fn the_credit_names_the_author_without_claiming_the_product() {
        // The terms that let us ship VB-CABLE ask that the user can see and identify it as a
        // VB-Audio application and can donate. This is where that lives, and it is deliberately
        // one quiet line: VB-CABLE is a component Talkee depends on, not its author.
        let credit = credit_text();
        assert!(credit.contains("VB-Audio"), "must name the author: {credit}");
        assert!(
            credit.contains("donationware"),
            "must say it is a donationware, or there is no invitation to pay: {credit}"
        );
        assert!(
            credit.contains("vb-audio.com"),
            "must give the origin, which the terms ask for by name: {credit}"
        );
        assert!(
            credit.len() < 64,
            "it is a footnote, not a banner: {} chars",
            credit.len()
        );
    }

    #[test]
    fn the_cable_item_only_offers_help_when_there_is_no_cable() {
        // The one label in the app whose meaning flips with the state. Backwards, it would
        // invite a user who already has a cable to install one, or leave the user who needs it
        // with nothing to press.
        assert_eq!(cable_item_text(AppState::NoCable), "Install VB-CABLE…");
        for state in AppState::ALL {
            if state != AppState::NoCable {
                assert_ne!(
                    cable_item_text(state),
                    cable_item_text(AppState::NoCable),
                    "{state:?} must not be invited to install a cable it already has"
                );
            }
        }
    }

    #[test]
    fn a_running_app_names_its_state_in_the_tooltip() {
        for state in AppState::ALL {
            let text = tooltip(&reading(state, 0.2, OutMeter::Measured(0.2)), false, None);
            assert!(
                text.to_lowercase().contains(&state.title().to_lowercase()),
                "{state:?} is not named in its own tooltip: {text}"
            );
            assert!(text.chars().count() <= 120, "{text}");
        }
    }

    #[test]
    fn the_menu_line_carries_the_difficult_states_and_the_meter() {
        let live = reading(AppState::Live, 0.3, OutMeter::Measured(0.3));
        let text = status_text(&live, false);
        assert!(text.contains("live"), "{text}");
        assert!(text.contains('['), "the menu shows the meter too: {text}");

        let silent = reading(AppState::Silent, 0.0, OutMeter::Measured(0.0));
        assert!(status_text(&silent, false).contains("silent"));
        assert!(status_text(&silent, true).contains("stopped"));
    }

    #[test]
    fn the_icon_is_only_rebuilt_when_it_would_change() {
        let quiet = reading(AppState::Live, 0.0, OutMeter::Measured(0.0));
        let loud = reading(AppState::Live, 0.5, OutMeter::Measured(0.5));
        assert_eq!(IconKey::of(&quiet, false), IconKey::of(&quiet, false));
        assert_ne!(IconKey::of(&quiet, false), IconKey::of(&loud, false));
        assert_ne!(IconKey::of(&quiet, false), IconKey::of(&quiet, true));
        let silent = reading(AppState::Silent, 0.0, OutMeter::Measured(0.0));
        assert_ne!(IconKey::of(&quiet, false), IconKey::of(&silent, false));
    }
}