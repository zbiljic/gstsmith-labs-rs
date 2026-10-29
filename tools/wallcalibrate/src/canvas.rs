//! From photo-space display corners to a `wall-geometry` canvas layout.
//!
//! Two mappings are offered because one photo supports two different claims:
//!
//! - [`Mode::Viewpoint`] keeps the photo's own geometry. The canvas is the
//!   photo, cropped and scaled, so the source video looks correct from where
//!   the photo was taken. It works for any arrangement in space, but
//!   perspective distortion is part of the result.
//! - [`Mode::Planar`] assumes every display lies in one plane, such as a wall,
//!   and removes the camera's perspective. The reference display's corners are
//!   mapped to an upright rectangle of its known aspect ratio, which fixes the
//!   plane up to scale. Displays off that plane come out wrong. Physical units
//!   are only known when the reference display's size is given; lens
//!   distortion is not modelled.
//!
//! Two adjustments apply on top of either mode. *Leveling* rotates a
//! viewpoint layout so an anchor display's edges are horizontal, removing the
//! camera's roll; a planar layout is already aligned to its reference.
//! *Straightening* assumes the displays were meant to be upright and in line,
//! and that the photo shows small errors: every display becomes an upright
//! rectangle with its output's aspect ratio, turned only in quarter turns, so
//! it shows a plain crop of the source instead of a warped one.

use std::f64::consts::FRAC_PI_2;

use anyhow::{Context, Result, bail, ensure};
use wall_geometry::{
    Homography, LAYOUT_VERSION, Layout, MAX_DIMENSION, Orientation, Point, Quad, Screen, Size,
};

use crate::calibration::{Calibration, ScreenCalibration};

/// How photo coordinates become canvas coordinates.
#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    /// Keep the camera's view.
    Viewpoint,
    /// Rectify a plane of displays using a reference display.
    Planar {
        reference: String,
        /// Physical width and height of the reference display's picture, in
        /// any unit. Defaults to its output size, assuming square pixels.
        size: Option<(f64, f64)>,
    },
}

/// Canvas size in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanvasSize {
    /// This width, with the height following the displays' extent.
    Width(u32),
    /// Exactly the displays' extent, its longer side this many pixels, so no
    /// part of the canvas falls outside every display's extent.
    Longest(u32),
    /// The displays' extent fitted and centered in this size.
    Fit { width: u32, height: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayoutOptions {
    pub mode: Mode,
    pub canvas: CanvasSize,
    /// Leave out displays without corners instead of failing.
    pub allow_incomplete: bool,
    /// Viewpoint mode only: rotate the layout so this display's edges are
    /// horizontal and vertical, to the nearest quarter turn.
    pub level: Option<String>,
    /// Make every display an upright rectangle with its output's aspect ratio.
    pub straighten: bool,
}

/// A generated layout and what it leaves out.
#[derive(Clone, Debug, PartialEq)]
pub struct GeneratedLayout {
    pub layout: Layout,
    /// Displays left out because they have no corners.
    pub omitted: Vec<String>,
    /// Reference-size units per canvas pixel, when the reference size is known.
    pub units_per_pixel: Option<f64>,
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is checked to be finite and within 1..=MAX_DIMENSION first"
)]
fn dimension(value: f64) -> Result<u32> {
    let rounded = value.round();
    ensure!(
        rounded.is_finite() && (1.0..=f64::from(MAX_DIMENSION)).contains(&rounded),
        "canvas dimension {value} is out of range"
    );
    Ok(rounded as u32)
}

/// The map from photo pixels to the reference plane, or `None` to keep the
/// photo's own geometry.
fn rectification(placed: &[(&ScreenCalibration, Quad)], mode: &Mode) -> Result<Option<Homography>> {
    let Mode::Planar { reference, size } = mode else {
        return Ok(None);
    };
    let (screen, corners) = placed
        .iter()
        .find(|(screen, _)| screen.id == *reference)
        .with_context(|| format!("reference screen {reference:?} has no corners"))?;
    let (width, height) = size.unwrap_or((
        f64::from(screen.output.width),
        f64::from(screen.output.height),
    ));
    ensure!(
        width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0,
        "reference size must be positive"
    );
    let rectangle = Quad::new([
        Point::new(0.0, 0.0),
        Point::new(width, 0.0),
        Point::new(width, height),
        Point::new(0.0, height),
    ]);
    let plane = Homography::from_correspondences(corners, &rectangle)
        .with_context(|| format!("rectifying with reference screen {reference:?}"))?;
    Ok(Some(plane))
}

/// The angle of a quad's top and bottom edges, averaged.
fn direction(quad: &Quad) -> f64 {
    let [top_left, top_right, bottom_right, bottom_left] = quad.corners();
    let x = (top_right.x - top_left.x) + (bottom_right.x - bottom_left.x);
    let y = (top_right.y - top_left.y) + (bottom_right.y - bottom_left.y);
    y.atan2(x)
}

/// `angle` less its nearest quarter turn, and that quarter turn, `0..4`.
fn quarter_turns(angle: f64) -> (f64, i64) {
    let turns = (angle / FRAC_PI_2).round();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "atan2 angles lie within ±π, so there are at most two quarter turns"
    )]
    (angle - turns * FRAC_PI_2, (turns as i64).rem_euclid(4))
}

/// Rotate `quads` about the origin so `anchor`'s edges are horizontal and
/// vertical, keeping its quarter turns.
fn level(quads: &mut [Quad], anchor: usize) -> Result<()> {
    let quad = quads.get(anchor).context("no anchor display")?;
    let (roll, _) = quarter_turns(direction(quad));
    let (sin, cos) = (-roll).sin_cos();
    for quad in quads {
        *quad = quad.map(|p| Point::new(p.x * cos - p.y * sin, p.x * sin + p.y * cos));
    }
    Ok(())
}

/// The upright rectangle `quad` was presumably meant to be: same center and
/// area, the aspect ratio of `output`, turned to its nearest quarter turn.
fn straighten(quad: &Quad, output: Size) -> Quad {
    let [top_left, top_right, bottom_right, bottom_left] = quad.corners();
    let length = |a: Point, b: Point| (a.x - b.x).hypot(a.y - b.y);
    let width = f64::midpoint(
        length(top_left, top_right),
        length(bottom_left, bottom_right),
    );
    let height = f64::midpoint(
        length(top_left, bottom_left),
        length(top_right, bottom_right),
    );
    let aspect = f64::from(output.width) / f64::from(output.height);
    let (half_width, half_height) = (
        (width * height * aspect).sqrt() / 2.0,
        (width * height / aspect).sqrt() / 2.0,
    );
    let center = Point::new(
        quad.corners().iter().map(|p| p.x).sum::<f64>() / 4.0,
        quad.corners().iter().map(|p| p.y).sum::<f64>() / 4.0,
    );
    let (_, turns) = quarter_turns(direction(quad));
    let turn = |x: f64, y: f64| {
        let (x, y) = match turns {
            1 => (-y, x),
            2 => (-x, -y),
            3 => (y, -x),
            _ => (x, y),
        };
        Point::new(center.x + x, center.y + y)
    };
    Quad::new([
        turn(-half_width, -half_height),
        turn(half_width, -half_height),
        turn(half_width, half_height),
        turn(-half_width, half_height),
    ])
}

/// The scale and offset placing the bounds of `quads` on the canvas, and the
/// canvas size.
fn fit_canvas(quads: &[Quad], canvas: CanvasSize) -> Result<(f64, Point, Size)> {
    let (mut low, mut high) = (
        Point::new(f64::INFINITY, f64::INFINITY),
        Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY),
    );
    for corner in quads.iter().flat_map(Quad::corners) {
        low = Point::new(low.x.min(corner.x), low.y.min(corner.y));
        high = Point::new(high.x.max(corner.x), high.y.max(corner.y));
    }
    let (extent_x, extent_y) = (high.x - low.x, high.y - low.y);
    ensure!(
        extent_x > 0.0 && extent_y > 0.0,
        "the displays cover no area"
    );
    let (scale, margin, size) = match canvas {
        CanvasSize::Width(width) => {
            let scale = f64::from(width) / extent_x;
            let height = dimension(extent_y * scale)?;
            (scale, Point::new(0.0, 0.0), Size { width, height })
        }
        CanvasSize::Longest(longest) => {
            let scale = f64::from(longest) / extent_x.max(extent_y);
            let size = Size {
                width: dimension(extent_x * scale)?,
                height: dimension(extent_y * scale)?,
            };
            (scale, Point::new(0.0, 0.0), size)
        }
        CanvasSize::Fit { width, height } => {
            let scale = (f64::from(width) / extent_x).min(f64::from(height) / extent_y);
            let margin = Point::new(
                (f64::from(width) - extent_x * scale) / 2.0,
                (f64::from(height) - extent_y * scale) / 2.0,
            );
            (scale, margin, Size { width, height })
        }
    };
    let offset = Point::new(margin.x - low.x * scale, margin.y - low.y * scale);
    Ok((scale, offset, size))
}

/// Lay out every display with corners on a canvas.
pub fn layout(calibration: &Calibration, options: &LayoutOptions) -> Result<GeneratedLayout> {
    calibration.validate()?;
    let omitted: Vec<String> = calibration
        .missing()
        .into_iter()
        .map(str::to_owned)
        .collect();
    if !omitted.is_empty() && !options.allow_incomplete {
        bail!(
            "calibration is incomplete: no corners for {}; add them by hand or allow an \
             incomplete layout",
            omitted.join(", ")
        );
    }
    let placed: Vec<_> = calibration
        .screens
        .iter()
        .filter_map(|screen| Some((screen, screen.corners?)))
        .collect();
    ensure!(!placed.is_empty(), "no screen has corners");

    let plane = rectification(&placed, &options.mode)?;
    let mut quads = Vec::new();
    for (screen, corners) in &placed {
        let quad = match &plane {
            None => *corners,
            Some(plane) => {
                let points: Option<Vec<_>> =
                    corners.corners().iter().map(|p| plane.map(*p)).collect();
                let points: [Point; 4] = points
                    .and_then(|points| points.try_into().ok())
                    .with_context(|| format!("screen {:?} maps to infinity", screen.id))?;
                Quad::new(points)
            }
        };
        if quad.orientation() != Ok(Orientation::Normal) {
            bail!(
                "screen {:?} does not map to an unmirrored convex shape on the reference \
                 plane; planar mode needs every display in one plane",
                screen.id
            );
        }
        quads.push(quad);
    }
    if let Some(anchor) = &options.level {
        ensure!(
            options.mode == Mode::Viewpoint,
            "leveling applies to viewpoint layouts; planar layouts align to their reference"
        );
        let index = placed
            .iter()
            .position(|(screen, _)| screen.id == *anchor)
            .with_context(|| format!("anchor screen {anchor:?} has no corners"))?;
        level(&mut quads, index)?;
    }
    if options.straighten {
        for ((screen, _), quad) in placed.iter().zip(&mut quads) {
            *quad = straighten(quad, screen.output);
        }
    }

    let (scale, offset, canvas) = fit_canvas(&quads, options.canvas)?;
    let layout = Layout {
        version: LAYOUT_VERSION,
        canvas,
        screens: placed
            .iter()
            .zip(&quads)
            .map(|((screen, _), quad)| Screen {
                id: screen.id.clone(),
                output: screen.output,
                corners: quad.map(|p| Point::new(p.x * scale + offset.x, p.y * scale + offset.y)),
            })
            .collect(),
    };
    layout.validate()?;
    let units_per_pixel = match &options.mode {
        Mode::Planar { size: Some(_), .. } => Some(1.0 / scale),
        _ => None,
    };
    Ok(GeneratedLayout {
        layout,
        omitted,
        units_per_pixel,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::{CALIBRATION_VERSION, ScreenCalibration, Status};
    use crate::detect::Photo;

    fn quad(values: [[f64; 2]; 4]) -> Quad {
        Quad::new(values.map(Point::from))
    }

    /// Two 16:9 displays side by side on a wall, 40 units apart, and a camera
    /// view of that wall in perspective.
    fn wall() -> (Vec<(&'static str, Quad)>, Homography) {
        let screens = vec![
            (
                "left",
                quad([[0.0, 0.0], [160.0, 0.0], [160.0, 90.0], [0.0, 90.0]]),
            ),
            (
                "right",
                quad([[200.0, 10.0], [360.0, 10.0], [360.0, 100.0], [200.0, 100.0]]),
            ),
        ];
        let wall = quad([[0.0, 0.0], [360.0, 0.0], [360.0, 100.0], [0.0, 100.0]]);
        let photo = quad([
            [300.0, 250.0],
            [1500.0, 150.0],
            [1450.0, 700.0],
            [320.0, 620.0],
        ]);
        (
            screens,
            Homography::from_correspondences(&wall, &photo).expect("camera"),
        )
    }

    fn calibration(screens: &[(&str, Quad)], camera: &Homography) -> Calibration {
        Calibration {
            version: CALIBRATION_VERSION,
            detector: "test".to_owned(),
            photo: Photo {
                path: None,
                width: 1800,
                height: 900,
            },
            screens: screens
                .iter()
                .map(|(id, quad)| ScreenCalibration {
                    id: (*id).to_owned(),
                    output: Size {
                        width: 1920,
                        height: 1080,
                    },
                    status: Status::Complete,
                    corners: Some(quad.map(|p| camera.map(p).expect("finite"))),
                    rms_error: None,
                    max_error: None,
                    markers: Vec::new(),
                    notes: Vec::new(),
                })
                .collect(),
            unknown_markers: Vec::new(),
        }
    }

    fn options(mode: Mode) -> LayoutOptions {
        LayoutOptions {
            mode,
            canvas: CanvasSize::Width(1440),
            allow_incomplete: false,
            level: None,
            straighten: false,
        }
    }

    fn close(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-6 && (a.y - b.y).abs() < 1e-6
    }

    /// A calibration of displays photographed straight on, with the camera
    /// rolled by `roll` radians about the photo's origin.
    fn rolled(screens: &[(&str, Quad, Size)], roll: f64) -> Calibration {
        let (sin, cos) = roll.sin_cos();
        let mut calibration = calibration(&[], &Homography::IDENTITY);
        calibration.screens = screens
            .iter()
            .map(|(id, quad, output)| ScreenCalibration {
                id: (*id).to_owned(),
                output: *output,
                status: Status::Complete,
                corners: Some(quad.map(|p| {
                    Point::new(400.0 + p.x * cos - p.y * sin, 300.0 + p.x * sin + p.y * cos)
                })),
                rms_error: None,
                max_error: None,
                markers: Vec::new(),
                notes: Vec::new(),
            })
            .collect();
        calibration
    }

    #[test]
    fn leveling_removes_the_camera_roll_and_keeps_quarter_turns() {
        let landscape = Size {
            width: 1600,
            height: 900,
        };
        let portrait = Size {
            width: 900,
            height: 1600,
        };
        // A landscape display, and a portrait panel mounted turned a quarter
        // turn clockwise, its top edge pointing down.
        let screens = [
            (
                "main",
                quad([[0.0, 0.0], [160.0, 0.0], [160.0, 90.0], [0.0, 90.0]]),
                landscape,
            ),
            (
                "side",
                quad([[300.0, 0.0], [300.0, 90.0], [240.0, 90.0], [240.0, 0.0]]),
                portrait,
            ),
        ];
        let calibration = rolled(&screens, 0.08);
        let mut options = options(Mode::Viewpoint);
        options.canvas = CanvasSize::Longest(1200);
        let tilted = layout(&calibration, &options).expect("layout");
        let main = tilted.layout.screens[0].corners;
        assert!((main.top_right.y - main.top_left.y).abs() > 1.0, "rolled");
        options.level = Some("side".to_owned());
        let leveled = layout(&calibration, &options).expect("layout");
        let [main, side] = [0, 1].map(|index| leveled.layout.screens[index].corners);
        assert!((main.top_right.y - main.top_left.y).abs() < 1e-6);
        // The side panel keeps its quarter turn: its top edge still points down.
        assert!((side.top_right.x - side.top_left.x).abs() < 1e-6);
        assert!(side.top_right.y > side.top_left.y);
        // The canvas is exactly the extent, 1200 pixels on its longer side.
        assert_eq!(
            leveled.layout.canvas,
            Size {
                width: 1200,
                height: 360
            }
        );
        options.mode = Mode::Planar {
            reference: "main".to_owned(),
            size: None,
        };
        let error = layout(&calibration, &options).expect_err("planar leveling");
        assert!(error.to_string().contains("leveling"), "{error}");
    }

    #[test]
    fn straightened_displays_are_upright_crops_of_their_aspect_ratio() {
        let (screens, camera) = wall();
        let calibration = calibration(&screens, &camera);
        let mut options = options(Mode::Viewpoint);
        options.straighten = true;
        let generated = layout(&calibration, &options).expect("layout");
        for screen in &generated.layout.screens {
            let [top_left, top_right, bottom_right, bottom_left] = screen.corners.corners();
            assert!((top_left.y - top_right.y).abs() < 1e-6);
            assert!((bottom_left.y - bottom_right.y).abs() < 1e-6);
            assert!((top_left.x - bottom_left.x).abs() < 1e-6);
            let aspect = (top_right.x - top_left.x) / (bottom_left.y - top_left.y);
            assert!((aspect - 16.0 / 9.0).abs() < 1e-9, "{aspect}");
        }
        // A rectangle with the right aspect ratio stays as it is.
        let exact = quad([[10.0, 20.0], [170.0, 20.0], [170.0, 110.0], [10.0, 110.0]]);
        let output = Size {
            width: 1600,
            height: 900,
        };
        let straightened = straighten(&exact, output);
        for (a, b) in exact.corners().into_iter().zip(straightened.corners()) {
            assert!(close(a, b), "{a:?} != {b:?}");
        }
        // A slightly rotated, perspective-distorted display keeps its center
        // and area.
        let skewed = quad([[12.0, 18.0], [171.0, 23.0], [168.0, 108.0], [9.0, 111.0]]);
        let fixed = straighten(&skewed, output);
        assert!(close(
            Point::new(
                fixed.corners().iter().map(|p| p.x).sum::<f64>() / 4.0,
                fixed.corners().iter().map(|p| p.y).sum::<f64>() / 4.0
            ),
            Point::new(90.0, 65.0)
        ));
        assert_eq!(fixed.orientation(), Ok(Orientation::Normal));
    }

    #[test]
    fn planar_mode_recovers_the_wall_up_to_scale() {
        let (screens, camera) = wall();
        let calibration = calibration(&screens, &camera);
        let mode = Mode::Planar {
            reference: "left".to_owned(),
            size: Some((160.0, 90.0)),
        };
        let generated = layout(&calibration, &options(mode)).expect("layout");
        // 360 wall units across 1440 px: four pixels per unit.
        assert_eq!(
            generated.layout.canvas,
            Size {
                width: 1440,
                height: 400
            }
        );
        assert!(
            generated
                .units_per_pixel
                .is_some_and(|units| (units - 0.25).abs() < 1e-9)
        );
        for ((_, truth), screen) in screens.iter().zip(&generated.layout.screens) {
            for (expected, actual) in truth.corners().iter().zip(screen.corners.corners()) {
                assert!(
                    (expected.x * 4.0 - actual.x).abs() < 1e-6
                        && (expected.y * 4.0 - actual.y).abs() < 1e-6,
                    "{actual:?} != 4 × {expected:?}"
                );
            }
        }
        // Without a measured size, square pixels give the same shape and no units.
        let mode = Mode::Planar {
            reference: "left".to_owned(),
            size: None,
        };
        let generated = layout(&calibration, &options(mode)).expect("layout");
        assert_eq!(generated.layout.canvas.height, 400);
        assert_eq!(generated.units_per_pixel, None);
    }

    #[test]
    fn viewpoint_mode_keeps_the_photo_perspective() {
        let (screens, camera) = wall();
        let calibration = calibration(&screens, &camera);
        let generated = layout(&calibration, &options(Mode::Viewpoint)).expect("layout");
        let left = &generated.layout.screens[0].corners;
        // The displays are similar to their photo outlines, not to the wall.
        let photo = calibration.screens[0].corners.expect("corners");
        // Left over right edge length: one on the wall, not in the photo.
        let ratio = |q: &Quad| {
            (q.bottom_left.x - q.top_left.x).hypot(q.bottom_left.y - q.top_left.y)
                / (q.bottom_right.x - q.top_right.x).hypot(q.bottom_right.y - q.top_right.y)
        };
        assert!((ratio(left) - ratio(&photo)).abs() < 1e-9);
        assert!(
            (ratio(left) - 1.0).abs() > 0.05,
            "perspective remains: {}",
            ratio(left)
        );
        assert_eq!(generated.layout.canvas.width, 1440);
        assert_eq!(generated.units_per_pixel, None);
    }

    #[test]
    fn fitted_canvases_center_the_displays() {
        let (screens, camera) = wall();
        let calibration = calibration(&screens, &camera);
        let mut options = options(Mode::Planar {
            reference: "left".to_owned(),
            size: None,
        });
        options.canvas = CanvasSize::Fit {
            width: 1920,
            height: 1080,
        };
        let generated = layout(&calibration, &options).expect("layout");
        assert_eq!(
            generated.layout.canvas,
            Size {
                width: 1920,
                height: 1080
            }
        );
        let first = generated.layout.screens[0].corners.top_left;
        // 360x100 fitted into 1920x1080 is 1920x533.3, centered vertically.
        assert!(first.x.abs() < 1e-6 && (first.y - (1080.0 - 1920.0 / 3.6) / 2.0).abs() < 1e-6);
    }

    #[test]
    fn missing_screens_need_permission_and_references_need_corners() {
        let (screens, camera) = wall();
        let mut calibration = calibration(&screens, &camera);
        calibration.screens[1].corners = None;
        calibration.screens[1].status = Status::Missing;
        let error = layout(&calibration, &options(Mode::Viewpoint)).expect_err("incomplete");
        assert!(error.to_string().contains("right"), "{error}");
        let mut allowed = options(Mode::Viewpoint);
        allowed.allow_incomplete = true;
        let generated = layout(&calibration, &allowed).expect("layout");
        assert_eq!(generated.omitted, ["right"]);
        assert_eq!(generated.layout.screens.len(), 1);
        allowed.mode = Mode::Planar {
            reference: "right".to_owned(),
            size: None,
        };
        let error = layout(&calibration, &allowed).expect_err("missing reference");
        assert!(error.to_string().contains("reference screen"), "{error}");
    }

    #[test]
    fn off_plane_screens_are_reported() {
        let (screens, camera) = wall();
        let mut calibration = calibration(&screens, &camera);
        // A display straddling the wall's horizon in the photo cannot lie in
        // the wall plane.
        let [_, _, [along_x, along_y, offset]] = camera.inverse().expect("inverse").matrix();
        let scale = -offset / (along_x * along_x + along_y * along_y);
        let (x, y) = (along_x * scale, along_y * scale);
        let mut tilted = calibration.screens[0].clone();
        tilted.id = "tilted".to_owned();
        tilted.corners = Some(quad([
            [x - 50.0, y - 50.0],
            [x + 50.0, y - 50.0],
            [x + 50.0, y + 50.0],
            [x - 50.0, y + 50.0],
        ]));
        calibration.screens.push(tilted);
        let mode = Mode::Planar {
            reference: "left".to_owned(),
            size: None,
        };
        let error = layout(&calibration, &options(mode)).expect_err("off-plane screen");
        assert!(error.to_string().contains("tilted"), "{error}");
        let generated = layout(&calibration, &options(Mode::Viewpoint)).expect("viewpoint");
        assert_eq!(generated.layout.screens.len(), 3);
    }
}
