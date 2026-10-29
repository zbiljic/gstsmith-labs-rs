//! Previews: the photo with detected markers and display outlines, editable by
//! hand, and the generated canvas layout.

use std::fmt::Write as _;
use std::io::Cursor;

use anyhow::Result;
use image::DynamicImage;
use wall_geometry::Layout;

use crate::calibration::Calibration;

/// Longest side of the photo embedded in a preview.
const PREVIEW_PIXELS: u32 = 2048;

const EDITOR: &str = include_str!("preview.html");

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let symbol = |index: u32| {
        char::from(
            *ALPHABET
                .get(usize::try_from(index & 63).unwrap_or(0))
                .unwrap_or(&b'A'),
        )
    };
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let [a, b, c] = [0, 1, 2].map(|index| chunk.get(index).copied().map_or(0, u32::from));
        let group = (a << 16) | (b << 8) | c;
        text.push(symbol(group >> 18));
        text.push(symbol(group >> 12));
        text.push(if chunk.len() > 1 {
            symbol(group >> 6)
        } else {
            '='
        });
        text.push(if chunk.len() > 2 { symbol(group) } else { '=' });
    }
    text
}

/// The photo as JPEG, reduced to at most [`PREVIEW_PIXELS`] across.
pub fn photo_jpeg(photo: &DynamicImage) -> Result<Vec<u8>> {
    let preview = if photo.width().max(photo.height()) > PREVIEW_PIXELS {
        photo.thumbnail(PREVIEW_PIXELS, PREVIEW_PIXELS)
    } else {
        photo.clone()
    };
    let mut jpeg = Cursor::new(Vec::new());
    preview
        .to_rgb8()
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)?;
    Ok(jpeg.into_inner())
}

/// A data URI of JPEG bytes.
#[must_use]
pub fn jpeg_data_uri(jpeg: &[u8]) -> String {
    format!("data:image/jpeg;base64,{}", base64(jpeg))
}

/// A JPEG data URI of the photo, reduced to at most [`PREVIEW_PIXELS`] across.
pub fn photo_data_uri(photo: &DynamicImage) -> Result<String> {
    Ok(jpeg_data_uri(&photo_jpeg(photo)?))
}

/// An HTML page drawing `calibration` over the photo. Corners can be dragged,
/// missing displays placed, and the result downloaded as `calibration.json`.
/// Embedded in a frame of a page from the same web server, the page also
/// offers to apply the result, posting a `wallcalibrate-calibration` message
/// with the edited calibration to its parent.
pub fn calibration_page(calibration: &Calibration, photo_uri: Option<&str>) -> Result<String> {
    // Keep the embedded JSON from closing its script element.
    let json = calibration.to_json()?.replace("</", "<\\/");
    Ok(EDITOR
        .replace("{{WIDTH}}", &calibration.photo.width.to_string())
        .replace("{{HEIGHT}}", &calibration.photo.height.to_string())
        .replace("{{PHOTO}}", photo_uri.unwrap_or(""))
        .replace("{{CALIBRATION}}", &json))
}

/// An SVG of the canvas with every display's outline, ID, output size, and top
/// edge, which marks the display's own orientation.
pub fn layout_svg(layout: &Layout) -> Result<String> {
    let (width, height) = (layout.canvas.width, layout.canvas.height);
    let unit = f64::from(width.max(height)) / 400.0;
    let mut svg = String::new();
    writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} {height}" width="{width}" height="{height}" font-family="sans-serif">"#
    )?;
    writeln!(
        svg,
        r##"<rect width="{width}" height="{height}" fill="#1b1b1b" stroke="#777" stroke-width="{unit}"/>"##
    )?;
    for screen in &layout.screens {
        let corners = screen.corners.corners();
        let points: Vec<_> = corners
            .iter()
            .map(|p| format!("{:.2},{:.2}", p.x, p.y))
            .collect();
        let (mut cx, mut cy) = (0.0, 0.0);
        for corner in &corners {
            cx += corner.x / 4.0;
            cy += corner.y / 4.0;
        }
        let top = screen.corners;
        writeln!(
            svg,
            r##"<polygon points="{}" fill="#3c8cff" fill-opacity="0.25" stroke="#3c8cff" stroke-width="{unit}"/>"##,
            points.join(" ")
        )?;
        writeln!(
            svg,
            r##"<line x1="{:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}" stroke="#ffd23c" stroke-width="{}"/>"##,
            top.top_left.x,
            top.top_left.y,
            top.top_right.x,
            top.top_right.y,
            unit * 3.0
        )?;
        writeln!(
            svg,
            r##"<circle cx="{:.2}" cy="{:.2}" r="{}" fill="#ffd23c"/>"##,
            top.top_left.x,
            top.top_left.y,
            unit * 4.0
        )?;
        writeln!(
            svg,
            r##"<text x="{cx:.2}" y="{cy:.2}" font-size="{}" text-anchor="middle" fill="#fff">{}</text>"##,
            unit * 12.0,
            screen.id
        )?;
        writeln!(
            svg,
            r##"<text x="{cx:.2}" y="{:.2}" font-size="{}" text-anchor="middle" fill="#bbb">{}x{}</text>"##,
            cy + unit * 12.0,
            unit * 8.0,
            screen.output.width,
            screen.output.height
        )?;
    }
    svg.push_str("</svg>\n");
    Ok(svg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foobar", "Zm9vYmFy"),
            ("\u{ff}\u{fe}", "w7/Dvg=="),
        ] {
            assert_eq!(base64(input.as_bytes()), expected, "{input:?}");
        }
    }

    #[test]
    fn layouts_draw_every_screen() {
        let layout = Layout::from_json(include_str!(
            "../../../video/wallwarp/examples/three-screens.json"
        ))
        .expect("layout");
        let svg = layout_svg(&layout).expect("svg");
        assert_eq!(svg.matches("<polygon").count(), 3);
        for screen in &layout.screens {
            assert!(svg.contains(&format!(">{}<", screen.id)));
        }
    }
}
