//! The dictation hotkey: Ctrl+Win, watched with a low-level keyboard hook.
//!
//! Two ways to use it, told apart by how long the chord is held:
//!
//! * **Tap** (released within [`TAP_MS`]): dictation starts and keeps going. Pressing Ctrl+Win again
//!   is the only thing that stops it.
//! * **Hold**: dictation runs while the chord is held, and stops when it is released.
//!
//! # Why a hook and not `RegisterHotKey`
//!
//! `RegisterHotKey` reports a press and never a release, and both the tap/hold distinction and
//! hold-to-talk need the release. A `WH_KEYBOARD_LL` hook sees both.
//!
//! # Why Ctrl+Win
//!
//! Ctrl+Space is autocomplete in most editors and switches input methods on Windows; Shift+Space
//! fires by accident while typing. Ctrl+Win on its own does nothing in Windows, and the shortcuts
//! that start with it (Ctrl+Win+D, Ctrl+Win+Left, …) still work: pressing any other key while the
//! chord is held cancels the dictation and lets the shortcut through. Once a tap has latched
//! dictation on, other keys are left alone, so typing elsewhere does not end it.
//!
//! # Why its own thread
//!
//! Windows makes every keystroke on the machine wait for a low-level hook to return. On the app's
//! 20 Hz loop, any slow tick would make typing lag everywhere, so the hook gets a thread whose only
//! job is a message loop, and the callback only updates a small state machine and sends an event.

use std::sync::mpsc::Sender;

/// A chord released sooner than this after it went down was a tap: dictation stays on.
pub const TAP_MS: u64 = 400;

/// What the chord asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Start recording.
    Start,
    /// Stop recording and deliver the text.
    Stop,
    /// Another key was pressed while the chord was held: it was a shortcut, not a dictation.
    Cancel,
}

const VK_CONTROL: u32 = 0x11;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;

/// The chord's state, kept apart from the hook so it can be tested without Windows.
#[derive(Debug, Default)]
pub struct Chord {
    ctrl: bool,
    win: bool,
    /// Recording because the chord is being held; `held_since` is when it went down (ms).
    holding: bool,
    held_since: u64,
    /// Recording after a tap; only the next press of the chord stops it.
    latched: bool,
    /// This press of the chord ended a dictation; its release does nothing more.
    stopping: bool,
    /// Another key was pressed while a modifier was held; nothing starts until both are released.
    spoiled: bool,
    /// The chord fired since Win went down, so Win's release must not open the Start menu.
    mask_start_menu: bool,
}

/// What the hook has to do after a key.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reaction {
    /// Tell the dictation worker.
    pub event: Option<Event>,
    /// This is Win's release after the chord fired: send a dummy key first so Start stays shut.
    pub mask_start_menu: bool,
}

impl Chord {
    /// Feeds one key transition: `vk` is the virtual-key code, `now_ms` a monotonic clock in
    /// milliseconds. Auto-repeat downs are harmless.
    pub fn key(&mut self, vk: u32, down: bool, now_ms: u64) -> Reaction {
        let is_ctrl = matches!(vk, VK_CONTROL | VK_LCONTROL | VK_RCONTROL);
        let is_win = matches!(vk, VK_LWIN | VK_RWIN);
        let mut reaction = Reaction::default();

        if down {
            if is_ctrl || is_win {
                let was_down = self.ctrl && self.win;
                if is_ctrl {
                    self.ctrl = true;
                } else {
                    self.win = true;
                }
                if self.ctrl && self.win && !was_down && !self.spoiled {
                    self.mask_start_menu = true;
                    if self.latched {
                        // The second press after a tap: this is the stop.
                        self.latched = false;
                        self.stopping = true;
                        reaction.event = Some(Event::Stop);
                    } else if !self.holding {
                        self.holding = true;
                        self.held_since = now_ms;
                        reaction.event = Some(Event::Start);
                    }
                }
            } else if self.ctrl || self.win {
                // A shortcut while a modifier is down: Ctrl+C, Win+E, Ctrl+Win+D. Never a dictation.
                // A latched dictation is left alone: the person may simply be typing elsewhere.
                self.spoiled = true;
                if self.holding {
                    self.holding = false;
                    reaction.event = Some(Event::Cancel);
                }
            }
        } else if is_ctrl || is_win {
            let chord_was_down = self.ctrl && self.win;
            if is_ctrl {
                self.ctrl = false;
            } else {
                self.win = false;
                reaction.mask_start_menu = std::mem::take(&mut self.mask_start_menu);
            }
            if chord_was_down && self.holding {
                self.holding = false;
                if now_ms.saturating_sub(self.held_since) < TAP_MS {
                    self.latched = true; // a tap: keep recording until the next press
                } else {
                    reaction.event = Some(Event::Stop);
                }
            }
            if !self.ctrl && !self.win {
                self.spoiled = false;
                self.stopping = false;
                self.mask_start_menu = false;
            }
        }
        reaction
    }

    /// Recording was ended from somewhere else (the time limit, the window): forget it here too, so
    /// the next press starts a new dictation rather than stopping one that is already over.
    pub fn reset(&mut self) {
        self.holding = false;
        self.latched = false;
    }
}

/// Starts the hook on its own thread. Events go to `events`. Returns false if Windows refused.
#[cfg(windows)]
pub fn start(events: Sender<Event>) -> bool {
    win::start(events)
}

/// No hook off Windows.
#[cfg(not(windows))]
pub fn start(_events: Sender<Event>) -> bool {
    false
}

/// Tells the hook that no dictation is recording, so a latched tap is forgotten. See
/// [`Chord::reset`].
#[cfg(windows)]
pub fn reset() {
    win::reset();
}

/// Nothing to reset off Windows.
#[cfg(not(windows))]
pub fn reset() {}

#[cfg(windows)]
#[allow(unsafe_code)]
mod win {
    use super::{Chord, Event};
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Mutex, OnceLock};
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
        KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
        WM_SYSKEYUP,
    };

    /// An unassigned virtual-key code. Pressing it while Win is down makes Windows treat the Win
    /// release as part of a combination, so the Start menu does not open.
    const VK_UNASSIGNED: u16 = 0xE8;

    /// The hook callback has no user-data pointer, so its state lives here.
    static STATE: OnceLock<Mutex<(Chord, Sender<Event>)>> = OnceLock::new();

    pub fn start(events: Sender<Event>) -> bool {
        if STATE.set(Mutex::new((Chord::default(), events))).is_err() {
            return false; // already running
        }
        let (ready_tx, ready_rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("talkee-hotkey".into())
            .spawn(move || {
                // SAFETY: a valid callback with the HOOKPROC signature, this module's handle, and
                // thread id 0 (global, which is what WH_KEYBOARD_LL requires). The hook lives as long
                // as this thread, which runs its message loop for the life of the process.
                let hook = unsafe {
                    SetWindowsHookExW(
                        WH_KEYBOARD_LL,
                        Some(callback),
                        GetModuleHandleW(std::ptr::null()),
                        0,
                    )
                };
                let _ = ready_tx.send(!hook.is_null());
                if hook.is_null() {
                    return;
                }
                // SAFETY: a stack MSG filled in by GetMessageW and read only by these calls. The loop
                // is what lets Windows deliver keystrokes to the hook at all.
                unsafe {
                    let mut message: MSG = std::mem::zeroed();
                    while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            });
        spawned.is_ok() && ready_rx.recv().unwrap_or(false)
    }

    pub fn reset() {
        if let Some(state) = STATE.get() {
            if let Ok(mut held) = state.lock() {
                held.0.reset();
            }
        }
    }

    unsafe extern "system" fn callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            // SAFETY: for WH_KEYBOARD_LL with code >= 0, lparam points at a KBDLLHOOKSTRUCT that is
            // valid for the duration of this call.
            let info = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            // Our own typing (and the masking key below) comes back through the hook as injected
            // input; it must never move the chord.
            if info.flags & LLKHF_INJECTED == 0 {
                let down = matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
                let up = matches!(wparam as u32, WM_KEYUP | WM_SYSKEYUP);
                if down || up {
                    if let Some(state) = STATE.get() {
                        if let Ok(mut held) = state.lock() {
                            let reaction = held.0.key(info.vkCode, down, info.time as u64);
                            if let Some(event) = reaction.event {
                                let _ = held.1.send(event);
                            }
                            if reaction.mask_start_menu {
                                mask_start_menu();
                            }
                        }
                    }
                }
            }
        }
        // SAFETY: passing the hook's own arguments on, as every hook must.
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    fn mask_start_menu() {
        let key = |flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_UNASSIGNED,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let inputs = [key(0), key(KEYEVENTF_KEYUP)];
        // SAFETY: a pointer to two initialised INPUTs and their exact size.
        unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: u32 = VK_LCONTROL;
    const W: u32 = VK_LWIN;
    const D: u32 = 0x44;

    /// Keys as (vk, down, time in ms).
    fn run(chord: &mut Chord, keys: &[(u32, bool, u64)]) -> Vec<Event> {
        keys.iter()
            .filter_map(|&(vk, down, t)| chord.key(vk, down, t).event)
            .collect()
    }

    #[test]
    fn hold_then_release_stops_in_either_order() {
        let mut chord = Chord::default();
        let events = run(
            &mut chord,
            &[
                (C, true, 0),
                (W, true, 10),
                (W, false, 2000),
                (C, false, 2010),
            ],
        );
        assert_eq!(events, [Event::Start, Event::Stop]);
        let events = run(
            &mut chord,
            &[
                (W, true, 5000),
                (C, true, 5010),
                (C, false, 7000),
                (W, false, 7010),
            ],
        );
        assert_eq!(events, [Event::Start, Event::Stop]);
    }

    #[test]
    fn a_tap_latches_and_only_the_next_press_stops() {
        let mut chord = Chord::default();
        // Tap: down and up within TAP_MS. Recording starts and stays on.
        let events = run(
            &mut chord,
            &[
                (C, true, 0),
                (W, true, 20),
                (W, false, 150),
                (C, false, 160),
            ],
        );
        assert_eq!(events, [Event::Start]);
        // Typing elsewhere, even with Ctrl, does not end it.
        let events = run(
            &mut chord,
            &[
                (0x41, true, 500),
                (0x41, false, 520),
                (C, true, 600),
                (0x43, true, 610),
                (0x43, false, 620),
                (C, false, 630),
            ],
        );
        assert!(events.is_empty(), "{events:?}");
        // The next press of the chord stops it, on the press, and its release does nothing more.
        let events = run(&mut chord, &[(C, true, 9000), (W, true, 9010)]);
        assert_eq!(events, [Event::Stop]);
        let events = run(&mut chord, &[(W, false, 9900), (C, false, 9910)]);
        assert!(events.is_empty());
        // And the press after that starts a new one.
        assert_eq!(
            run(&mut chord, &[(C, true, 12000), (W, true, 12010)]),
            [Event::Start]
        );
    }

    #[test]
    fn auto_repeat_does_not_restart_or_stop() {
        let mut chord = Chord::default();
        let events = run(
            &mut chord,
            &[
                (C, true, 0),
                (W, true, 10),
                (W, true, 300),
                (C, true, 330),
                (C, true, 360),
                (W, false, 1500),
            ],
        );
        assert_eq!(events, [Event::Start, Event::Stop]);
    }

    #[test]
    fn a_shortcut_while_held_cancels_and_does_not_restart_until_released() {
        let mut chord = Chord::default();
        // Ctrl+Win+D: new virtual desktop.
        let events = run(
            &mut chord,
            &[
                (C, true, 0),
                (W, true, 10),
                (D, true, 50),
                (D, false, 60),
                (W, false, 70),
                (C, false, 80),
            ],
        );
        assert_eq!(events, [Event::Start, Event::Cancel]);
        // Not latched by the quick release: the next press is a clean start.
        assert_eq!(
            run(&mut chord, &[(C, true, 1000), (W, true, 1010)]),
            [Event::Start]
        );
    }

    #[test]
    fn a_shortcut_before_the_chord_prevents_it() {
        let mut chord = Chord::default();
        // Ctrl+C, then Win while Ctrl is still down: not a dictation.
        let events = run(
            &mut chord,
            &[
                (C, true, 0),
                (0x43, true, 10),
                (0x43, false, 20),
                (W, true, 30),
                (W, false, 40),
                (C, false, 50),
            ],
        );
        assert!(events.is_empty());
    }

    #[test]
    fn win_release_after_the_chord_masks_the_start_menu_once() {
        let mut chord = Chord::default();
        chord.key(C, true, 0);
        chord.key(W, true, 10);
        assert!(
            !chord.key(C, false, 2000).mask_start_menu,
            "Ctrl's release needs nothing"
        );
        assert!(
            chord.key(W, false, 2010).mask_start_menu,
            "Win's release must be masked"
        );
        // A plain Win tap afterwards opens Start as usual.
        chord.key(W, true, 3000);
        assert!(!chord.key(W, false, 3050).mask_start_menu);
    }

    #[test]
    fn reset_forgets_a_latched_dictation() {
        let mut chord = Chord::default();
        run(
            &mut chord,
            &[
                (C, true, 0),
                (W, true, 10),
                (W, false, 100),
                (C, false, 110),
            ],
        );
        chord.reset();
        assert_eq!(
            run(&mut chord, &[(C, true, 5000), (W, true, 5010)]),
            [Event::Start]
        );
    }

    #[test]
    fn other_keys_alone_do_nothing() {
        let mut chord = Chord::default();
        assert!(run(
            &mut chord,
            &[
                (D, true, 0),
                (D, false, 10),
                (0x10, true, 20),
                (0x10, false, 30)
            ]
        )
        .is_empty());
    }
}
