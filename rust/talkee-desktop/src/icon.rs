//! The tray icon, drawn from the current state.
//!
//! # The icon is the level meter
//!
//! There is no window in this app, so the icon has to carry both facts the user
//! needs: which of the four states is current, and how loud the audio is on each side
//! of the pipeline. That is why it is drawn rather than loaded from a file — an icon
//! that moves with the audio cannot be a static asset — and drawing it here makes the
//! whole thing a pure function that can be asserted on with no display attached.
//!
//! Two bars, input above output, coloured by state. Showing both is the point: audio
//! arriving and audio leaving fail independently, and a single bar cannot say which
//! one stopped.

use crate::state::{AppState, Reading};

/// Edge length of the drawn icon, in pixels.
///
/// 32 is the size Windows asks a tray icon for at 100% scaling. The shell rescales it
/// for the notification area, so one size is enough.
pub const ICON_SIZE: u32 = 32;

/// Cells in a bar, mirrored from `state` so the maths below reads as pixels.
const CELLS: usize = crate::state::METER_CELLS;

/// Horizontal extent of the bars.
const BAR_LEFT: u32 = 5;
const BAR_WIDTH: u32 = 22;

/// Vertical extent of the two bars. Input on top, output below: the top bar is what
/// the phone is sending, the bottom one is what the cable is being fed.
const IN_TOP: u32 = 9;
const IN_BOTTOM: u32 = 13;
const OUT_TOP: u32 = 18;
const OUT_BOTTOM: u32 = 22;

/// A lit cell. Near-white, so it reads against both a light and a dark taskbar.
const FILL: [u8; 4] = [240, 240, 242, 255];
/// The unfilled part of a bar.
const TRACK: [u8; 4] = [38, 38, 42, 255];
/// A bar whose value cannot be measured. Deliberately neither `FILL` nor `TRACK`, so
/// "unknown" can never be misread as "empty" — which is the failure this whole app
/// exists to remove.
const HATCH: [u8; 4] = [140, 140, 150, 255];
/// Outline colour, so the icon has a defined edge on any taskbar.
const EDGE: [u8; 4] = [16, 16, 18, 255];

/// A raw RGBA bitmap, ready for the tray.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba {
    /// `width * height * 4` bytes, row-major from the top.
    pub pixels: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Rgba {
    /// The pixel at `(x, y)`, or fully transparent if out of bounds.
    ///
    /// Only the tests read single pixels: the tray hands the whole buffer to the shell.
    #[cfg(test)]
    pub fn at(&self, x: u32, y: u32) -> [u8; 4] {
        if x >= self.width || y >= self.height {
            return [0, 0, 0, 0];
        }
        let start = ((y * self.width + x) * 4) as usize;
        let mut out = [0u8; 4];
        out.copy_from_slice(&self.pixels[start..start + 4]);
        out
    }
}

/// The background colour for a state.
///
/// Red for `Silent` on purpose: that is the one state where the user is being
/// misunderstood and does not know it.
fn state_colour(state: AppState) -> [u8; 4] {
    match state {
        AppState::NoCable => [122, 58, 58, 255],
        AppState::Searching => [150, 110, 30, 255],
        AppState::Live => [40, 150, 70, 255],
        AppState::Silent => [196, 40, 40, 255],
    }
}

/// Draws the icon for a reading.
pub fn draw(reading: &Reading) -> Rgba {
    let mut pixels = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];

    // A rounded square rather than a circle: two straight bars need straight edges, or
    // they have to be inset so far that the meter is unreadable at 16 px.
    let background = state_colour(reading.state);
    for y in 1..ICON_SIZE - 1 {
        for x in 1..ICON_SIZE - 1 {
            if !cut_corner(x, y) {
                put(&mut pixels, x, y, background);
            }
        }
    }
    outline(&mut pixels);

    fill_bar(
        &mut pixels,
        IN_TOP,
        IN_BOTTOM,
        crate::state::level_cells(reading.level_in) as f32,
    );
    match reading.out.cells() {
        Some(cells) => fill_bar(&mut pixels, OUT_TOP, OUT_BOTTOM, cells as f32),
        None => hatch_bar(&mut pixels, OUT_TOP, OUT_BOTTOM),
    }
    Rgba {
        pixels,
        width: ICON_SIZE,
        height: ICON_SIZE,
    }
}

/// The same icon, desaturated.
///
/// Used for one situation only: the user pressed Stop, so there is no session and
/// therefore no state of the receiver to report. Grey says "nothing is being listened
/// for" without claiming one of the four states, which would be a statement about a
/// receiver that is not running.
pub fn draw_stopped(reading: &Reading) -> Rgba {
    let mut icon = draw(reading);
    for pixel in icon.pixels.chunks_exact_mut(4) {
        // Rec. 601 luma: enough to make "not running" obvious at a glance without a
        // second colour scheme to keep in sync.
        let luma = (0.299 * pixel[0] as f32 + 0.587 * pixel[1] as f32 + 0.114 * pixel[2] as f32)
            .round() as u8;

        pixel[0] = luma;
        pixel[1] = luma;
        pixel[2] = luma;
    }
    icon
}

/// True for the four corner pixels that are cut away to round the square.
fn cut_corner(x: u32, y: u32) -> bool {
    let left = x == 1;
    let right = x == ICON_SIZE - 2;
    let top = y == 1;
    let bottom = y == ICON_SIZE - 2;
    (left && top) || (left && bottom) || (right && top) || (right && bottom)
}

/// Draws the one-pixel dark border inside the rounded square.
fn outline(pixels: &mut [u8]) {
    for y in 1..ICON_SIZE - 1 {
        for x in 1..ICON_SIZE - 1 {
            let on_edge = x == 1 || x == ICON_SIZE - 2 || y == 1 || y == ICON_SIZE - 2;
            if on_edge && !cut_corner(x, y) {
                put(pixels, x, y, EDGE);
            }
        }
    }
}

/// Draws one bar: a track with `cells` of it lit.
///
/// The lit width rounds up, so a single lit cell is at least one pixel wide and a
/// whisper the meter has decided to show cannot round away to nothing.
fn fill_bar(pixels: &mut [u8], top: u32, bottom: u32, cells: f32) {
    for y in top..=bottom {
        for x in BAR_LEFT..BAR_LEFT + BAR_WIDTH {
            put(pixels, x, y, TRACK);
        }
    }
    if cells <= 0.0 {
        return;
    }
    let lit = ((cells / CELLS as f32) * BAR_WIDTH as f32).ceil() as u32;
    let lit = lit.clamp(1, BAR_WIDTH);
    for y in top..=bottom {
        for x in BAR_LEFT..BAR_LEFT + lit {
            put(pixels, x, y, FILL);
        }
    }
}

/// Draws a bar whose value cannot be measured, as a hatch.
fn hatch_bar(pixels: &mut [u8], top: u32, bottom: u32) {
    for y in top..=bottom {
        for x in BAR_LEFT..BAR_LEFT + BAR_WIDTH {
            let colour = if (x + y) % 3 == 0 { HATCH } else { TRACK };
            put(pixels, x, y, colour);
        }
    }
}

/// Writes one opaque pixel, ignoring anything out of bounds.
fn put(pixels: &mut [u8], x: u32, y: u32, colour: [u8; 4]) {
    if x >= ICON_SIZE || y >= ICON_SIZE {
        return;
    }
    let start = ((y * ICON_SIZE + x) * 4) as usize;
    pixels[start..start + 4].copy_from_slice(&colour);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Observation, OutMeter};
    use std::time::Duration;

    /// An observation that really does produce `state`.
    fn observation(state: AppState, level_in: f32) -> Observation {
        let (cable_present, datagrams, since_last_datagram) = match state {
            AppState::NoCable => (false, 0, Duration::ZERO),
            AppState::Searching => (true, 0, Duration::ZERO),
            AppState::Live => (true, 100, Duration::ZERO),
            AppState::Silent => (true, 100, Duration::from_secs(2)),
        };
        Observation {
            cable_present,
            datagrams,
            since_last_datagram,
            quiet_for: Duration::ZERO,
            level_in,
            level_out: 0.0,
            refused: 0,
        }
    }

    fn reading(state: AppState, level_in: f32, out: OutMeter) -> Reading {
        let reading = Reading::sample(&observation(state, level_in), out);
        assert_eq!(reading.state, state, "fixture produced the wrong state");
        reading
    }

    /// Pixels of one colour inside one bar.
    fn band_pixels(icon: &Rgba, top: u32, bottom: u32, colour: [u8; 4]) -> usize {
        let mut count = 0;
        for y in top..=bottom {
            for x in BAR_LEFT..BAR_LEFT + BAR_WIDTH {
                if icon.at(x, y) == colour {
                    count += 1;
                }
            }
        }
        count
    }

    fn band_area() -> usize {
        BAR_WIDTH as usize * (IN_BOTTOM - IN_TOP + 1) as usize
    }

    #[test]
    fn every_icon_is_a_complete_bitmap_with_no_partial_alpha() {
        for state in AppState::ALL {
            let icon = draw(&reading(state, 0.2, OutMeter::Measured(0.2)));
            assert_eq!(icon.pixels.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
            assert_eq!(icon.width, ICON_SIZE);
            assert_eq!(icon.height, ICON_SIZE);
            // The corners are cut and the ring is inset, so transparency is deliberate.
            // What must never happen is *partial* alpha: blending against an unknown
            // taskbar theme produces mud, and nothing tells us what we are drawn on.
            let mut opaque = 0;
            for (index, chunk) in icon.pixels.chunks_exact(4).enumerate() {
                assert!(
                    chunk[3] == 255 || chunk[3] == 0,
                    "pixel {index} has partial alpha {}",
                    chunk[3]
                );
                if chunk[3] == 255 {
                    opaque += 1;
                }
            }
            let total = (ICON_SIZE * ICON_SIZE) as usize;
            assert!(
                opaque > total / 2,
                "{state:?} painted only {opaque} of {total} pixels"
            );
        }
    }

    #[test]
    fn silence_lights_nothing_and_loudness_lights_more() {
        let quiet = draw(&reading(AppState::Live, 0.0, OutMeter::Measured(0.0)));
        assert_eq!(
            band_pixels(&quiet, IN_TOP, IN_BOTTOM, FILL),
            0,
            "a silent input must light no cell"
        );

        let mut previous = 0;
        for level in [0.001, 0.01, 0.05, 0.2, 1.0] {
            let icon = draw(&reading(AppState::Live, level, OutMeter::Measured(level)));
            let lit = band_pixels(&icon, IN_TOP, IN_BOTTOM, FILL);
            assert!(lit > previous, "level {level} did not grow the bar");
            previous = lit;
        }
        assert_eq!(
            previous,
            band_area(),
            "full scale must fill the whole bar"
        );
    }

    #[test]
    fn the_two_bars_are_drawn_from_the_two_different_levels() {
        // The point of showing both: a loud input with a dead output has to look
        // different from a quiet input with a working one.
        let loud_in_dead_out = draw(&reading(AppState::Live, 0.3, OutMeter::Measured(0.0)));
        assert!(band_pixels(&loud_in_dead_out, IN_TOP, IN_BOTTOM, FILL) > 0);
        assert_eq!(band_pixels(&loud_in_dead_out, OUT_TOP, OUT_BOTTOM, FILL), 0);

        let quiet_in_working_out = draw(&reading(AppState::Live, 0.0, OutMeter::Measured(0.3)));
        assert_eq!(band_pixels(&quiet_in_working_out, IN_TOP, IN_BOTTOM, FILL), 0);
        assert!(band_pixels(&quiet_in_working_out, OUT_TOP, OUT_BOTTOM, FILL) > 0);
    }

    #[test]
    fn an_unmeasured_output_is_hatched_rather_than_empty() {
        let icon = draw(&reading(AppState::Live, 0.3, OutMeter::Unpublished));
        assert_eq!(
            band_pixels(&icon, OUT_TOP, OUT_BOTTOM, FILL),
            0,
            "an unmeasured bar must not look lit"
        );
        assert!(
            band_pixels(&icon, OUT_TOP, OUT_BOTTOM, HATCH) > 0,
            "an unmeasured bar must not look empty either"
        );
    }

    #[test]
    fn the_four_states_are_four_different_icons() {
        let icons: Vec<Vec<u8>> = AppState::ALL
            .iter()
            .map(|state| draw(&reading(*state, 0.2, OutMeter::Measured(0.2))).pixels)
            .collect();
        for (index, icon) in icons.iter().enumerate() {
            for other in icons.iter().skip(index + 1) {
                assert_ne!(icon, other, "two states draw identically");
            }
        }
    }

    #[test]
    fn a_stopped_app_looks_stopped() {
        let running = draw(&reading(AppState::Searching, 0.0, OutMeter::Waiting));
        let stopped = draw_stopped(&reading(AppState::Searching, 0.0, OutMeter::Waiting));
        assert_ne!(running.pixels, stopped.pixels, "stopped must be visible");
        for pixel in stopped.pixels.chunks_exact(4) {
            assert_eq!(pixel[0], pixel[1]);
            assert_eq!(pixel[1], pixel[2]);
        }
    }
}
