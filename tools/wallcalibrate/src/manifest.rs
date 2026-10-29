//! Calibration manifests: which tags each display shows, and where.

use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use wall_geometry::{Layout, Orientation, Point, Quad, Size, is_valid_screen_id};

use crate::tag::{BORDER_SPAN, FAMILY, TAG_COUNT, TAG_SPAN};

/// The manifest schema version this crate reads and writes.
pub const MANIFEST_VERSION: u32 = 1;

/// Largest deviation of marker corners from a parallelogram, in display-local
/// units. Patterns are drawn with affine transforms.
const PARALLELOGRAM_TOLERANCE: f64 = 1e-6;

/// Tags shown by every display, with stable display IDs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub family: String,
    pub screens: Vec<ScreenMarkers>,
}

/// The tags one display shows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenMarkers {
    pub id: String,
    pub output: Size,
    pub markers: Vec<Marker>,
}

/// One tag on a display.
///
/// `corners` are the tag's black border corners in display-local coordinates
/// (`[0, 1]²` across the output), listed top-left, top-right, bottom-right,
/// bottom-left in the tag's own orientation. The white ring lies one cell
/// outside them.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub tag: u32,
    pub corners: Quad,
}

/// Affine frame of a tag's cells in display-local coordinates: cell `(0, 0)`
/// is the outer ring's top-left and `(10, 10)` its bottom-right.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellFrame {
    pub origin: Point,
    pub x_axis: Point,
    pub y_axis: Point,
}

impl CellFrame {
    #[must_use]
    pub fn point(&self, x: f64, y: f64) -> Point {
        Point::new(
            self.origin.x + x * self.x_axis.x + y * self.y_axis.x,
            self.origin.y + x * self.x_axis.y + y * self.y_axis.y,
        )
    }

    /// Cell coordinates of a display-local point.
    #[must_use]
    pub fn cell(&self, point: Point) -> Option<(f64, f64)> {
        let (dx, dy) = (point.x - self.origin.x, point.y - self.origin.y);
        let determinant = self.x_axis.x * self.y_axis.y - self.y_axis.x * self.x_axis.y;
        if determinant.abs() < f64::MIN_POSITIVE {
            return None;
        }
        Some((
            (dx * self.y_axis.y - dy * self.y_axis.x) / determinant,
            (dy * self.x_axis.x - dx * self.x_axis.y) / determinant,
        ))
    }
}

impl Marker {
    #[must_use]
    pub fn frame(&self) -> CellFrame {
        let Quad {
            top_left,
            top_right,
            bottom_left,
            ..
        } = self.corners;
        let x_axis = Point::new(
            (top_right.x - top_left.x) / BORDER_SPAN,
            (top_right.y - top_left.y) / BORDER_SPAN,
        );
        let y_axis = Point::new(
            (bottom_left.x - top_left.x) / BORDER_SPAN,
            (bottom_left.y - top_left.y) / BORDER_SPAN,
        );
        // The black border starts one cell inside the white ring.
        let origin = Point::new(
            top_left.x - x_axis.x - y_axis.x,
            top_left.y - x_axis.y - y_axis.y,
        );
        CellFrame {
            origin,
            x_axis,
            y_axis,
        }
    }

    /// The tag's full extent, including its white ring.
    #[must_use]
    pub fn outer(&self) -> Quad {
        let frame = self.frame();
        Quad::new([
            frame.point(0.0, 0.0),
            frame.point(TAG_SPAN, 0.0),
            frame.point(TAG_SPAN, TAG_SPAN),
            frame.point(0.0, TAG_SPAN),
        ])
    }
}

/// Generated marker size and margin, as fractions of a display's shorter side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// Side of each tag, white ring included.
    pub size: f64,
    /// Gap between the display edge and the tag's white ring.
    pub inset: f64,
}

impl Default for Placement {
    fn default() -> Self {
        Self {
            size: 0.3,
            inset: 0.04,
        }
    }
}

fn bounds(quad: &Quad) -> (Point, Point) {
    let corners = quad.corners();
    let fold = |pick: fn(Point) -> f64, start: f64, op: fn(f64, f64) -> f64| {
        corners.iter().map(|corner| pick(*corner)).fold(start, op)
    };
    (
        Point::new(
            fold(|p| p.x, f64::INFINITY, f64::min),
            fold(|p| p.y, f64::INFINITY, f64::min),
        ),
        Point::new(
            fold(|p| p.x, f64::NEG_INFINITY, f64::max),
            fold(|p| p.y, f64::NEG_INFINITY, f64::max),
        ),
    )
}

fn check_marker(id: &str, marker: &Marker) -> Result<()> {
    let tag = marker.tag;
    ensure!(
        usize::try_from(tag).is_ok_and(|tag| tag < TAG_COUNT),
        "screen {id:?}: tag {tag} is outside {FAMILY} (0 to {})",
        TAG_COUNT - 1
    );
    match marker.corners.orientation() {
        Ok(Orientation::Normal) => (),
        Ok(Orientation::Mirrored) => bail!("screen {id:?}: tag {tag} corners are mirrored"),
        Err(err) => bail!("screen {id:?}: tag {tag} corners: {err}"),
    }
    let Quad {
        top_left,
        top_right,
        bottom_right,
        bottom_left,
    } = marker.corners;
    let gap = (top_right.x + bottom_left.x - top_left.x - bottom_right.x)
        .abs()
        .max((top_right.y + bottom_left.y - top_left.y - bottom_right.y).abs());
    ensure!(
        gap <= PARALLELOGRAM_TOLERANCE,
        "screen {id:?}: tag {tag} corners must form a parallelogram"
    );
    let (low, high) = bounds(&marker.outer());
    let inside =
        |value: f64| (-PARALLELOGRAM_TOLERANCE..=1.0 + PARALLELOGRAM_TOLERANCE).contains(&value);
    ensure!(
        inside(low.x) && inside(low.y) && inside(high.x) && inside(high.y),
        "screen {id:?}: tag {tag} and its white ring must lie inside the display"
    );
    Ok(())
}

impl Manifest {
    /// Four corner markers per screen, with tags numbered from `first_tag` in
    /// screen order: top-left, top-right, bottom-right, bottom-left.
    pub fn generate(
        screens: &[(String, Size)],
        first_tag: u32,
        placement: Placement,
    ) -> Result<Self> {
        let Placement { size, inset } = placement;
        ensure!(
            size > 0.0 && inset >= 0.0 && 2.0 * (size + inset) < 1.0,
            "marker size {size} with inset {inset} does not fit twice across a display"
        );
        let mut next_tag = first_tag;
        let screens = screens
            .iter()
            .map(|(id, output)| {
                let (width, height) = (f64::from(output.width), f64::from(output.height));
                let shorter = width.min(height);
                let (side, inset) = (size * shorter, inset * shorter);
                let cell = side / TAG_SPAN;
                let (left, right) = (inset, width - inset - side);
                let (top, bottom) = (inset, height - inset - side);
                let markers = [(left, top), (right, top), (right, bottom), (left, bottom)]
                    .into_iter()
                    .map(|(x, y)| {
                        let (x0, y0) = (x + cell, y + cell);
                        let (x1, y1) = (x0 + BORDER_SPAN * cell, y0 + BORDER_SPAN * cell);
                        let local = |x: f64, y: f64| Point::new(x / width, y / height);
                        let marker = Marker {
                            tag: next_tag,
                            corners: Quad::new([
                                local(x0, y0),
                                local(x1, y0),
                                local(x1, y1),
                                local(x0, y1),
                            ]),
                        };
                        next_tag = next_tag.saturating_add(1);
                        marker
                    })
                    .collect();
                ScreenMarkers {
                    id: id.clone(),
                    output: *output,
                    markers,
                }
            })
            .collect();
        let manifest = Self {
            version: MANIFEST_VERSION,
            family: FAMILY.to_owned(),
            screens,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// [`generate`](Self::generate) for the screens of a layout.
    pub fn from_layout(layout: &Layout, first_tag: u32, placement: Placement) -> Result<Self> {
        let screens: Vec<_> = layout
            .screens
            .iter()
            .map(|screen| (screen.id.clone(), screen.output))
            .collect();
        Self::generate(&screens, first_tag, placement)
    }

    /// Parse and validate a manifest.
    pub fn from_json(json: &str) -> Result<Self> {
        let manifest: Self = serde_json::from_str(json).context("invalid manifest JSON")?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate and serialize as pretty-printed JSON.
    pub fn to_json(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Check the version, family, screen IDs, and that every tag is unique,
    /// in the family, and fully inside its display without overlapping another.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == MANIFEST_VERSION,
            "unsupported manifest version {}; expected {MANIFEST_VERSION}",
            self.version
        );
        ensure!(
            self.family == FAMILY,
            "unsupported marker family {:?}; expected {FAMILY:?}",
            self.family
        );
        ensure!(
            !self.screens.is_empty(),
            "manifest must contain at least one screen"
        );
        let mut ids = HashSet::new();
        let mut tags = HashSet::new();
        for screen in &self.screens {
            let id = screen.id.as_str();
            ensure!(is_valid_screen_id(id), "invalid screen ID {id:?}");
            ensure!(ids.insert(id), "screen ID {id:?} is used more than once");
            ensure!(
                screen.output.is_valid(),
                "screen {id:?} has an invalid output size"
            );
            ensure!(!screen.markers.is_empty(), "screen {id:?} shows no markers");
            for (index, marker) in screen.markers.iter().enumerate() {
                check_marker(id, marker)?;
                ensure!(
                    tags.insert(marker.tag),
                    "tag {} is used more than once",
                    marker.tag
                );
                let (low, high) = bounds(&marker.outer());
                for other in screen.markers.iter().skip(index + 1) {
                    let (other_low, other_high) = bounds(&other.outer());
                    ensure!(
                        high.x <= other_low.x
                            || other_high.x <= low.x
                            || high.y <= other_low.y
                            || other_high.y <= low.y,
                        "screen {id:?}: tags {} and {} overlap",
                        marker.tag,
                        other.tag
                    );
                }
            }
        }
        Ok(())
    }

    /// The screen with the given ID.
    #[must_use]
    pub fn screen(&self, id: &str) -> Option<&ScreenMarkers> {
        self.screens.iter().find(|screen| screen.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An expected error message and the edit causing it.
    type Edit = (&'static str, fn(&mut Manifest));

    fn screens() -> Vec<(String, Size)> {
        vec![
            (
                "wide".to_owned(),
                Size {
                    width: 1280,
                    height: 720,
                },
            ),
            (
                "tall".to_owned(),
                Size {
                    width: 1080,
                    height: 1920,
                },
            ),
        ]
    }

    fn manifest() -> Manifest {
        Manifest::generate(&screens(), 10, Placement::default()).expect("manifest")
    }

    fn near(a: Point, b: Point) -> bool {
        (a.x - b.x).abs() < 1e-12 && (a.y - b.y).abs() < 1e-12
    }

    #[test]
    fn generated_markers_sit_in_the_corners() {
        let manifest = manifest();
        let tags: Vec<_> = manifest
            .screens
            .iter()
            .flat_map(|screen| screen.markers.iter().map(|marker| marker.tag))
            .collect();
        assert_eq!(tags, (10..18).collect::<Vec<_>>());

        // A 216 px tag (21.6 px cells) inset by 28.8 px on a 1280x720 display.
        let wide = &manifest.screens[0];
        let first = wide.markers[0].corners;
        assert!(near(
            first.top_left,
            Point::new(50.4 / 1280.0, 50.4 / 720.0)
        ));
        assert!(near(
            first.bottom_right,
            Point::new(223.2 / 1280.0, 223.2 / 720.0)
        ));
        let last = wide.markers[3].corners;
        assert!(near(
            last.bottom_left,
            Point::new(50.4 / 1280.0, (720.0 - 50.4) / 720.0)
        ));
        // Tags are square in display pixels whatever the aspect ratio.
        for screen in &manifest.screens {
            let (width, height) = (
                f64::from(screen.output.width),
                f64::from(screen.output.height),
            );
            for marker in &screen.markers {
                let Quad {
                    top_left,
                    top_right,
                    bottom_left,
                    ..
                } = marker.corners;
                let across = (top_right.x - top_left.x) * width;
                let down = (bottom_left.y - top_left.y) * height;
                assert!((across - down).abs() < 1e-9, "{across} != {down}");
            }
        }
    }

    #[test]
    fn frames_map_cells_to_corners_and_back() {
        let marker = manifest().screens[1].markers[2];
        let frame = marker.frame();
        assert!(near(frame.point(1.0, 1.0), marker.corners.top_left));
        assert!(near(frame.point(9.0, 9.0), marker.corners.bottom_right));
        let (x, y) = frame.cell(marker.corners.top_right).expect("cell");
        assert!((x - 9.0).abs() < 1e-9 && (y - 1.0).abs() < 1e-9);
    }

    #[test]
    fn json_round_trips() {
        let manifest = manifest();
        let json = manifest.to_json().expect("serialize");
        assert_eq!(Manifest::from_json(&json).expect("parse"), manifest);
    }

    #[test]
    fn layouts_name_the_screens() {
        let layout = Layout::from_json(include_str!(
            "../../../video/wallwarp/examples/three-screens.json"
        ))
        .expect("layout");
        let manifest = Manifest::from_layout(&layout, 0, Placement::default()).expect("manifest");
        let ids: Vec<_> = manifest
            .screens
            .iter()
            .map(|screen| screen.id.as_str())
            .collect();
        assert_eq!(ids, ["display-c", "display-a", "display-b"]);
        assert_eq!(
            manifest.screen("display-b").expect("display-b").output,
            Size {
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn invalid_manifests_are_rejected() {
        let edits: [Edit; 10] = [
            ("version", |m| m.version = 2),
            ("family", |m| m.family = "tag16h5".to_owned()),
            ("used more than once", |m| m.screens[1].markers[0].tag = 10),
            ("outside tag36h11", |m| m.screens[0].markers[0].tag = 587),
            ("invalid screen ID", |m| m.screens[0].id = "a b".to_owned()),
            ("shows no markers", |m| m.screens[0].markers.clear()),
            ("screen ID", |m| m.screens[1].id = m.screens[0].id.clone()),
            ("mirrored", |m| {
                let corners = m.screens[0].markers[0].corners.corners();
                let [a, b, c, d] = corners;
                m.screens[0].markers[0].corners = Quad::new([b, a, d, c]);
            }),
            ("inside the display", |m| {
                m.screens[0].markers[0].corners = m.screens[0].markers[0]
                    .corners
                    .map(|p| Point::new(p.x - 0.039, p.y));
            }),
            ("overlap", |m| {
                let marker = m.screens[0].markers[0];
                m.screens[0].markers[1].corners =
                    marker.corners.map(|p| Point::new(p.x + 0.05, p.y));
            }),
        ];
        for (message, edit) in edits {
            let mut manifest = manifest();
            edit(&mut manifest);
            let error = manifest.validate().expect_err(message).to_string();
            assert!(error.contains(message), "{message:?} not in {error:?}");
        }
        let error = Manifest::generate(
            &screens(),
            0,
            Placement {
                size: 0.45,
                inset: 0.06,
            },
        )
        .expect_err("oversized markers")
        .to_string();
        assert!(error.contains("does not fit"), "{error}");
        let error = Manifest::generate(&screens(), 584, Placement::default())
            .expect_err("tags beyond the family")
            .to_string();
        assert!(error.contains("outside tag36h11"), "{error}");
    }
}
