//! Which virtual audio device are we aiming at, and may we legally ship it?
//!
//! # Why this is its own module with its own tests
//!
//! The hardest part of this project is that Windows has no user-mode way to
//! create a microphone. Microsoft's ACX documentation is explicit that it is
//! *"built using the Kernel Mode Driver Framework (KMDF) and not the User Mode
//! Driver Framework (UMDF)"*. So until we ship our own signed kernel driver
//! (M6), we render into somebody else's virtual cable — and the details of
//! *which* one differ per product, per version and per install.
//!
//! Getting this wrong produces the worst possible bug report: the app runs, the
//! meters move, and nothing comes out of the other side. So detection is a pure
//! function over a device name, and it is tested.
//!
//! # The state machine, and why the answers are data
//!
//! "Is a usable cable installed?" has three answers, not two, and they want
//! different *instructions* rather than different field values — see
//! [`CableState`]. "May we fix it for the user?" has exactly one answer yes, for
//! exactly one product, and that is a licensing fact this module carries as data
//! ([`InstallRecipe`]) so a test can hold it still.
//!
//! Nothing in this module touches a device, the network or a process. Everything
//! a user is told, and everything `talkee cable install` would run, is derived
//! from the tables below by pure functions.

use crate::{DeviceInfo, DeviceKind};

/// A known third-party virtual audio cable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CableFamily {
    /// Stable identifier, safe to log and to key config off.
    pub id: &'static str,
    /// Name to show a human.
    pub label: &'static str,
    /// Substrings identifying the **render** endpoint: the one we write into.
    pub render_patterns: &'static [&'static str],
    /// Substring identifying the **capture** endpoint: the fake microphone the
    /// user selects in Discord, Zoom or OBS.
    pub capture_pattern: &'static str,
    /// Whether we are permitted to bundle or silently install it.
    pub redistributable: bool,
    /// One-line licensing or setup note to surface in the UI.
    pub notes: &'static str,
}

/// Every family we know how to drive.
///
/// `render_patterns` are matched case-insensitively as substrings against the
/// endpoint's friendly name, which on Windows looks like
/// `CABLE Input (VB-Audio Virtual Cable)`.
pub const FAMILIES: &[CableFamily] = &[
    CableFamily {
        id: "vb-cable",
        label: "VB-CABLE",
        render_patterns: &["CABLE Input"],
        capture_pattern: "CABLE Output",
        redistributable: true,
        notes: "Free donationware. VB-Audio explicitly permits redistributing and \
                silently installing VB-CABLE with another app, provided the user can \
                see that it is VB-Audio and can donate if they find it useful.",
    },
    // A+B and C+D are one family here because they share the wording that
    // decides their licence: VB-Audio states that "VB-CABLE A+B or C+D cannot be
    // distributed or bundled with other product". Installing them is fine;
    // shipping them is not, and neither is fetching them for the user.
    CableFamily {
        id: "vb-cable-16ch",
        label: "VB-CABLE A+B (16 channel)",
        render_patterns: &[
            "CABLE In 16ch",
            "CABLE-A Input",
            "CABLE-B Input",
            "CABLE-C Input",
            "CABLE-D Input",
        ],
        capture_pattern: "CABLE Out 16ch",
        redistributable: false,
        notes: "Do NOT bundle this one. VB-Audio's terms state that the A+B and C+D \
                packages 'cannot be distributed or bundled with other product'. \
                Installing it is fine; shipping it is not.",
    },
    // Deliberately non-redistributable even though the README records that plain
    // Voicemeeter is "free to distribute as-is". A device name cannot tell us
    // which edition is installed, and the licence page singles Potato out:
    // "Voicemeeter Potato package cannot be distributed or bundled with other
    // product". Reading it the optimistic way would mean shipping Potato by
    // accident, which is a licence breach, while reading it this way costs a user
    // one manual install.
    CableFamily {
        id: "voicemeeter",
        label: "Voicemeeter",
        render_patterns: &[
            "Voicemeeter Input",
            "Voicemeeter Aux Input",
            "Voicemeeter VAIO3 Input",
        ],
        capture_pattern: "Voicemeeter Out B1",
        redistributable: false,
        notes: "Free to distribute as-is under VB-Audio's donationware model, but \
                Voicemeeter Potato cannot be bundled. The B1 bus is the one that \
                appears as a recording device.",
    },
    CableFamily {
        id: "vac",
        label: "Virtual Audio Cable (VAC)",
        render_patterns: &["Line 1 (Virtual Audio Cable)", "Virtual Audio Cable"],
        capture_pattern: "Line 1 (Virtual Audio Cable)",
        redistributable: false,
        notes: "Eugene Muzychenko's paid product, licensed per machine.",
    },
    CableFamily {
        id: "blackhole-16ch",
        label: "BlackHole 16ch",
        render_patterns: &["BlackHole 16ch"],
        capture_pattern: "BlackHole 16ch",
        redistributable: false,
        notes: "GPL-3.0. Usable only if the whole application is GPL-3.0.",
    },
    CableFamily {
        id: "blackhole-2ch",
        label: "BlackHole 2ch",
        render_patterns: &["BlackHole 2ch"],
        capture_pattern: "BlackHole 2ch",
        redistributable: false,
        notes: "GPL-3.0. Usable only if the whole application is GPL-3.0; \
                otherwise contact Existential Audio for a licence.",
    },
];

/// Substrings that strongly suggest a device is a virtual cable even when we do
/// not have a curated entry for the exact product.
const VIRTUAL_HINTS: &[&str] = &[
    "virtual cable",
    "vb-audio",
    "voicemeeter",
    "blackhole",
    "soundflower",
    "loopback",
    "virtual audio",
];

/// Finds the family a render endpoint belongs to.
///
/// Returns `None` for real hardware, and also for a virtual device we have no
/// curated entry for. Callers should still accept such a device — see
/// [`looks_virtual`] — but should warn, because an uncurated cable usually means
/// the user has to work out which capture endpoint to select.
pub fn detect_render(name: &str) -> Option<&'static CableFamily> {
    let lower = name.to_ascii_lowercase();
    FAMILIES.iter().find(|family| {
        family
            .render_patterns
            .iter()
            .any(|pattern| lower.contains(&pattern.to_ascii_lowercase()))
    })
}

/// True if the name looks like a virtual device rather than real hardware.
pub fn looks_virtual(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    VIRTUAL_HINTS.iter().any(|hint| lower.contains(hint))
}

/// Families we are permitted to install on the user's behalf.
pub fn bundlable() -> impl Iterator<Item = &'static CableFamily> {
    FAMILIES.iter().filter(|f| f.redistributable)
}

/// The family id we offer when the machine has no cable at all.
///
/// Named rather than written twice, so [`RECIPES`] and [`FAMILIES`] cannot drift:
/// a typo in one would silently remove the only install offer we are allowed to
/// make, which is the difference between a working microphone and telling the
/// user to go and fix it themselves.
const VB_CABLE_ID: &str = "vb-cable";

/// Everything needed to obtain one family — and the licence conditions attached.
///
/// # Why this is a second table rather than fields on `CableFamily`
///
/// [`CableFamily::redistributable`] is a *licensing* fact. This is the
/// *mechanism*. Keeping them apart lets a test assert the two agree in **both**
/// directions, which is what catches the mistake that actually happens: someone
/// adds a download recipe for a cable we are not allowed to ship, and the entry
/// looks fine because nothing in the table contradicts itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallRecipe {
    /// [`CableFamily::id`] this recipe installs.
    pub family_id: &'static str,
    /// Direct download of the redistributable, as linked from the vendor's own
    /// product page.
    ///
    /// Measured, not remembered: `HEAD` on this URL returns HTTP 200,
    /// `Content-Length: 1318877`, so the pack we would fetch is 1.29 MB.
    pub url: &'static str,
    /// The installer inside the pack that handles 64-bit Windows.
    ///
    /// Read out of the pack's own central directory rather than guessed. The same
    /// pack also ships `VBCABLE_Setup.exe`, which its readme says "will do nothing
    /// on 64 bit O/S".
    pub setup_exe: &'static str,
    /// Arguments passed to [`setup_exe`](Self::setup_exe).
    ///
    /// **UNVERIFIED.** VB-Audio publishes no switch list anywhere we could reach:
    /// the pack's readme documents none — it says only to launch the setup in
    /// administrator mode — the reference manual draws its text as glyphs, and
    /// the licensing page refers to "VB-CABLE silent installation" without naming
    /// a single option. `-i` (install) and `-h` (hide the window) are the switches
    /// automated deployments of this pack use. The failure mode if a future pack
    /// changes them is benign and visible: the vendor's setup window appears
    /// instead of running hidden, in front of the user who asked for it.
    pub setup_args: &'static [&'static str],
    /// Whether we may embed this pack inside our own installer.
    ///
    /// `false` even for VB-CABLE, and that is not timidity. VB-Audio's licence
    /// page allows distributing VB-CABLE with an application and "possibly embed
    /// it in your installation package (VB-CABLE silent installation)", but only
    /// while the donationware model stays applicable; the readme inside the pack
    /// is stricter still — "It is not allowed to integrate the VB-CABLE package in
    /// another software installation procedure without Author agreement." So we
    /// download and run the vendor's own setup, in the user's own session, and
    /// never from ours.
    pub can_embed: bool,
    /// What the user must be told before consenting.
    ///
    /// Not decoration: the licence page makes the wording load-bearing. Being able
    /// to "see and identify VB-CABLE as VB-Audio application" and to "donate/pay
    /// license if finding it useful" is the *condition* on which distribution is
    /// permitted, so this text is the licence being satisfied.
    pub consent_note: &'static str,
}

/// The recipes we are permitted to act on. One entry, and it is VB-CABLE.
pub const RECIPES: &[InstallRecipe] = &[InstallRecipe {
    family_id: VB_CABLE_ID,
    url: "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack45.zip",
    setup_exe: "VBCABLE_Setup_x64.exe",
    setup_args: &["-i", "-h"],
    can_embed: false,
    consent_note: "VB-CABLE is VB-Audio's donationware virtual audio cable, from www.vb-cable.com. \
                   VB-CABLE is a donationware: all participations are welcome. Talkee downloads \
                   the pack from vb-audio.com and runs VB-Audio's own setup program; the pack is \
                   not bundled inside Talkee. Installing it adds a signed kernel-mode audio \
                   driver, so Windows will ask for administrator rights, and the new device only \
                   appears after a reboot.",
}];

/// The curated family with this id, if we have one.
pub fn family_by_id(id: &str) -> Option<&'static CableFamily> {
    FAMILIES.iter().find(|family| family.id == id)
}

/// The recipe for a family, if we may install it ourselves.
pub fn install_recipe(family_id: &str) -> Option<&'static InstallRecipe> {
    RECIPES.iter().find(|recipe| recipe.family_id == family_id)
}

/// Families we may download and install on the user's behalf, with their recipes.
///
/// The same two-condition gate as [`install_plan_for`], listed rather than
/// decided.
pub fn installable() -> impl Iterator<Item = (&'static CableFamily, &'static InstallRecipe)> {
    RECIPES.iter().filter_map(|recipe| {
        let family = family_by_id(recipe.family_id)?;
        family.redistributable.then_some((family, recipe))
    })
}

/// What it would take to get a cable onto a machine that has none.
///
/// # This is the gate
///
/// Every path that could install anything for the user goes through here, and it
/// requires *both* a recipe and a family licensed to be shipped. A test walks
/// [`FAMILIES`] asserting that only VB-CABLE ever comes back as
/// [`InstallPlan::WeInstall`] — so the licensing rule is enforced by the shape of
/// the data rather than by every future reader remembering it.
pub fn install_plan_for(family: &'static CableFamily) -> InstallPlan {
    match install_recipe(family.id) {
        Some(recipe) if family.redistributable => InstallPlan::WeInstall { family, recipe },
        _ => InstallPlan::UserInstalls { family },
    }
}

/// What this machine can give Talkee to play into, and what to do about it.
///
/// # Why an enum rather than a struct of `Option`s
///
/// `Uncurated` and `Missing` are different *instructions*, not different field
/// values: with `Uncurated` audio already flows and the user merely has to name
/// the endpoint, while with `Missing` nothing can come out of the far side until
/// something is installed. A struct with `family: None` would make every caller
/// re-derive that distinction, and getting it wrong means either refusing to run
/// on a working machine or telling a user with no cable that all is well.
///
/// # What this deliberately does not check
///
/// Whether the cable's *capture* half is also present. VB-CABLE's packs name
/// their two halves differently — the free pack's fake microphone is
/// `CABLE Output`, while A+B gives one output per cable — and we cannot verify
/// those names without the hardware in front of us. A false "not installed" on a
/// machine where audio works is worse than a hint the user can ignore.
///
/// # Using it
///
/// ```
/// use talkee_vmic::cable::{family_by_id, CableState, InstallPlan};
///
/// // Something we recognise and already have: usable, nothing to install, and
/// // licensed for us to ship. Built by hand rather than probed, so this runs
/// // anywhere.
/// let vb_cable = family_by_id("vb-cable").expect("curated");
/// let ready = CableState::Ready {
///     endpoint: "CABLE Input (VB-Audio Virtual Cable)".to_string(),
///     family: vb_cable,
/// };
/// assert!(ready.is_usable());
/// assert_eq!(ready.may_redistribute(), Some(true));
/// assert_eq!(ready.install_plan(), InstallPlan::NothingToDo);
///
/// // Nothing installed: not usable, and the only thing we would ever offer to
/// // install is that one cable.
/// let missing = CableState::Missing;
/// assert!(!missing.is_usable());
/// assert_eq!(missing.may_redistribute(), None);
/// assert!(missing.install_plan().we_install());
///
/// // Whatever the state, the advice is the same text every surface prints.
/// assert!(missing
///     .next_steps()
///     .iter()
///     .any(|line| line.contains("talkee cable install")));
/// assert!(!ready
///     .next_steps()
///     .iter()
///     .any(|line| line.contains("talkee cable install")));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CableState {
    /// A family we have a curated entry for is installed.
    Ready {
        /// Render endpoint we would play into.
        endpoint: String,
        /// The recognised product.
        family: &'static CableFamily,
    },
    /// A virtual render endpoint exists that we have no curated entry for.
    ///
    /// Usable: audio will flow. But we cannot tell the user which capture endpoint
    /// to select, because we do not know the product.
    Uncurated {
        /// Render endpoint we would play into.
        endpoint: String,
    },
    /// No virtual render endpoint at all.
    Missing,
}

/// What it would take to go from a [`CableState`] to a working microphone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallPlan {
    /// Already usable. Nothing to install, and we should not touch it.
    NothingToDo,
    /// We may download and run the vendor's setup, in the user's own session,
    /// after they have explicitly confirmed.
    WeInstall {
        /// The family we would install.
        family: &'static CableFamily,
        /// How to obtain it.
        recipe: &'static InstallRecipe,
    },
    /// Only the user may install this one. We must never fetch, bundle or run it.
    UserInstalls {
        /// The family we may not install.
        family: &'static CableFamily,
    },
}

impl InstallPlan {
    /// The family this plan is about, if any.
    pub fn family(&self) -> Option<&'static CableFamily> {
        match self {
            InstallPlan::NothingToDo => None,
            InstallPlan::WeInstall { family, .. } | InstallPlan::UserInstalls { family } => {
                Some(family)
            }
        }
    }

    /// True if we are the ones who would do the installing.
    ///
    /// The one question the CLI must never get wrong, so it is a single method
    /// rather than a `matches!` at each call site.
    pub fn we_install(&self) -> bool {
        matches!(self, InstallPlan::WeInstall { .. })
    }
}

impl CableState {
    /// True if we have somewhere to play audio right now.
    ///
    /// Independent of `redistributable`, deliberately: a cable the user installed
    /// themselves is exactly as usable as one we could have installed for them,
    /// and half our supported products only ever work that way.
    pub fn is_usable(&self) -> bool {
        !matches!(self, CableState::Missing)
    }

    /// The recognised product, if we recognise what is installed.
    pub fn family(&self) -> Option<&'static CableFamily> {
        match self {
            CableState::Ready { family, .. } => Some(family),
            CableState::Uncurated { .. } | CableState::Missing => None,
        }
    }

    /// The render endpoint we would play into, if there is one.
    pub fn endpoint(&self) -> Option<&str> {
        match self {
            CableState::Ready { endpoint, .. } | CableState::Uncurated { endpoint } => {
                Some(endpoint)
            }
            CableState::Missing => None,
        }
    }

    /// Whether we may redistribute what is installed.
    ///
    /// `None` when we do not recognise the cable. Guessing on a licensing question
    /// is worse than not answering it: an unnamed virtual endpoint might be any
    /// product, under any licence.
    pub fn may_redistribute(&self) -> Option<bool> {
        self.family().map(|family| family.redistributable)
    }

    /// What we would do about a machine in this state.
    pub fn install_plan(&self) -> InstallPlan {
        match self {
            CableState::Ready { .. } | CableState::Uncurated { .. } => InstallPlan::NothingToDo,
            // Nothing is installed, so the question is which cable to put there —
            // and that answer belongs to `install_plan_for`, not to this match.
            CableState::Missing => family_by_id(VB_CABLE_ID)
                .map(install_plan_for)
                .unwrap_or(InstallPlan::NothingToDo),
        }
    }

    /// One line naming this state, for a probe table, a log or a terminal.
    pub fn summary(&self) -> String {
        match self {
            CableState::Ready { endpoint, family } => format!(
                "ready: {} ({}) is installed — Talkee will play into \"{endpoint}\"",
                family.label, family.id
            ),
            CableState::Uncurated { endpoint } => format!(
                "usable, but unrecognised: \"{endpoint}\" looks virtual and we have no entry for it"
            ),
            CableState::Missing => "no virtual audio cable is installed".to_string(),
        }
    }

    /// The lines a user must read to get from here to working audio.
    ///
    /// # Why this is in the library and not in the CLI
    ///
    /// Three surfaces want the same answer — `talkee probe`, `talkee cable
    /// status`, and the M3 GUI — and advice that differs per surface is advice
    /// that is wrong on two of them. It also keeps the licensing text in one
    /// place: the alternative a user is offered must be the same alternative
    /// everywhere, spelling out the same reasons.
    pub fn next_steps(&self) -> Vec<String> {
        match self {
            CableState::Ready { endpoint, family } => {
                let mut lines = vec![
                    format!("Talkee will play into : {endpoint}"),
                    format!(
                        "Select as microphone  : something containing \"{}\"",
                        family.capture_pattern
                    ),
                    format!("  {} — {}", family.label, family.notes),
                ];
                if !family.redistributable {
                    // Worth saying out loud, because the user may otherwise wonder
                    // why Talkee made them do it by hand.
                    lines.push(
                        "  You installed this one yourself, which is the only way we could \
                         get it: we may not ship or silently install it."
                            .to_string(),
                    );
                }
                lines.extend([
                    String::new(),
                    "Next:".to_string(),
                    "  1. talkee host".to_string(),
                    "  2. talkee send --to 127.0.0.1:47801     (stands in for the phone)".to_string(),
                    format!(
                        "  3. In Audacity or OBS, record from \"{}\"",
                        family.capture_pattern
                    ),
                ]);
                lines
            }
            CableState::Uncurated { endpoint } => vec![
                format!("Found a virtual output device we do not recognise: {endpoint}"),
                String::new(),
                "You can still use it, but you have to work out which of the capture".to_string(),
                "endpoints above is its other half, and then name it explicitly:".to_string(),
                format!("  talkee host --device \"{endpoint}\""),
            ],
            CableState::Missing => {
                let mut lines = vec![
                    "No virtual audio cable is installed, so there is nothing for Talkee to"
                        .to_string(),
                    "play into. This is not a Talkee bug and not something a setting can fix:"
                        .to_string(),
                    "Windows has no user-mode way to create a microphone, so a device appears in"
                        .to_string(),
                    "Discord/Zoom/OBS only because a signed kernel driver put it there. Ours is a"
                        .to_string(),
                    "later milestone; until then Talkee plays into an existing cable."
                        .to_string(),
                    String::new(),
                    "Fix it from here:".to_string(),
                ];
                for (family, recipe) in installable() {
                    lines.push(format!(
                        "  talkee cable install --yes        download and run VB-Audio's own setup \
                         for {family_label}",
                        family_label = family.label
                    ));
                    lines.push(format!("  the download: {}", recipe.url));
                }
                lines.push(
                    "  talkee cable install --dry-run    print exactly that, and do nothing"
                        .to_string(),
                );
                lines.push(String::new());
                lines.push(
                    "Or install a cable yourself — these all work with Talkee, and we may not"
                        .to_string(),
                );
                lines.push("fetch them for you, for the reasons given:".to_string());
                for family in FAMILIES.iter().filter(|family| !family.redistributable) {
                    lines.push(format!("  {} — {}", family.label, family.notes));
                }
                lines
            }
        }
    }
}

/// Derives the state from an already-enumerated device list.
///
/// # Why this is separate from `lib::cable_state`
///
/// `talkee probe` prints the endpoint table, so it has already enumerated. A
/// constructor over that list means probe cannot disagree with its own output,
/// and it makes the whole state machine a pure function that tests drive with
/// synthetic device names — no sound card, no driver, no machine.
///
/// The search rule is the one the desktop side already uses: the first **render**
/// endpoint we recognise wins, and a virtual device we cannot name still counts
/// as usable. A capture-only endpoint never does, because we can play into a
/// cable but not out of one.
pub fn cable_state_from(devices: &[DeviceInfo]) -> CableState {
    for device in devices {
        if device.kind != DeviceKind::Render {
            continue;
        }
        if let Some(family) = detect_render(&device.name) {
            return CableState::Ready {
                endpoint: device.name.clone(),
                family,
            };
        }
    }
    for device in devices {
        if device.kind == DeviceKind::Render && device.is_virtual {
            return CableState::Uncurated {
                endpoint: device.name.clone(),
            };
        }
    }
    CableState::Missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_vb_cable_by_windows_friendly_name() {
        // Exactly the string this machine reports for its render endpoint.
        let family = detect_render("CABLE Input (VB-Audio Virtual Cable)").unwrap();
        assert_eq!(family.id, "vb-cable");
        assert_eq!(family.capture_pattern, "CABLE Output");
    }

    #[test]
    fn recognises_the_16_channel_variant_which_we_may_not_bundle() {
        let family = detect_render("CABLE In 16ch (VB-Audio Virtual Cable)").unwrap();
        assert_eq!(family.id, "vb-cable-16ch");
        assert!(
            !family.redistributable,
            "A+B must never be marked redistributable"
        );
    }

    #[test]
    fn plain_input_never_matches_the_16ch_entry() {
        // A prefix-matching bug here would silently pick the wrong capture
        // device, which is precisely the "no sound" report we are avoiding.
        assert_eq!(
            detect_render("CABLE Input (VB-Audio Virtual Cable)").unwrap().id,
            "vb-cable"
        );
    }

    #[test]
    fn cabledash_inputs_match_the_16ch_family() {
        // C and D are the same product line under the same licence, and none of
        // these names contains the free pack's "CABLE Input", so the ordering of
        // FAMILIES cannot sneak them into the redistributable entry.
        for name in [
            "CABLE-A Input (VB-Audio Virtual Cable)",
            "CABLE-B Input (VB-Audio Virtual Cable)",
            "CABLE-C Input (VB-Audio Virtual Cable)",
            "CABLE-D Input (VB-Audio Virtual Cable)",
        ] {
            let family = detect_render(name).unwrap_or_else(|| panic!("{name} unmatched"));
            assert_eq!(family.id, "vb-cable-16ch", "{name}");
            assert!(!family.redistributable, "{name} must never be offered for install");
        }
    }

    #[test]
    fn recognises_blackhole_and_distinguishes_variants() {
        assert_eq!(detect_render("BlackHole 2ch").unwrap().id, "blackhole-2ch");
        assert_eq!(detect_render("BlackHole 16ch").unwrap().id, "blackhole-16ch");
    }

    #[test]
    fn recognises_voicemeeter_render_buses() {
        assert_eq!(detect_render("Voicemeeter Input").unwrap().id, "voicemeeter");
        assert_eq!(detect_render("Voicemeeter Aux Input").unwrap().id, "voicemeeter");
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(
            detect_render("cable input (vb-audio virtual cable)").unwrap().id,
            "vb-cable"
        );
        assert_eq!(detect_render("BLACKHOLE 2CH").unwrap().id, "blackhole-2ch");
    }

    #[test]
    fn real_hardware_is_not_detected_as_a_cable() {
        assert!(detect_render("Speakers/Headphones (Realtek(R) Audio)").is_none());
        assert!(detect_render("HP LE2202x (Intel(R) Display Audio)").is_none());
    }

    #[test]
    fn real_hardware_names_do_not_look_virtual() {
        for name in [
            "Speakers/Headphones (Realtek(R) Audio)",
            "Microphone (Realtek(R) Audio)",
            "Stereo Mix (Realtek(R) Audio)",
            "Microphone (WO Mic Device)",
            "Microphone Array (USB Audio)",
        ] {
            assert!(!looks_virtual(name), "{name} should not look virtual");
        }
    }

    #[test]
    fn unknown_cables_still_look_virtual() {
        // We cannot name it, but treating it as speakers would be worse.
        assert!(detect_render("SomeBrand Virtual Cable 4").is_none());
        assert!(looks_virtual("SomeBrand Virtual Cable 4"));
        assert!(looks_virtual("Soundflower (2ch)"));
    }

    #[test]
    fn only_vb_cable_is_advertised_as_bundlable() {
        let bundlable_ids: Vec<&str> = bundlable().map(|f| f.id).collect();
        assert_eq!(
            bundlable_ids,
            vec!["vb-cable"],
            "we must never offer to auto-install something we may not ship"
        );
    }

    #[test]
    fn every_family_has_a_usable_capture_pattern() {
        for family in FAMILIES {
            assert!(!family.capture_pattern.is_empty(), "{} has no capture pattern", family.id);
            assert!(!family.render_patterns.is_empty(), "{} has no render patterns", family.id);
            assert!(!family.notes.is_empty(), "{} has no note for the UI", family.id);
        }
    }

    /// A [`DeviceInfo`] shaped exactly the way `lib::describe` builds one, so
    /// these fixtures cannot drift away from a real enumeration.
    fn device(name: &str, kind: DeviceKind) -> DeviceInfo {
        let family = detect_render(name);
        DeviceInfo {
            is_virtual: family.is_some() || looks_virtual(name),
            family: family.map(|f| f.id),
            family_label: family.map(|f| f.label),
            name: name.to_string(),
            kind,
            is_default: false,
        }
    }

    fn render(name: &str) -> DeviceInfo {
        device(name, DeviceKind::Render)
    }

    fn capture(name: &str) -> DeviceInfo {
        device(name, DeviceKind::Capture)
    }

    // ---- The licensing rules, as data ----

    #[test]
    fn only_vb_cable_has_an_install_recipe() {
        let ids: Vec<&str> = RECIPES.iter().map(|r| r.family_id).collect();
        assert_eq!(
            ids,
            vec!["vb-cable"],
            "a second recipe here means offering to fetch a cable we may not ship"
        );
    }

    #[test]
    fn the_recipe_table_and_the_licence_flags_agree_in_both_directions() {
        // Two independent tables, so each one checks the other. The mistake this
        // catches is a plausible-looking recipe added for A+B or BlackHole by
        // someone who read the mechanism and not the licence.
        for recipe in RECIPES {
            let family = family_by_id(recipe.family_id)
                .unwrap_or_else(|| panic!("recipe {} has no family", recipe.family_id));
            assert!(
                family.redistributable,
                "{} has a download recipe but is not licensed to be shipped",
                family.id
            );
        }
        for family in FAMILIES.iter().filter(|f| f.redistributable) {
            assert!(
                install_recipe(family.id).is_some(),
                "{} is licensed to be shipped but has no recipe",
                family.id
            );
        }
    }

    #[test]
    fn only_vb_cable_is_ever_offered_for_auto_install() {
        // The headline licensing rule, asserted across every family rather than
        // restated by hand: Voicemeeter (Potato "cannot be distributed or
        // bundled"), VB-CABLE A+B / C+D (same wording), Virtual Audio Cable (paid,
        // per machine) and BlackHole (GPL-3.0) must all come back as the user's
        // own job.
        let offered: Vec<&str> = FAMILIES
            .iter()
            .filter(|family| install_plan_for(family).we_install())
            .map(|family| family.id)
            .collect();
        assert_eq!(offered, vec!["vb-cable"]);
    }

    #[test]
    fn a_cable_we_may_not_ship_is_still_usable_once_the_user_installs_it() {
        // The other half of the rule: "we may not install it" must never be
        // implemented as "we refuse to use it". Somebody with A+B, Voicemeeter,
        // VAC or BlackHole already has working audio and must be left alone.
        for name in [
            "CABLE In 16ch (VB-Audio Virtual Cable)",
            "Voicemeeter Input",
            "Line 1 (Virtual Audio Cable)",
            "BlackHole 2ch",
        ] {
            let state = cable_state_from(&[render(name)]);
            assert!(state.is_usable(), "{name} must be usable");
            assert_eq!(
                state.may_redistribute(),
                Some(false),
                "{name} is not ours to ship"
            );
            assert_eq!(
                state.install_plan(),
                InstallPlan::NothingToDo,
                "{name} works, so there is nothing to install"
            );
            let family = state.family().expect("curated");
            assert!(
                !install_plan_for(family).we_install(),
                "{name} must never be offered for auto-install"
            );
        }
    }

    #[test]
    fn the_consent_note_keeps_the_donationware_model_applicable() {
        // VB-Audio's licence page makes this wording the condition on which
        // distribution is allowed at all: the end user must be able to see that it
        // is VB-Audio, and to donate. Dropping a phrase here would make the
        // install unlicensed, and no compiler would ever tell us.
        let recipe = install_recipe("vb-cable").expect("VB-CABLE must have a recipe");
        let note = recipe.consent_note.to_ascii_lowercase();
        assert!(
            note.contains("vb-cable.com"),
            "the origin of VB-CABLE must be named"
        );
        assert!(
            note.contains("donationware") || note.contains("donate"),
            "the consent text must say it is donationware"
        );
        assert!(note.contains("vb-audio"), "the author must be identified");
        assert!(
            !recipe.can_embed,
            "the pack's own readme forbids integrating it into another installer \
             without the Author's agreement"
        );
    }

    #[test]
    fn every_recipe_downloads_over_https_from_the_vendor_host() {
        for recipe in RECIPES {
            assert!(
                recipe.url.starts_with("https://download.vb-audio.com/"),
                "{} must come from the vendor's own host, not a mirror",
                recipe.url
            );
            assert!(recipe.url.ends_with(".zip"), "{}", recipe.url);
            assert!(
                !recipe.setup_exe.contains(['/', '\\']),
                "{} must be a bare file name inside the pack",
                recipe.setup_exe
            );
        }
    }

    // ---- The state machine ----

    #[test]
    fn no_devices_at_all_is_missing() {
        assert_eq!(cable_state_from(&[]), CableState::Missing);
    }

    #[test]
    fn a_machine_with_only_real_hardware_is_missing() {
        let devices = [
            render("Speakers/Headphones (Realtek(R) Audio)"),
            render("HP LE2202x (Intel(R) Display Audio)"),
            capture("Microphone (Realtek(R) Audio)"),
        ];
        assert_eq!(cable_state_from(&devices), CableState::Missing);
        assert!(!cable_state_from(&devices).is_usable());
    }

    #[test]
    fn a_capture_endpoint_alone_never_counts_as_a_cable() {
        // The bug this prevents: `CABLE Output` looks exactly like a virtual
        // cable, and if it counted as one we would tell a user with only half the
        // driver installed that everything was ready, then have nowhere to play.
        let devices = [capture("CABLE Output (VB-Audio Virtual Cable)")];
        assert_eq!(cable_state_from(&devices), CableState::Missing);
    }

    #[test]
    fn a_recognised_render_endpoint_is_ready() {
        let state = cable_state_from(&[
            render("Speakers (Realtek(R) Audio)"),
            render("CABLE Input (VB-Audio Virtual Cable)"),
        ]);
        assert!(state.is_usable());
        assert_eq!(state.family().unwrap().id, "vb-cable");
        assert_eq!(
            state.endpoint(),
            Some("CABLE Input (VB-Audio Virtual Cable)")
        );
        assert_eq!(state.may_redistribute(), Some(true));
        assert!(!state.install_plan().we_install());
    }

    #[test]
    fn an_unrecognised_virtual_render_endpoint_is_usable_but_uncurated() {
        let state = cable_state_from(&[render("SomeBrand Virtual Cable 4")]);
        assert!(state.is_usable(), "we cannot name it, but it is not nothing");
        assert_eq!(state.family(), None);
        assert_eq!(state.may_redistribute(), None, "do not guess a licence");
        assert_eq!(state.endpoint(), Some("SomeBrand Virtual Cable 4"));
        assert_eq!(
            state.install_plan(),
            InstallPlan::NothingToDo,
            "audio already flows, so nothing should be installed over it"
        );
    }

    #[test]
    fn a_recognised_cable_wins_over_an_uncurated_one_whichever_comes_first() {
        // Enumeration order is whatever Windows hands back, so the verdict must
        // not depend on it. A known product is more useful than an unknown one:
        // it is the only case where we can name the capture side.
        let uncurated = render("SomeBrand Virtual Cable 4");
        let known = render("CABLE Input (VB-Audio Virtual Cable)");
        let forward = cable_state_from(&[uncurated.clone(), known.clone()]);
        let backward = cable_state_from(&[known, uncurated]);
        assert_eq!(forward, backward);
        assert_eq!(forward.family().unwrap().id, "vb-cable");
    }

    #[test]
    fn the_family_we_report_is_the_one_the_device_list_already_carries() {
        // `lib::describe` fills in `DeviceInfo::family` from the same predicate,
        // so a disagreement would mean probe's table and its verdict name two
        // different products on one screen.
        let cable = render("CABLE In 16ch (VB-Audio Virtual Cable)");
        assert_eq!(cable.family, Some("vb-cable-16ch"));
        let state = cable_state_from(&[cable]);
        assert_eq!(state.family().unwrap().id, "vb-cable-16ch");
    }

    #[test]
    fn the_missing_state_offers_the_one_cable_we_may_install() {
        let state = CableState::Missing;
        let plan = state.install_plan();
        assert!(plan.we_install());
        let family = plan.family().expect("the plan names a family");
        assert_eq!(family.id, "vb-cable");
        assert_eq!(
            install_recipe(family.id).map(|r| r.setup_exe),
            Some("VBCABLE_Setup_x64.exe"),
            "the plan must carry the recipe the CLI will actually run"
        );
    }

    // ---- What the user is told ----

    #[test]
    fn the_missing_state_tells_the_user_exactly_what_to_run() {
        let advice = CableState::Missing.next_steps().join("\n");
        assert!(
            advice.contains("talkee cable install --yes"),
            "the fix must be a command they can paste"
        );
        assert!(
            advice.contains("talkee cable install --dry-run"),
            "and the safe preview of it"
        );
        assert!(
            advice.contains("download.vb-audio.com"),
            "and where the download comes from, before they run it"
        );
        assert!(
            !advice.contains("talkee host --device"),
            "there is no endpoint to name on this machine"
        );
    }

    #[test]
    fn the_missing_state_offers_every_alternative_we_may_not_fetch() {
        // A user who already owns VAC, or who runs Voicemeeter, must be able to
        // see that Talkee will work with it — otherwise the only cable they hear
        // about is the one we happen to be allowed to install.
        let advice = CableState::Missing.next_steps().join("\n");
        for family in FAMILIES.iter().filter(|f| !f.redistributable) {
            assert!(
                advice.contains(family.label),
                "{} is missing from the list of cables that work",
                family.id
            );
        }
    }

    #[test]
    fn a_ready_state_names_the_capture_endpoint_and_offers_no_install() {
        let state = cable_state_from(&[render("CABLE Input (VB-Audio Virtual Cable)")]);
        let advice = state.next_steps().join("\n");
        assert!(advice.contains("CABLE Output"), "{advice}");
        assert!(
            !advice.contains("talkee cable install"),
            "a working machine must never be told to install anything"
        );
    }

    #[test]
    fn a_ready_state_says_so_when_the_user_had_to_install_it_themselves() {
        let state = cable_state_from(&[render("Line 1 (Virtual Audio Cable)")]);
        let advice = state.next_steps().join("\n");
        assert!(
            advice.contains("yourself"),
            "the user should not be left wondering why Talkee did not do it"
        );
        assert!(!advice.contains("talkee cable install"));

        // The one case where we did the work, so there is nothing to explain.
        let ours = cable_state_from(&[render("CABLE Input (VB-Audio Virtual Cable)")]);
        assert!(!ours.next_steps().join("\n").contains("yourself"));
    }

    #[test]
    fn an_uncurated_state_tells_the_user_to_name_the_endpoint() {
        let state = cable_state_from(&[render("SomeBrand Virtual Cable 4")]);
        let advice = state.next_steps().join("\n");
        assert!(advice.contains("--device"), "{advice}");
        assert!(advice.contains("SomeBrand Virtual Cable 4"), "{advice}");
    }

    #[test]
    fn the_summary_line_says_which_product_or_that_there_is_none() {
        let ready = cable_state_from(&[render("CABLE Input (VB-Audio Virtual Cable)")]);
        assert!(ready.summary().contains("VB-CABLE"), "{}", ready.summary());
        assert!(ready.summary().contains("CABLE Input"), "{}", ready.summary());

        let uncurated = cable_state_from(&[render("SomeBrand Virtual Cable 4")]);
        assert!(uncurated.summary().contains("unrecognised"));
        assert!(uncurated.summary().contains("SomeBrand Virtual Cable 4"));

        assert_eq!(
            CableState::Missing.summary(),
            "no virtual audio cable is installed"
        );
    }
}

