use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{GeometryError, Homography, LayoutError, Mask, Orientation, Point, Quad};

/// The layout schema version this crate reads and writes.
pub const LAYOUT_VERSION: u32 = 1;

/// Largest width or height, matching the range of `GStreamer` caps.
pub const MAX_DIMENSION: u32 = i32::MAX.unsigned_abs();

/// Longest accepted screen ID, in bytes.
pub const MAX_ID_LENGTH: usize = 64;

/// A width and height in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    /// Whether both dimensions are between 1 and [`MAX_DIMENSION`].
    #[must_use]
    pub fn is_valid(self) -> bool {
        (1..=MAX_DIMENSION).contains(&self.width) && (1..=MAX_DIMENSION).contains(&self.height)
    }
}

/// One physical display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screen {
    /// Stable identifier, independent of the order of `screens`.
    pub id: String,
    /// Resolution of the stream sent to the display.
    pub output: Size,
    /// The display's corners on the canvas, in its own orientation.
    pub corners: Quad,
    /// The part of the display that shows the picture; all of it when empty.
    #[serde(default, skip_serializing_if = "Mask::is_empty")]
    pub mask: Mask,
}

impl Screen {
    /// Map display-local coordinates in `[0, 1]²` to canvas pixels.
    pub fn local_to_canvas(&self) -> Result<Homography, GeometryError> {
        Homography::unit_square_to_quad(&self.corners)
    }
}

/// A versioned video wall layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub version: u32,
    pub canvas: Size,
    pub screens: Vec<Screen>,
}

#[derive(Deserialize)]
struct VersionProbe {
    version: Option<u64>,
}

fn check_size(field: impl Fn() -> String, size: Size) -> Result<(), LayoutError> {
    for (name, value) in [("width", size.width), ("height", size.height)] {
        if value == 0 || value > MAX_DIMENSION {
            return Err(LayoutError::InvalidDimension {
                field: format!("{} {name}", field()),
                value,
            });
        }
    }
    Ok(())
}

/// Whether `id` can identify a screen: 1 to [`MAX_ID_LENGTH`] ASCII letters,
/// digits, `-`, `_`, or `.`.
#[must_use]
pub fn is_valid_screen_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_LENGTH
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

impl Layout {
    /// Parse and validate a layout.
    pub fn from_json(json: &str) -> Result<Self, LayoutError> {
        // Report an unknown version before schema errors it would explain.
        if let Some(version) = serde_json::from_str::<VersionProbe>(json)?.version
            && version != u64::from(LAYOUT_VERSION)
        {
            return Err(LayoutError::UnsupportedVersion(version));
        }
        let layout: Self = serde_json::from_str(json)?;
        layout.validate()?;
        Ok(layout)
    }

    /// Validate and serialize as pretty-printed JSON.
    pub fn to_json(&self) -> Result<String, LayoutError> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Check the version, dimensions, screen IDs, and screen geometry.
    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.version != LAYOUT_VERSION {
            return Err(LayoutError::UnsupportedVersion(self.version.into()));
        }
        check_size(|| "canvas".to_owned(), self.canvas)?;
        if self.screens.is_empty() {
            return Err(LayoutError::NoScreens);
        }
        let mut ids = HashSet::new();
        for screen in &self.screens {
            let id = &screen.id;
            if !is_valid_screen_id(id) {
                return Err(LayoutError::InvalidId(id.clone()));
            }
            if !ids.insert(id.as_str()) {
                return Err(LayoutError::DuplicateId(id.clone()));
            }
            check_size(|| format!("screen {id:?} output"), screen.output)?;
            let geometry = |error| LayoutError::Geometry {
                id: id.clone(),
                error,
            };
            match screen.corners.orientation().map_err(geometry)? {
                Orientation::Normal => (),
                Orientation::Mirrored => return Err(LayoutError::Mirrored(id.clone())),
            }
            screen.local_to_canvas().map_err(geometry)?;
            self.local_to_source(screen).map_err(geometry)?;
        }
        Ok(())
    }

    /// The screen with the given ID.
    #[must_use]
    pub fn screen(&self, id: &str) -> Option<&Screen> {
        self.screens.iter().find(|screen| screen.id == id)
    }

    /// A screen's corners in normalized source coordinates.
    #[must_use]
    pub fn source_quad(&self, screen: &Screen) -> Quad {
        let width = f64::from(self.canvas.width);
        let height = f64::from(self.canvas.height);
        screen
            .corners
            .map(|corner| Point::new(corner.x / width, corner.y / height))
    }

    /// Map display-local coordinates in `[0, 1]²` to normalized source
    /// coordinates.
    pub fn local_to_source(&self, screen: &Screen) -> Result<Homography, GeometryError> {
        Homography::unit_square_to_quad(&self.source_quad(screen))
    }

    /// Require a source frame with the canvas aspect ratio, to within one
    /// source pixel in either dimension.
    pub fn check_source_size(&self, width: u32, height: u32) -> Result<(), LayoutError> {
        check_size(|| "source".to_owned(), Size { width, height })?;
        let canvas = self.canvas;
        // |width - height · canvas.width / canvas.height| < 1, and likewise for
        // height, without rounding.
        let error = (i128::from(width) * i128::from(canvas.height)
            - i128::from(height) * i128::from(canvas.width))
        .unsigned_abs();
        if error >= u128::from(canvas.width.min(canvas.height)) {
            return Err(LayoutError::SourceAspect {
                source: (width, height),
                canvas: (canvas.width, canvas.height),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"{
      "version": 1,
      "canvas": { "width": 1920, "height": 1080 },
      "screens": [
        {
          "id": "display-a",
          "output": { "width": 1280, "height": 720 },
          "corners": [[80, 100], [780, 90], [800, 500], [90, 520]]
        },
        {
          "id": "display-b",
          "output": { "width": 1080, "height": 1920 },
          "corners": [[1100, 30], [1370, 60], [1330, 920], [1060, 900]]
        },
        {
          "id": "display-c",
          "output": { "width": 1024, "height": 576 },
          "corners": [[350, 700], [960, 660], [970, 1000], [360, 1030]]
        }
      ]
    }"#;

    fn example() -> Layout {
        Layout::from_json(EXAMPLE).expect("example layout")
    }

    fn edit(change: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut value: serde_json::Value = serde_json::from_str(EXAMPLE).expect("example JSON");
        change(&mut value);
        value.to_string()
    }

    fn rejects(json: &str) -> LayoutError {
        Layout::from_json(json).expect_err("invalid layout")
    }

    #[test]
    fn example_layout_parses() {
        let layout = example();
        assert_eq!(layout.version, 1);
        assert_eq!(
            layout.canvas,
            Size {
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(layout.screens.len(), 3);
        let b = layout.screen("display-b").expect("display-b");
        assert_eq!(
            b.output,
            Size {
                width: 1080,
                height: 1920
            }
        );
        assert_eq!(b.corners.top_right, Point::new(1370.0, 60.0));
        assert!(layout.screen("display-d").is_none());
    }

    #[test]
    fn json_round_trips() {
        let layout = example();
        let json = layout.to_json().expect("serialize");
        assert_eq!(Layout::from_json(&json).expect("reparse"), layout);
    }

    #[test]
    fn masks_are_optional_and_checked() {
        let layout = example();
        assert!(layout.screens.iter().all(|screen| screen.mask.is_empty()));
        assert!(!layout.to_json().expect("serialize").contains("mask"));
        let json = edit(|value| {
            value["screens"][0]["mask"] = serde_json::json!([[[0.5, 0], [1, 1], [0, 1]]]);
        });
        let masked = Layout::from_json(&json).expect("masked layout");
        let mask = &masked.screen("display-a").expect("display-a").mask;
        assert!(mask.shows(0.5, 0.9));
        assert!(!mask.shows(0.1, 0.1));
        let json = masked.to_json().expect("serialize");
        assert_eq!(Layout::from_json(&json).expect("reparse"), masked);
        let json = edit(|value| {
            value["screens"][0]["mask"] = serde_json::json!([[[0.5, 0], [1, 1]]]);
        });
        assert!(matches!(rejects(&json), LayoutError::Json(_)));
    }

    #[test]
    fn screens_map_local_coordinates_to_their_corners() {
        let layout = example();
        for screen in &layout.screens {
            let canvas = screen.local_to_canvas().expect("canvas mapping");
            let source = layout.local_to_source(screen).expect("source mapping");
            let normalized = layout.source_quad(screen);
            for ((unit, corner), normalized) in Quad::UNIT
                .corners()
                .into_iter()
                .zip(screen.corners.corners())
                .zip(normalized.corners())
            {
                let mapped = canvas.map(unit).expect("corner");
                assert!((mapped.x - corner.x).abs() < 1e-9 && (mapped.y - corner.y).abs() < 1e-9);
                let mapped = source.map(unit).expect("corner");
                assert!((mapped.x - corner.x / 1920.0).abs() < 1e-12);
                assert!((mapped.y - corner.y / 1080.0).abs() < 1e-12);
                assert!((normalized.x - corner.x / 1920.0).abs() < f64::EPSILON);
            }
        }
    }

    #[test]
    fn schema_errors_are_reported() {
        assert!(matches!(rejects("{}"), LayoutError::Json(_)));
        assert!(matches!(rejects("[]"), LayoutError::Json(_)));
        assert!(matches!(
            rejects(&edit(|v| v["canvas"]["depth"] = 8.into())),
            LayoutError::Json(_)
        ));
        assert!(matches!(
            rejects(&edit(|v| v["screens"][0]["output"]["width"] = (-1).into())),
            LayoutError::Json(_)
        ));
        assert!(matches!(
            rejects(&edit(
                |v| v["screens"][0]["corners"][0] = serde_json::json!([1])
            )),
            LayoutError::Json(_)
        ));
        assert!(matches!(
            rejects(&EXAMPLE.replace("[80, 100]", "[1e400, 100]")),
            LayoutError::Json(_)
        ));
    }

    #[test]
    fn versions_other_than_one_are_rejected() {
        assert!(matches!(
            rejects(&edit(|v| v["version"] = 2.into())),
            LayoutError::UnsupportedVersion(2)
        ));
        assert!(matches!(
            rejects(&edit(|v| {
                v["version"] = 7.into();
                v["future"] = true.into();
            })),
            LayoutError::UnsupportedVersion(7)
        ));
        let mut layout = example();
        layout.version = 0;
        assert!(matches!(
            layout.validate(),
            Err(LayoutError::UnsupportedVersion(0))
        ));
    }

    #[test]
    fn dimensions_must_be_positive_and_representable() {
        for (pointer, value) in [
            ("/canvas/width", 0),
            ("/screens/1/output/height", 0),
            ("/canvas/height", u64::from(MAX_DIMENSION) + 1),
        ] {
            let json = edit(|v| *v.pointer_mut(pointer).expect("field") = value.into());
            assert!(
                matches!(rejects(&json), LayoutError::InvalidDimension { .. }),
                "{pointer}"
            );
        }
    }

    #[test]
    fn screen_ids_must_be_present_valid_and_unique() {
        assert!(matches!(
            rejects(&edit(|v| v["screens"] = serde_json::json!([]))),
            LayoutError::NoScreens
        ));
        for id in [
            "",
            "has space",
            "slash/id",
            "ünicode",
            &"x".repeat(MAX_ID_LENGTH + 1),
        ] {
            assert!(
                matches!(
                    rejects(&edit(|v| v["screens"][0]["id"] = id.into())),
                    LayoutError::InvalidId(_)
                ),
                "{id:?}"
            );
        }
        assert!(matches!(
            rejects(&edit(|v| v["screens"][2]["id"] = "display-a".into())),
            LayoutError::DuplicateId(id) if id == "display-a"
        ));
        assert!(matches!(
            rejects(&edit(|v| {
                v["screens"][0]
                    .as_object_mut()
                    .expect("screen")
                    .remove("id");
            })),
            LayoutError::Json(_)
        ));
        assert!(is_valid_screen_id("Lobby_Left-2.v1"));
    }

    #[test]
    fn invalid_corners_name_their_screen() {
        let bow_tie = serde_json::json!([[80, 100], [800, 500], [780, 90], [90, 520]]);
        assert!(matches!(
            rejects(&edit(|v| v["screens"][0]["corners"] = bow_tie)),
            LayoutError::Geometry { id, error: GeometryError::SelfIntersecting } if id == "display-a"
        ));
        let collinear = serde_json::json!([[0, 0], [10, 0], [20, 0], [0, 10]]);
        assert!(matches!(
            rejects(&edit(|v| v["screens"][1]["corners"] = collinear)),
            LayoutError::Geometry { id, error: GeometryError::Degenerate } if id == "display-b"
        ));
        let mirrored = serde_json::json!([[780, 90], [80, 100], [90, 520], [800, 500]]);
        assert!(matches!(
            rejects(&edit(|v| v["screens"][2]["corners"] = mirrored)),
            LayoutError::Mirrored(id) if id == "display-c"
        ));
    }

    #[test]
    fn rotated_screens_keep_a_normal_winding() {
        // display-b rotated 90° clockwise: its own top-left is the canvas top-right.
        let rotated = serde_json::json!([[1370, 60], [1330, 920], [1060, 900], [1100, 30]]);
        let layout = Layout::from_json(&edit(|v| v["screens"][1]["corners"] = rotated))
            .expect("rotated layout");
        let screen = layout.screen("display-b").expect("display-b");
        let origin = screen
            .local_to_canvas()
            .expect("mapping")
            .map(Point::new(0.0, 0.0))
            .expect("origin");
        assert_eq!(origin, Point::new(1370.0, 60.0));
    }

    #[test]
    fn source_aspect_ratio_must_match_the_canvas() {
        let layout = example();
        for (width, height) in [
            (1920, 1080),
            (1280, 720),
            (3840, 2160),
            (1366, 768),
            (854, 480),
        ] {
            assert!(
                layout.check_source_size(width, height).is_ok(),
                "{width}x{height}"
            );
        }
        for (width, height) in [(1920, 1088), (1080, 1920), (1440, 1080), (0, 1080)] {
            assert!(
                layout.check_source_size(width, height).is_err(),
                "{width}x{height}"
            );
        }
    }

    #[test]
    fn errors_describe_the_problem() {
        let error = rejects(&edit(|v| v["screens"][2]["id"] = "display-a".into()));
        assert_eq!(
            error.to_string(),
            "screen ID \"display-a\" is used more than once"
        );
        let error = rejects(&edit(|v| v["canvas"]["width"] = 0.into()));
        assert_eq!(
            error.to_string(),
            "canvas width must be between 1 and 2147483647, got 0"
        );
    }
}
