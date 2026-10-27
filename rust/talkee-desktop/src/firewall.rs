//! Is Windows Firewall letting Talkee be reached, and how to fix it when it is not.
//!
//! # The problem this solves
//!
//! The first time Talkee listens on the network, Windows asks whether to allow it. If that prompt is
//! dismissed, hidden behind another window or times out, Windows quietly creates a *Block* rule — and a
//! phone then cannot connect, with no message anywhere. From the phone it looks like the PC is simply
//! not there. This reads the rules so the window can say what is wrong, and offers to fix it.
//!
//! # How it looks
//!
//! `netsh advfirewall firewall show rule` can be read without administrator rights. A rule is a block
//! of `Label:  value` lines; the ones that matter are the program it applies to and its action. The
//! labels are in the language of Windows, so this understands English output and reports
//! [`State::Unknown`] for anything it cannot read, which the window treats as "nothing to say".
//!
//! # The fix
//!
//! Changing rules needs administrator rights, and the app does not have them and does not take them
//! silently: [`fix`] asks Windows for an elevated shell, so the user sees the standard permission
//! prompt and decides. It removes every rule for this program (including the blocks) and adds one
//! allow rule for private networks.

use std::path::Path;

/// What the firewall says about this program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// Not checked yet, or the rules could not be read.
    #[default]
    Unknown,
    /// There is no rule for this program yet; Windows will ask the first time it listens.
    NotAsked,
    /// An allow rule covers private networks.
    Allowed,
    /// A block rule covers private networks and nothing allows it.
    Blocked,
}

impl State {
    /// A short word for the status endpoint.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Unknown => "unknown",
            State::NotAsked => "not-asked",
            State::Allowed => "allowed",
            State::Blocked => "blocked",
        }
    }
}

/// Reads `netsh` output and says what it means for `exe`.
///
/// Pure, so it can be tested against text copied from a real machine.
pub fn parse(output: &str, exe: &str) -> State {
    let exe = exe.to_ascii_lowercase();
    let mut saw_any_rule = false;
    let mut allowed = false;
    let mut blocked = false;

    for rule in output.split("\r\n\r\n").flat_map(|chunk| chunk.split("\n\n")) {
        let mut program = None;
        let mut action = None;
        let mut enabled = true;
        let mut profiles = String::new();
        for line in rule.lines() {
            let Some((label, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match label.trim().to_ascii_lowercase().as_str() {
                "rule name" => saw_any_rule = true,
                "program" => program = Some(value.to_ascii_lowercase()),
                "action" => action = Some(value.to_ascii_lowercase()),
                "enabled" => enabled = value.eq_ignore_ascii_case("yes"),
                "profiles" => profiles = value.to_ascii_lowercase(),
                _ => {}
            }
        }
        if program.as_deref() != Some(exe.as_str()) || !enabled {
            continue;
        }
        // The profile that matters is the one a home network is on.
        if !(profiles.contains("private") || profiles.contains("any") || profiles.contains("all")) {
            continue;
        }
        match action.as_deref() {
            Some("allow") => allowed = true,
            Some("block") => blocked = true,
            _ => {}
        }
    }

    if !saw_any_rule {
        // Nothing readable: not English, or the command failed. Say nothing rather than guess.
        return State::Unknown;
    }
    // A block rule beats an allow rule in Windows Firewall, so it decides.
    if blocked {
        State::Blocked
    } else if allowed {
        State::Allowed
    } else {
        State::NotAsked
    }
}

/// Runs `netsh` and reads the answer for this program.
#[cfg(windows)]
pub fn check() -> State {
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // For looking at the other states of the setup guide without damaging a real firewall.
    if let Ok(fake) = std::env::var("TALKEE_FAKE_FIREWALL") {
        return match fake.as_str() {
            "blocked" => State::Blocked,
            "allowed" => State::Allowed,
            "not-asked" => State::NotAsked,
            _ => State::Unknown,
        };
    }
    let Ok(exe) = std::env::current_exe() else {
        return State::Unknown;
    };
    let Ok(output) = Command::new("netsh")
        .args(["advfirewall", "firewall", "show", "rule", "name=all", "dir=in", "verbose"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    else {
        return State::Unknown;
    };
    parse(&String::from_utf8_lossy(&output.stdout), &exe.display().to_string())
}

/// Off Windows there is no firewall to ask.
#[cfg(not(windows))]
pub fn check() -> State {
    State::Unknown
}

/// The PowerShell that repairs the rules for `exe`, as plain text.
///
/// Exposed separately from running it so the exact commands can be read and tested: this is the one
/// place the app changes system settings, and it should be easy to see precisely what it does.
pub fn fix_script(exe: &Path) -> String {
    let exe = exe.display();
    format!(
        "netsh advfirewall firewall delete rule name=all program=\"{exe}\"; \
         netsh advfirewall firewall add rule name=\"Talkee\" dir=in action=allow \
         program=\"{exe}\" profile=private enable=yes"
    )
}

/// Asks Windows for administrator rights and runs the repair. The user sees the standard permission
/// prompt; if they decline, nothing changes.
#[cfg(windows)]
pub fn fix() -> std::io::Result<()> {
    use base64::Engine as _;
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let exe = std::env::current_exe()?;
    // Encoded, so no quoting survives to be got wrong: PowerShell reads UTF-16LE base64.
    let utf16: Vec<u8> = fix_script(&exe)
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let launcher = format!(
        "Start-Process powershell -Verb RunAs -WindowStyle Hidden \
         -ArgumentList '-NoProfile','-EncodedCommand','{encoded}'"
    );
    Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &launcher])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

/// Nothing to repair off Windows.
#[cfg(not(windows))]
pub fn fix() -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Users\someone\AppData\Local\Programs\Talkee\Talkee.exe";

    /// One rule in the form `netsh` prints it.
    fn rule(name: &str, program: &str, profiles: &str, action: &str, enabled: &str) -> String {
        format!(
            "Rule Name:                            {name}\r\n\
             ----------------------------------------------------------------------\r\n\
             Enabled:                              {enabled}\r\n\
             Direction:                            In\r\n\
             Profiles:                             {profiles}\r\n\
             Grouping:                             \r\n\
             LocalIP:                              Any\r\n\
             Protocol:                             TCP\r\n\
             Program:                              {program}\r\n\
             Action:                               {action}\r\n\r\n"
        )
    }

    #[test]
    fn an_allow_rule_for_private_networks_is_allowed() {
        let text = rule("Talkee", EXE, "Private", "Allow", "Yes");
        assert_eq!(parse(&text, EXE), State::Allowed);
    }

    #[test]
    fn a_block_rule_wins_over_an_allow_rule() {
        let text = rule("Talkee", EXE, "Private", "Allow", "Yes")
            + &rule("talkee.exe", EXE, "Private,Public", "Block", "Yes");
        assert_eq!(parse(&text, EXE), State::Blocked);
    }

    #[test]
    fn rules_for_other_programs_are_not_ours() {
        let text = rule("Other", r"C:\other.exe", "Private", "Block", "Yes");
        assert_eq!(parse(&text, EXE), State::NotAsked);
    }

    #[test]
    fn a_disabled_block_rule_does_not_block() {
        let text = rule("talkee.exe", EXE, "Private", "Block", "No");
        assert_eq!(parse(&text, EXE), State::NotAsked);
    }

    #[test]
    fn a_public_only_block_does_not_matter_on_a_home_network() {
        let text = rule("talkee.exe", EXE, "Public", "Block", "Yes");
        assert_eq!(parse(&text, EXE), State::NotAsked);
    }

    #[test]
    fn the_program_path_is_compared_without_regard_to_case() {
        let text = rule("Talkee", &EXE.to_ascii_uppercase(), "Private", "Block", "Yes");
        assert_eq!(parse(&text, EXE), State::Blocked);
    }

    #[test]
    fn output_that_cannot_be_read_says_nothing() {
        assert_eq!(parse("", EXE), State::Unknown);
        assert_eq!(parse("Aucune règle ne correspond aux critères.", EXE), State::Unknown);
    }

    #[test]
    fn the_repair_removes_every_rule_for_the_program_and_adds_one_private_allow() {
        let script = fix_script(Path::new(EXE));
        assert!(script.contains("delete rule name=all program="), "{script}");
        assert!(script.contains("action=allow"), "{script}");
        assert!(script.contains("profile=private"), "{script}");
        assert!(!script.contains("public"), "it must not open public networks: {script}");
        assert_eq!(script.matches(EXE).count(), 2, "both commands name this exact program");
    }
}
