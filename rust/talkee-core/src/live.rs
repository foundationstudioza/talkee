//! Settings that can change while the app runs, and that more than one thing can change.
//!
//! The tray, the browser dashboard and the phone can all turn the same switches, and the audio thread
//! reads them on every frame. One shared value is what keeps them from disagreeing: every writer
//! goes through [`LiveControls::set`], every reader looks at the same atomics, and a version number
//! tells the control connection when there is something new to tell the phone.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// One switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    /// Muted: audio is neither captured on the phone nor played on the PC.
    Paused,
    /// WebRTC noise suppression after echo cancellation.
    NoiseSuppression,
    /// Removing the PC's own speaker sound from the phone's microphone.
    EchoCancel,
    /// Lifting a quiet phone toward a usable level.
    AutoLevel,
}

#[derive(Debug)]
struct Inner {
    paused: AtomicBool,
    noise_suppression: AtomicBool,
    echo_cancel: AtomicBool,
    auto_level: AtomicBool,
    /// Bumped on every real change, so a watcher can ask "anything new?" with one load.
    version: AtomicU64,
    /// Bumped to ask the control connection to be dropped, so a phone that is stuck reconnects.
    reconnects: AtomicU64,
    phone: Mutex<Option<String>>,
}

/// A cheap-to-clone handle on the shared settings.
#[derive(Debug, Clone)]
pub struct LiveControls(Arc<Inner>);

impl LiveControls {
    /// Starts with the given values.
    pub fn new(noise_suppression: bool, echo_cancel: bool, auto_level: bool) -> Self {
        Self(Arc::new(Inner {
            paused: AtomicBool::new(false),
            noise_suppression: AtomicBool::new(noise_suppression),
            echo_cancel: AtomicBool::new(echo_cancel),
            auto_level: AtomicBool::new(auto_level),
            version: AtomicU64::new(1),
            reconnects: AtomicU64::new(0),
            phone: Mutex::new(None),
        }))
    }

    fn cell(&self, setting: Setting) -> &AtomicBool {
        match setting {
            Setting::Paused => &self.0.paused,
            Setting::NoiseSuppression => &self.0.noise_suppression,
            Setting::EchoCancel => &self.0.echo_cancel,
            Setting::AutoLevel => &self.0.auto_level,
        }
    }

    /// The current value.
    pub fn get(&self, setting: Setting) -> bool {
        self.cell(setting).load(Ordering::Relaxed)
    }

    /// Changes a value. Returns whether it actually changed, and only then bumps the version, so
    /// setting a value to what it already is — which every refresh of a checkbox does — is free and
    /// does not wake the phone.
    pub fn set(&self, setting: Setting, value: bool) -> bool {
        let changed = self.cell(setting).swap(value, Ordering::Relaxed) != value;
        if changed {
            self.0.version.fetch_add(1, Ordering::Release);
        }
        changed
    }

    /// Flips a value and returns the new one.
    pub fn toggle(&self, setting: Setting) -> bool {
        let now = !self.get(setting);
        self.set(setting, now);
        now
    }

    /// Increases whenever any value changes.
    pub fn version(&self) -> u64 {
        self.0.version.load(Ordering::Acquire)
    }

    /// Asks for the phone's connection to be dropped. The phone notices within a moment and pairs
    /// again from scratch, which is the cure for a connection that looks fine and carries nothing.
    pub fn request_reconnect(&self) {
        self.0.reconnects.fetch_add(1, Ordering::Release);
    }

    /// Increases each time [`LiveControls::request_reconnect`] is called.
    pub fn reconnect_requests(&self) -> u64 {
        self.0.reconnects.load(Ordering::Acquire)
    }

    /// Records which phone is connected, or none.
    pub fn set_phone(&self, name: Option<String>) {
        if let Ok(mut held) = self.0.phone.lock() {
            *held = name;
        }
    }

    /// The connected phone's name, when there is one.
    pub fn phone(&self) -> Option<String> {
        self.0.phone.lock().ok().and_then(|p| p.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_bumps_the_version_and_a_repeat_does_not() {
        let controls = LiveControls::new(true, true, true);
        let start = controls.version();
        assert!(controls.set(Setting::Paused, true));
        assert!(controls.version() > start);
        let after = controls.version();
        assert!(!controls.set(Setting::Paused, true));
        assert_eq!(controls.version(), after, "setting the same value must not notify anyone");
    }

    #[test]
    fn clones_share_one_state() {
        let a = LiveControls::new(false, false, false);
        let b = a.clone();
        b.toggle(Setting::NoiseSuppression);
        assert!(a.get(Setting::NoiseSuppression));
    }

    #[test]
    fn a_reconnect_request_is_counted_and_shared() {
        let a = LiveControls::new(true, true, true);
        let b = a.clone();
        let before = a.reconnect_requests();
        b.request_reconnect();
        assert_eq!(a.reconnect_requests(), before + 1);
    }
}
