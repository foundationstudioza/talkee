//! `talkee probe` — what can we actually do on this machine?
//!
//! This is the first thing to run and the first thing to ask a user for when
//! something does not work. It answers two questions: which endpoints exist, and
//! what the virtual-cable situation is.
//!
//! # Why the verdict looks like `talkee cable status`
//!
//! It is the same code: `cable_state_from` over the list printed above, and
//! `CableState::next_steps` for the advice. Two surfaces disagreeing about whether
//! a machine is ready is worse than either one being terse — and the advice is
//! also where the licensing alternatives live, so it must not exist in two
//! variants that can drift.

use anyhow::Result;
use talkee_vmic::{cable_state_from, list_devices, DeviceInfo};

/// Lists endpoints and prints a verdict plus setup instructions.
pub fn run() -> Result<()> {
    println!("Talkee {} — audio endpoint probe", env!("CARGO_PKG_VERSION"));

    let devices = list_devices()?;
    if devices.is_empty() {
        println!("\nNo audio endpoints found at all, which is unusual.");
        return Ok(());
    }

    println!("\n{} endpoints:\n", devices.len());
    let width = devices.iter().map(|d| d.name.len()).max().unwrap_or(0);
    for device in &devices {
        println!("  {:<4} {}", device.kind.as_str(), describe_line(device, width));
    }

    // Built from the list above rather than by enumerating again, so the verdict
    // cannot disagree with the table the user is looking at.
    let state = cable_state_from(&devices);
    println!("\n{}", state.summary());
    println!();
    for line in state.next_steps() {
        println!("{line}");
    }
    Ok(())
}

/// Formats one endpoint with its tags padded so the names line up.
fn describe_line(device: &DeviceInfo, width: usize) -> String {
    let mut tags = Vec::new();
    if device.is_default {
        tags.push("default".to_string());
    }
    if device.is_virtual {
        tags.push("VIRTUAL".to_string());
    }
    if let Some(label) = device.family_label {
        tags.push(format!("= {label}"));
    }
    format!("{:<width$}  {}", device.name, tags.join(", "), width = width)
}

