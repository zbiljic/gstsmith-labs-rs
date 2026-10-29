//! Calibration patterns: one fullscreen page per display.
//!
//! Each display shows a white page with its tags drawn exactly where the
//! manifest places them, and its ID in the middle for whoever installs it.
//! Pages stretch to fill the display, so manifest positions stay valid at any
//! browser resolution as long as the page covers the whole screen.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::{GrayImage, Luma};
use wall_geometry::Point;

use crate::manifest::{CellFrame, Manifest, ScreenMarkers};
use crate::tag::{self, BORDER_CELLS, Cells, TAG_CELLS};

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the index is only used when the coordinate lies in [0, TAG_CELLS)"
)]
fn cell_index(coordinate: f64) -> Option<usize> {
    (0.0..tag::TAG_SPAN)
        .contains(&coordinate)
        .then_some(coordinate as usize)
}

/// A display's pattern, sampled in display-local coordinates.
pub struct ScreenPattern {
    tags: Vec<(CellFrame, Cells)>,
}

impl ScreenPattern {
    /// The pattern of `screen`, leaving out any tag in `hidden`.
    pub fn new(screen: &ScreenMarkers, hidden: &[u32]) -> Result<Self> {
        let tags = screen
            .markers
            .iter()
            .filter(|marker| !hidden.contains(&marker.tag))
            .map(|marker| {
                let cells = tag::cells(marker.tag)
                    .with_context(|| format!("tag {} is not in {}", marker.tag, tag::FAMILY))?;
                Ok((marker.frame(), cells))
            })
            .collect::<Result<_>>()?;
        Ok(Self { tags })
    }

    /// Whether the pattern is white at a display-local point.
    #[must_use]
    pub fn is_white(&self, point: Point) -> bool {
        for (frame, cells) in &self.tags {
            if let Some((x, y)) = frame.cell(point)
                && let (Some(x), Some(y)) = (cell_index(x), cell_index(y))
            {
                return cells
                    .get(y)
                    .and_then(|row| row.get(x))
                    .copied()
                    .unwrap_or(true);
            }
        }
        true
    }
}

/// The pattern as an SVG document in display pixels.
pub fn svg(screen: &ScreenMarkers) -> Result<String> {
    let (width, height) = (screen.output.width, screen.output.height);
    let (w, h) = (f64::from(width), f64::from(height));
    let mut svg = String::new();
    writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} {height}" width="{width}" height="{height}" preserveAspectRatio="none">"#
    )?;
    writeln!(
        svg,
        r##"<rect width="{width}" height="{height}" fill="#fff"/>"##
    )?;
    // Screen IDs only contain ASCII letters, digits, '-', '_', and '.', so
    // they need no escaping.
    let font = w.min(h) / 12.0;
    writeln!(
        svg,
        r##"<text x="{}" y="{}" font-family="sans-serif" font-size="{font:.1}" text-anchor="middle" fill="#888">{}</text>"##,
        w / 2.0,
        h / 2.0,
        screen.id
    )?;
    writeln!(
        svg,
        r##"<text x="{}" y="{}" font-family="sans-serif" font-size="{:.1}" text-anchor="middle" fill="#aaa">{width}x{height}</text>"##,
        w / 2.0,
        h / 2.0 + font,
        font / 2.0
    )?;
    for marker in &screen.markers {
        let cells = tag::cells(marker.tag)
            .with_context(|| format!("tag {} is not in {}", marker.tag, tag::FAMILY))?;
        // Map cell coordinates to display pixels.
        let CellFrame {
            origin,
            x_axis,
            y_axis,
        } = marker.frame();
        writeln!(
            svg,
            r#"<g transform="matrix({} {} {} {} {} {})">"#,
            x_axis.x * w,
            x_axis.y * h,
            y_axis.x * w,
            y_axis.y * h,
            origin.x * w,
            origin.y * h
        )?;
        writeln!(
            svg,
            r##"<rect width="{TAG_CELLS}" height="{TAG_CELLS}" fill="#fff"/><rect x="1" y="1" width="{BORDER_CELLS}" height="{BORDER_CELLS}" fill="#000"/>"##
        )?;
        // One path for all white data cells avoids seams between neighbours.
        let mut path = String::new();
        for (y, row) in cells.iter().enumerate().skip(2).take(BORDER_CELLS - 2) {
            for (x, white) in row.iter().enumerate().skip(2).take(BORDER_CELLS - 2) {
                if *white {
                    write!(path, "M{x} {y}h1v1h-1z")?;
                }
            }
        }
        if !path.is_empty() {
            writeln!(svg, r##"<path fill="#fff" d="{path}"/>"##)?;
        }
        writeln!(svg, "</g>")?;
    }
    svg.push_str("</svg>\n");
    Ok(svg)
}

const PAGE: &str = include_str!("pattern.html");

/// A page showing the pattern fullscreen, entering fullscreen on any click or
/// key press. It also shows the browser viewport size, in red when its aspect
/// ratio differs from the manifest's output size.
#[must_use]
pub fn page(screen: &ScreenMarkers, svg: &str) -> String {
    // Screen IDs need no escaping; the SVG goes last so nothing in it is
    // taken for a placeholder.
    PAGE.replace("{{ID}}", &screen.id)
        .replace("{{WIDTH}}", &screen.output.width.to_string())
        .replace("{{HEIGHT}}", &screen.output.height.to_string())
        .replace("{{SVG}}", svg)
}

/// A page linking every display's pattern.
pub fn index(manifest: &Manifest) -> Result<String> {
    let mut rows = String::new();
    for screen in &manifest.screens {
        let tags: Vec<_> = screen
            .markers
            .iter()
            .map(|marker| marker.tag.to_string())
            .collect();
        writeln!(
            rows,
            r#"<li><a href="{id}.html">{id}</a> — {}x{}, tags {}</li>"#,
            screen.output.width,
            screen.output.height,
            tags.join(", "),
            id = screen.id
        )?;
    }
    Ok(format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>wallcalibrate patterns</title>
<style>body {{ font: 16px system-ui, sans-serif; margin: 2em; }} li {{ margin: 0.5em 0; }}</style>
</head>
<body>
<h1>Calibration patterns</h1>
<p>Open each display's page on that display, make it fullscreen, then photograph the whole wall.</p>
<ul>
{rows}</ul>
</body>
</html>
"#
    ))
}

/// The pattern rasterized at the display's output resolution, without the
/// caption.
pub fn raster(screen: &ScreenMarkers) -> Result<GrayImage> {
    let pattern = ScreenPattern::new(screen, &[])?;
    let (width, height) = (screen.output.width, screen.output.height);
    Ok(GrayImage::from_fn(width, height, |x, y| {
        let point = Point::new(
            (f64::from(x) + 0.5) / f64::from(width),
            (f64::from(y) + 0.5) / f64::from(height),
        );
        Luma([if pattern.is_white(point) { 255 } else { 0 }])
    }))
}

/// Write `<id>.svg` and `<id>.html` for every display, `index.html`, and with
/// `png`, `<id>.png` at each display's resolution.
pub fn write(manifest: &Manifest, directory: &Path, png: bool) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let mut written = Vec::new();
    let mut save = |name: String, contents: &str| -> Result<()> {
        let path = directory.join(name);
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
        written.push(path);
        Ok(())
    };
    for screen in &manifest.screens {
        let svg = svg(screen)?;
        save(format!("{}.html", screen.id), &page(screen, &svg))?;
        save(format!("{}.svg", screen.id), &svg)?;
    }
    save("index.html".to_owned(), &index(manifest)?)?;
    if png {
        for screen in &manifest.screens {
            let path = directory.join(format!("{}.png", screen.id));
            raster(screen)?
                .save(&path)
                .with_context(|| format!("writing {}", path.display()))?;
            written.push(path);
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use wall_geometry::Size;

    use super::*;
    use crate::manifest::Placement;

    fn screen() -> ScreenMarkers {
        let screens = [(
            "lobby".to_owned(),
            Size {
                width: 640,
                height: 360,
            },
        )];
        Manifest::generate(&screens, 7, Placement::default())
            .expect("manifest")
            .screens
            .remove(0)
    }

    #[test]
    fn samples_follow_the_tag_cells() {
        let screen = screen();
        let pattern = ScreenPattern::new(&screen, &[]).expect("pattern");
        let marker = screen.markers[0];
        let frame = marker.frame();
        let cells = tag::cells(marker.tag).expect("cells");
        for (y, row) in (0u8..).zip(&cells) {
            for (x, white) in (0u8..).zip(row) {
                let center = frame.point(f64::from(x) + 0.5, f64::from(y) + 0.5);
                assert_eq!(pattern.is_white(center), *white, "cell ({x}, {y})");
            }
        }
        assert!(pattern.is_white(Point::new(0.5, 0.5)));
        let hidden = ScreenPattern::new(&screen, &[marker.tag]).expect("pattern");
        assert!(hidden.is_white(frame.point(1.5, 1.5)));
    }

    #[test]
    fn svg_places_every_tag() {
        let screen = screen();
        let svg = svg(&screen).expect("svg");
        assert!(svg.starts_with("<svg") && svg.trim_end().ends_with("</svg>"));
        assert!(svg.contains(r#"viewBox="0 0 640 360""#) && svg.contains(">lobby<"));
        assert_eq!(svg.matches("<g transform=").count(), screen.markers.len());
        let page = page(&screen, &svg);
        assert!(page.contains("requestFullscreen") && page.contains(&svg));
        assert!(page.contains("{ width: 640, height: 360 }") && page.contains("--screen lobby:"));
        assert!(!page.contains("{{"), "unreplaced placeholder");
    }

    #[test]
    fn raster_matches_the_output_size() {
        let screen = screen();
        let image = raster(&screen).expect("raster");
        assert_eq!(image.dimensions(), (640, 360));
        // The top-left tag's black border starts at 25.2 px (a 14.4 px inset
        // plus one 10.8 px ring cell); the ring and page center are white.
        assert_eq!(image.get_pixel(26, 26).0, [0]);
        assert_eq!(image.get_pixel(20, 20).0, [255]);
        assert_eq!(image.get_pixel(320, 180).0, [255]);
    }
}
