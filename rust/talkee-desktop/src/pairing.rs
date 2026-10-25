//! The pairing code, as a picture.
//!
//! One job: turn the `talkee://pair?...` link into an SVG QR code for a screen. The page that shows
//! it lives in `wizard`.
//!
//! # Why the encoding is the same crate the CLI uses
//!
//! `talkee-cli` draws the same link in a terminal, using the same `qrcode` crate, and its test
//! *decodes* what it draws with a real decoder — so the version, the error correction and the quiet
//! zone are checked rather than assumed. Rendering it here through the same crate means that
//! verification covers this path too.
//!
//! # Why this is SVG and not a bitmap
//!
//! A QR code is a grid of squares, and a bitmap of squares goes soft at some zoom level. The browser
//! scales an SVG to whatever size the user's screen and eyes want, which matters more than anything
//! else about it: the failure mode here is a code that does not scan.

use anyhow::{Context, Result};
use qrcode::render::svg;
use qrcode::QrCode;

/// The pairing link as an SVG QR code, sized for a screen.
///
/// Dark on light, deliberately. Scanners expect dark modules on a light field and the surrounding
/// page is dark — which is why the caller has to put this on a white card rather than straight onto
/// the background. The quiet zone comes from the renderer, and it is what lets a camera find the
/// code's edges at all.
pub fn qr_svg(link: &str) -> Result<String> {
    let code = QrCode::new(link.as_bytes())
        .with_context(|| format!("encoding a {}-byte pairing link as a QR code", link.len()))?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(420, 420)
        .dark_color(svg::Color("#0B0D10"))
        .light_color(svg::Color("#FFFFFF"))
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link of the length the app actually builds, 132 characters.
    const LINK: &str = "talkee://pair?v=1&h=192.168.0.24&p=47800&fpid=219e1e27\
&name=Talkee%20on%20DESKTOP-EXAMPLE&k=QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVowMTIzNDU";

    #[test]
    fn a_real_link_renders_as_a_square_code() {
        let svg = qr_svg(LINK).expect("the real link must render");
        assert!(svg.contains("<svg"), "not an SVG: {svg}");
        // The renderer prepends an XML prologue, so this is a `contains` rather than a `starts_with`,
        // and it sizes to the module grid rather than to the minimum asked for — 456 for this link.
        assert!(svg.contains("viewBox="), "no viewBox, so it will not scale");
        let width = dimension(&svg, "width");
        let height = dimension(&svg, "height");
        assert_eq!(width, height, "the code must be square");
        assert!(
            width >= 420,
            "too small to scan at arm's length: {width}px"
        );
        // Dark on light, on purpose: a scanner looks for dark modules on a light field.
        assert!(svg.contains("fill=\"#0B0D10\""), "the modules are not dark");
        assert!(svg.contains("fill=\"#FFFFFF\""), "the field is not light");
    }

    /// Reads an SVG attribute value, e.g. `width="456"`.
    fn dimension(svg: &str, name: &str) -> u32 {
        let needle = format!("{name}=\"");
        let start = svg.find(&needle).expect("the attribute is present") + needle.len();
        svg[start..]
            .split('"')
            .next()
            .expect("a value")
            .parse()
            .expect("a number")
    }

    #[test]
    fn the_drawing_is_a_grid_rather_than_a_picture_of_one() {
        // The point of SVG here. A rasterised QR goes soft when it is scaled, and a soft code is one a
        // phone declines to read — which presents to a user as "the scanner does not work".
        let svg = qr_svg(LINK).unwrap();
        assert!(
            svg.contains("<path") || svg.contains("<rect"),
            "the modules should be vector shapes: {svg}"
        );
        assert!(
            !svg.contains("<image"),
            "an embedded bitmap would defeat the point"
        );
    }

    #[test]
    fn a_link_too_long_to_encode_is_an_error_rather_than_a_broken_code() {
        // Not hypothetical: a QR tops out around 2 953 bytes and the link grows with the host name. An
        // error here becomes a sentence on the page; a broken code becomes a user who concludes the
        // scanner is broken.
        let huge = "talkee://pair?".to_string() + &"k=".repeat(4000);
        assert!(qr_svg(&huge).is_err());
    }
}