//! Drawing the pairing link as a QR code, for a terminal.
//!
//! The link is 132 characters and it *is* the key, so it gets scanned rather than typed. This is
//! the cheapest surface that works: the same terminal `talkee host` already prints to.
//!
//! # Why this is not in `talkee-crypto`
//!
//! That crate is the security boundary and says so in its own manifest — no `qrcode`, no image
//! decoding, nothing between the wire and the key material that does not have to be there. It
//! hands out the exact string to render and this crate draws it. When the tray app shows a QR it
//! will want its own renderer onto a bitmap; this one belongs to the console.
//!
//! # Why the test decodes it
//!
//! A QR that does not scan is a pairing that fails silently, and it fails in the worst way: the
//! user believes they scanned it and the microphone stays quiet. So the test renders the real
//! link, hands it to a decoder, and asserts it comes back byte for byte. The encoder is not taken
//! on trust, and neither is the quiet zone.

use anyhow::{Context, Result};
use qrcode::QrCode;

/// Modules of light border around the code.
///
/// The specification asks for four, and scanners genuinely use them to find the code's edges. A
/// code drawn without a quiet zone looks fine and does not scan, which is exactly the kind of bug
/// that survives until somebody tries it on a phone.
const QUIET_ZONE: usize = 4;

/// The pairing link as a square grid of modules, `true` being a dark module.
///
/// Includes the quiet zone, so this is what should be drawn, not a cropped version of it.
pub fn qr_matrix(link: &str) -> Result<Vec<Vec<bool>>> {
    let code = QrCode::new(link.as_bytes())
        .with_context(|| format!("encoding a {}-byte pairing link as a QR code", link.len()))?;
    let width = code.width();
    let colors = code.to_colors();
    let size = width + QUIET_ZONE * 2;

    let mut grid = vec![vec![false; size]; size];
    for y in 0..width {
        for x in 0..width {
            if colors[y * width + x] == qrcode::Color::Dark {
                grid[y + QUIET_ZONE][x + QUIET_ZONE] = true;
            }
        }
    }
    Ok(grid)
}

/// The same grid drawn with half-height blocks, two module rows per line of text.
///
/// Dark modules are blocks and light modules are spaces, so on a light-on-dark terminal the code
/// comes out inverted. Most scanners cope with that; the reader that does not can use the link
/// printed alongside, which is why it is printed alongside.
pub fn qr_text(link: &str) -> Result<String> {
    let grid = qr_matrix(link)?;
    let mut out = String::with_capacity(grid.len() * (grid.len() + 1));
    for pair in grid.chunks(2) {
        for x in 0..pair[0].len() {
            let top = pair[0][x];
            let bottom = pair.get(1).map(|row| row[x]).unwrap_or(false);
            out.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link of the shape `talkee host` actually prints.
    const LINK: &str = "talkee://pair?v=1&h=192.168.0.24&p=47800&fpid=219e1e27\
&name=Talkee%20on%20DESKTOP-EXAMPLE&k=QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU";

    /// Rasterises the grid the way a screen would, so a decoder sees an image and not a matrix.
    fn rasterise(grid: &[Vec<bool>], scale: usize) -> Vec<Vec<u8>> {
        let size = grid.len() * scale;
        (0..size)
            .map(|y| {
                (0..size)
                    .map(|x| if grid[y / scale][x / scale] { 0 } else { 255 })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_code_we_print_decodes_back_to_the_link() {
        // The whole point. If this fails, the user scans something that is not a pairing link,
        // sees no error, and the microphone stays silent — a failure that looks like nothing
        // happening, which is the hardest kind to report or diagnose.
        let grid = qr_matrix(LINK).expect("the real link must encode");
        let image = rasterise(&grid, 4);
        let pixels = image.len();

        let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(pixels, pixels, |x, y| {
            image[y][x]
        });
        let grids = prepared.detect_grids();
        assert_eq!(grids.len(), 1, "a decoder must find exactly one code");
        let (_meta, content) = grids[0].decode().expect("the code must decode");
        assert_eq!(content, LINK, "the code does not carry the link we printed");
    }

    #[test]
    fn there_is_a_quiet_zone_on_every_side() {
        // The ink is the payload; the border is what lets a scanner find it. A code with no quiet
        // zone is the classic "works on my screen, not on your phone" bug.
        let grid = qr_matrix(LINK).expect("encodes");
        let size = grid.len();
        assert!(size > QUIET_ZONE * 2);
        for i in 0..size {
            assert!(!grid[0][i], "the top border is not clear");
            assert!(!grid[size - 1][i], "the bottom border is not clear");
            assert!(!grid[i][0], "the left border is not clear");
            assert!(!grid[i][size - 1], "the right border is not clear");
        }
    }

    #[test]
    fn the_terminal_drawing_is_square_on_screen() {
        // Two module rows per line keeps the code square: 49 modules across is 49 columns and 25
        // lines. Getting this wrong stretches the code into something no phone will read.
        let text = qr_text(LINK).expect("encodes");
        let lines: Vec<&str> = text.lines().collect();
        let modules = qr_matrix(LINK).expect("encodes").len();
        assert_eq!(lines.len(), modules.div_ceil(2), "two module rows per line");
        for line in &lines {
            assert_eq!(
                line.chars().count(),
                modules,
                "every line must be as wide as the code is tall"
            );
        }
    }

    #[test]
    fn a_link_too_long_to_encode_is_an_error_rather_than_a_broken_code() {
        // Not hypothetical: a QR tops out around 2 953 bytes and the link grows with the host
        // name. Better a line in the log than a code that draws but cannot be read.
        let huge = "talkee://pair?".to_string() + &"k=".repeat(4000);
        assert!(qr_matrix(&huge).is_err());
    }
}
