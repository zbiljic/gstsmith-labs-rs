//! Synthetic photos of displays showing their patterns, with known geometry,
//! for testing detection and calibration without a camera.

use anyhow::{Context, Result};
use image::{GrayImage, Luma};
use wall_geometry::{Homography, Layout, Point, Quad};

use crate::calibration::{
    CALIBRATION_VERSION, Calibration, MarkerResult, MarkerState, ScreenCalibration, Status,
};
use crate::detect::Photo;
use crate::manifest::Manifest;
use crate::pattern::ScreenPattern;

/// How the synthetic camera sees the displays.
#[derive(Clone, Debug, PartialEq)]
pub struct SynthOptions {
    pub width: u32,
    pub height: u32,
    /// Brightness around the displays.
    pub background: u8,
    /// Brightness of a display's white and black.
    pub white: u8,
    pub black: u8,
    /// Passes of a 3×3 box blur.
    pub blur: u32,
    /// Largest uniform noise added to each pixel.
    pub noise: f64,
    /// Tags to leave out, as if covered.
    pub hidden_tags: Vec<u32>,
}

impl Default for SynthOptions {
    fn default() -> Self {
        Self {
            width: 2400,
            height: 1600,
            background: 24,
            white: 235,
            black: 20,
            blur: 0,
            noise: 0.0,
            hidden_tags: Vec::new(),
        }
    }
}

/// Subsamples per pixel along each axis.
const SUPERSAMPLING: u32 = 3;

/// Display corners in the photo when the canvas rectangle appears as `view`.
pub fn view_of_layout(layout: &Layout, view: &Quad) -> Result<Vec<(String, Quad)>> {
    let (width, height) = (
        f64::from(layout.canvas.width),
        f64::from(layout.canvas.height),
    );
    let canvas = Quad::new([
        Point::new(0.0, 0.0),
        Point::new(width, 0.0),
        Point::new(width, height),
        Point::new(0.0, height),
    ]);
    let camera = Homography::from_correspondences(&canvas, view).context("camera view")?;
    layout
        .screens
        .iter()
        .map(|screen| {
            let points: Option<Vec<_>> = screen
                .corners
                .corners()
                .iter()
                .map(|p| camera.map(*p))
                .collect();
            let points: [Point; 4] = points
                .and_then(|points| points.try_into().ok())
                .with_context(|| format!("screen {:?} leaves the view", screen.id))?;
            Ok((screen.id.clone(), Quad::new(points)))
        })
        .collect()
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "values are clamped to the u32 or u8 range first"
)]
fn clamp_to<T: TryFrom<u32> + Copy>(value: f64, low: f64, high: f64, fallback: T) -> T {
    T::try_from(value.clamp(low, high) as u32).unwrap_or(fallback)
}

/// Render the displays at `placements` (display corners in photo pixels).
pub fn render(
    manifest: &Manifest,
    placements: &[(String, Quad)],
    options: &SynthOptions,
) -> Result<GrayImage> {
    let mut photo =
        GrayImage::from_pixel(options.width, options.height, Luma([options.background]));
    let (width, height) = (f64::from(options.width), f64::from(options.height));
    let step = 1.0 / f64::from(SUPERSAMPLING);
    for (id, corners) in placements {
        let screen = manifest
            .screen(id)
            .with_context(|| format!("screen {id:?} is not in the manifest"))?;
        let pattern = ScreenPattern::new(screen, &options.hidden_tags)?;
        let photo_to_local = Homography::unit_square_to_quad(corners)
            .and_then(|local_to_photo| local_to_photo.inverse())
            .with_context(|| format!("screen {id:?} corners"))?;
        let points = corners.corners();
        let fold = |pick: fn(&Point) -> f64, op: fn(f64, f64) -> f64, start: f64| {
            points.iter().map(pick).fold(start, op)
        };
        let x0 = clamp_to(
            fold(|p| p.x, f64::min, f64::INFINITY).floor(),
            0.0,
            width,
            0,
        );
        let x1 = clamp_to(
            fold(|p| p.x, f64::max, f64::NEG_INFINITY).ceil(),
            0.0,
            width,
            0,
        );
        let y0 = clamp_to(
            fold(|p| p.y, f64::min, f64::INFINITY).floor(),
            0.0,
            height,
            0,
        );
        let y1 = clamp_to(
            fold(|p| p.y, f64::max, f64::NEG_INFINITY).ceil(),
            0.0,
            height,
            0,
        );
        for y in y0..y1 {
            for x in x0..x1 {
                let Some(pixel) = photo.get_pixel_mut_checked(x, y) else {
                    continue;
                };
                let mut total = 0.0;
                for sy in 0..SUPERSAMPLING {
                    for sx in 0..SUPERSAMPLING {
                        let sample = Point::new(
                            f64::from(x) + (f64::from(sx) + 0.5) * step,
                            f64::from(y) + (f64::from(sy) + 0.5) * step,
                        );
                        let level = match photo_to_local.map(sample) {
                            Some(local)
                                if (0.0..1.0).contains(&local.x)
                                    && (0.0..1.0).contains(&local.y) =>
                            {
                                if pattern.is_white(local) {
                                    options.white
                                } else {
                                    options.black
                                }
                            }
                            _ => pixel.0[0],
                        };
                        total += f64::from(level);
                    }
                }
                let samples = f64::from(SUPERSAMPLING * SUPERSAMPLING);
                *pixel = Luma([clamp_to((total / samples).round(), 0.0, 255.0, u8::MAX)]);
            }
        }
    }
    for _ in 0..options.blur {
        photo = image::imageops::filter3x3(&photo, &[1.0 / 9.0; 9]);
    }
    if options.noise > 0.0 {
        // Deterministic xorshift noise, so tests are repeatable.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for pixel in photo.pixels_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let unit = f64::from(u32::try_from(state >> 40).unwrap_or(0)) / f64::from(1u32 << 24);
            let value = f64::from(pixel.0[0]) + (unit * 2.0 - 1.0) * options.noise;
            *pixel = Luma([clamp_to(value.round(), 0.0, 255.0, u8::MAX)]);
        }
    }
    Ok(photo)
}

/// The exact calibration of a synthetic photo.
pub fn truth(
    manifest: &Manifest,
    placements: &[(String, Quad)],
    options: &SynthOptions,
) -> Result<Calibration> {
    let screens = placements
        .iter()
        .map(|(id, corners)| {
            let screen = manifest
                .screen(id)
                .with_context(|| format!("screen {id:?} is not in the manifest"))?;
            let local_to_photo = Homography::unit_square_to_quad(corners)?;
            let markers = screen
                .markers
                .iter()
                .map(|marker| {
                    let points: Option<Vec<_>> = marker
                        .corners
                        .corners()
                        .iter()
                        .map(|p| local_to_photo.map(*p))
                        .collect();
                    let corners = points
                        .and_then(|points| <[Point; 4]>::try_from(points).ok())
                        .map(Quad::new);
                    let hidden = options.hidden_tags.contains(&marker.tag);
                    MarkerResult {
                        tag: marker.tag,
                        state: if hidden {
                            MarkerState::Missing
                        } else {
                            MarkerState::Used
                        },
                        corners,
                        error: None,
                    }
                })
                .collect();
            Ok(ScreenCalibration {
                id: id.clone(),
                output: screen.output,
                status: Status::Complete,
                corners: Some(*corners),
                rms_error: None,
                max_error: None,
                markers,
                notes: Vec::new(),
            })
        })
        .collect::<Result<_>>()?;
    Ok(Calibration {
        version: CALIBRATION_VERSION,
        detector: "synthetic".to_owned(),
        photo: Photo {
            path: None,
            width: options.width,
            height: options.height,
        },
        screens,
        unknown_markers: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use wall_geometry::Size;

    use super::*;
    use crate::manifest::Placement;

    #[test]
    fn screens_appear_where_placed() {
        let screens = [(
            "solo".to_owned(),
            Size {
                width: 800,
                height: 450,
            },
        )];
        let manifest = Manifest::generate(&screens, 0, Placement::default()).expect("manifest");
        let corners = Quad::new([
            Point::new(100.0, 80.0),
            Point::new(500.0, 60.0),
            Point::new(520.0, 300.0),
            Point::new(90.0, 290.0),
        ]);
        let options = SynthOptions {
            width: 640,
            height: 400,
            ..SynthOptions::default()
        };
        let placements = [("solo".to_owned(), corners)];
        let photo = render(&manifest, &placements, &options).expect("render");
        assert_eq!(photo.dimensions(), (640, 400));
        assert_eq!(photo.get_pixel(5, 5).0, [24]);
        // The display center shows its white page.
        assert_eq!(photo.get_pixel(300, 180).0, [235]);
        let truth = truth(&manifest, &placements, &options).expect("truth");
        assert_eq!(truth.screens[0].corners, Some(corners));
        assert_eq!(truth.screens[0].markers.len(), 4);
    }
}
