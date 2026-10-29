//! Display corners in photo pixels, fitted from detected markers.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use wall_geometry::{Homography, Orientation, Point, Quad, Size};

use crate::detect::{DetectedMarker, Detections, Photo};
use crate::manifest::{Manifest, ScreenMarkers};

/// The calibration schema version this crate reads and writes.
pub const CALIBRATION_VERSION: u32 = 1;

/// A marker disagreeing with a screen's fit by more than this many pixels…
const REJECT_PIXELS: f64 = 2.0;
/// …and by more than this fraction of its own diagonal is rejected.
const REJECT_FRACTION: f64 = 0.05;

/// Where every display appears in one photo.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
    pub version: u32,
    pub detector: String,
    pub photo: Photo,
    pub screens: Vec<ScreenCalibration>,
    /// Detected tags that belong to no screen in the manifest.
    #[serde(default)]
    pub unknown_markers: Vec<DetectedMarker>,
}

/// How a display's corners were obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Fitted from every marker.
    Complete,
    /// Fitted from some markers; the others were not found or were rejected.
    Partial,
    /// Too few markers were found; the display has no corners.
    Missing,
    /// Corners were entered or adjusted by hand.
    Manual,
}

/// One display's place in the photo.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenCalibration {
    pub id: String,
    pub output: Size,
    pub status: Status,
    /// Display corners in photo pixels, in the display's own top-left,
    /// top-right, bottom-right, bottom-left order; `null` when missing.
    pub corners: Option<Quad>,
    /// Root-mean-square distance between detected and fitted marker corners,
    /// in photo pixels.
    pub rms_error: Option<f64>,
    /// Largest such distance.
    pub max_error: Option<f64>,
    pub markers: Vec<MarkerResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// What happened to one of a display's markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkerState {
    /// Detected and used in the fit.
    Used,
    /// Detected but inconsistent with the display's other markers.
    Rejected,
    /// Not detected.
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkerResult {
    pub tag: u32,
    pub state: MarkerState,
    /// Detected corners in photo pixels.
    pub corners: Option<Quad>,
    /// Largest distance between a detected and fitted corner, in photo pixels.
    pub error: Option<f64>,
}

/// Fitting options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SolveOptions {
    /// Fewest markers that give a display corners. One marker determines a
    /// fit exactly but extrapolates its errors across the whole display.
    pub min_markers: usize,
}

impl Default for SolveOptions {
    fn default() -> Self {
        Self { min_markers: 2 }
    }
}

fn distance(a: Point, b: Point) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

/// Largest distance between a marker's detected corners and where `fit`
/// places its manifest corners.
fn marker_error(fit: &Homography, local: &Quad, detected: &Quad) -> f64 {
    local
        .corners()
        .iter()
        .zip(detected.corners())
        .map(|(local, detected)| {
            fit.map(*local)
                .map_or(f64::INFINITY, |p| distance(p, detected))
        })
        .fold(0.0, f64::max)
}

/// Fit a screen's used markers, rejecting the marker that disagrees most with
/// the others while it is far off and enough markers remain. On failure the
/// error explains why the screen has no corners.
fn fit_markers(
    screen: &ScreenMarkers,
    markers: &mut [MarkerResult],
    notes: &mut Vec<String>,
    minimum: usize,
) -> Result<(Homography, Vec<(Point, Point)>), String> {
    loop {
        let used: Vec<_> = screen
            .markers
            .iter()
            .zip(markers.iter())
            .filter(|(_, result)| result.state == MarkerState::Used)
            .filter_map(|(marker, result)| Some((marker.corners, result.corners?)))
            .collect();
        if used.len() < minimum {
            return Err(format!(
                "found {} of {} markers; at least {minimum} are needed",
                used.len(),
                screen.markers.len()
            ));
        }
        let pairs: Vec<_> = used
            .iter()
            .flat_map(|(local, detected)| local.corners().into_iter().zip(detected.corners()))
            .collect();
        let fit =
            Homography::fit(&pairs).map_err(|err| format!("cannot fit the markers: {err}"))?;
        for (marker, result) in screen.markers.iter().zip(markers.iter_mut()) {
            result.error = result
                .corners
                .map(|detected| marker_error(&fit, &marker.corners, &detected));
        }

        let worst = markers
            .iter_mut()
            .filter(|result| result.state == MarkerState::Used)
            .max_by(|a, b| a.error.unwrap_or(0.0).total_cmp(&b.error.unwrap_or(0.0)));
        if let Some(worst) = worst
            && let (Some(error), Some(detected)) = (worst.error, worst.corners)
            && error
                > REJECT_PIXELS
                    .max(REJECT_FRACTION * distance(detected.top_left, detected.bottom_right))
        {
            if used.len() > minimum {
                worst.state = MarkerState::Rejected;
                notes.push(format!(
                    "tag {} disagreed with the other markers by {error:.1} px and was rejected",
                    worst.tag
                ));
                continue;
            }
            notes.push(format!(
                "tag {} disagrees with the other markers by {error:.1} px, but too few markers \
                 remain to reject it",
                worst.tag
            ));
        }
        return Ok((fit, pairs));
    }
}

/// Where `fit` places the display's own corners.
fn display_corners(fit: &Homography) -> Result<Quad, String> {
    let corners: Option<Vec<_>> = Quad::UNIT
        .corners()
        .iter()
        .map(|unit| fit.map(*unit))
        .collect();
    let Some(Ok(corners)) = corners.map(<[Point; 4]>::try_from) else {
        return Err("display corners map to infinity".to_owned());
    };
    let corners = Quad::new(corners);
    match corners.orientation() {
        Ok(Orientation::Normal) => Ok(corners),
        Ok(Orientation::Mirrored) => Err("the display appears mirrored".to_owned()),
        Err(err) => Err(format!("display corners: {err}")),
    }
}

fn solve_screen(
    screen: &ScreenMarkers,
    found: &HashMap<u32, DetectedMarker>,
    duplicates: &HashSet<u32>,
    options: SolveOptions,
) -> ScreenCalibration {
    let mut markers: Vec<_> = screen
        .markers
        .iter()
        .map(|marker| {
            let corners = found.get(&marker.tag).map(|detected| detected.corners);
            let state = if corners.is_some() {
                MarkerState::Used
            } else {
                MarkerState::Missing
            };
            MarkerResult {
                tag: marker.tag,
                state,
                corners,
                error: None,
            }
        })
        .collect();
    let mut notes: Vec<String> = screen
        .markers
        .iter()
        .filter(|marker| duplicates.contains(&marker.tag))
        .map(|marker| {
            format!(
                "tag {} was detected more than once; the most confident detection was kept",
                marker.tag
            )
        })
        .collect();
    let mut calibration = ScreenCalibration {
        id: screen.id.clone(),
        output: screen.output,
        status: Status::Missing,
        corners: None,
        rms_error: None,
        max_error: None,
        markers: Vec::new(),
        notes: Vec::new(),
    };
    let fitted = fit_markers(screen, &mut markers, &mut notes, options.min_markers.max(1))
        .and_then(|(fit, pairs)| Ok((display_corners(&fit)?, fit, pairs)));
    match fitted {
        Err(note) => notes.push(note),
        Ok((corners, fit, pairs)) => {
            let residuals: Vec<_> = pairs
                .iter()
                .map(|(local, detected)| {
                    fit.map(*local)
                        .map_or(f64::INFINITY, |p| distance(p, *detected))
                })
                .collect();
            let mut sum_squares = 0.0;
            let mut count = 0.0;
            for residual in &residuals {
                sum_squares += residual * residual;
                count += 1.0;
            }
            let complete = markers
                .iter()
                .all(|result| result.state == MarkerState::Used);
            calibration.status = if complete {
                Status::Complete
            } else {
                Status::Partial
            };
            calibration.corners = Some(corners);
            calibration.rms_error = Some((sum_squares / count).sqrt());
            calibration.max_error = Some(residuals.iter().copied().fold(0.0, f64::max));
        }
    }
    calibration.markers = markers;
    calibration.notes = notes;
    calibration
}

/// Fit every manifest screen's corners from its detected markers.
#[must_use]
pub fn solve(manifest: &Manifest, detections: &Detections, options: SolveOptions) -> Calibration {
    let margin = |marker: &DetectedMarker| marker.margin.unwrap_or(f64::NEG_INFINITY);
    let mut found = HashMap::new();
    let mut duplicates = HashSet::new();
    for marker in &detections.markers {
        match found.entry(marker.tag) {
            Entry::Vacant(entry) => {
                entry.insert(*marker);
            }
            Entry::Occupied(mut entry) => {
                duplicates.insert(marker.tag);
                if margin(marker) > margin(entry.get()) {
                    entry.insert(*marker);
                }
            }
        }
    }
    let known: HashSet<_> = manifest
        .screens
        .iter()
        .flat_map(|screen| screen.markers.iter().map(|marker| marker.tag))
        .collect();
    let mut unknown_markers: Vec<_> = detections
        .markers
        .iter()
        .filter(|marker| !known.contains(&marker.tag))
        .copied()
        .collect();
    unknown_markers.sort_by_key(|marker| marker.tag);
    Calibration {
        version: CALIBRATION_VERSION,
        detector: detections.detector.clone(),
        photo: detections.photo.clone(),
        screens: manifest
            .screens
            .iter()
            .map(|screen| solve_screen(screen, &found, &duplicates, options))
            .collect(),
        unknown_markers,
    }
}

impl Calibration {
    /// Parse and validate a calibration, including hand-edited ones.
    pub fn from_json(json: &str) -> Result<Self> {
        let calibration: Self = serde_json::from_str(json).context("invalid calibration JSON")?;
        calibration.validate()?;
        Ok(calibration)
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Check the version, unique screen IDs, and that every screen's corners
    /// form an unmirrored convex quadrilateral.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == CALIBRATION_VERSION,
            "unsupported calibration version {}; expected {CALIBRATION_VERSION}",
            self.version
        );
        let mut ids = HashSet::new();
        for screen in &self.screens {
            ensure!(
                ids.insert(screen.id.as_str()),
                "screen ID {:?} is used more than once",
                screen.id
            );
            if let Some(corners) = &screen.corners {
                match corners.orientation() {
                    Ok(Orientation::Normal) => (),
                    Ok(Orientation::Mirrored) => anyhow::bail!(
                        "screen {:?} corners wind counter-clockwise; list them top-left, \
                         top-right, bottom-right, bottom-left in the display's own orientation",
                        screen.id
                    ),
                    Err(err) => anyhow::bail!("screen {:?} corners: {err}", screen.id),
                }
            }
        }
        Ok(())
    }

    /// IDs of screens without corners.
    #[must_use]
    pub fn missing(&self) -> Vec<&str> {
        self.screens
            .iter()
            .filter(|screen| screen.corners.is_none())
            .map(|screen| screen.id.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use wall_geometry::Size;

    use super::*;
    use crate::detect::DETECTIONS_VERSION;
    use crate::manifest::Placement;
    use crate::tag::FAMILY;

    fn manifest() -> Manifest {
        let screens = [
            (
                "left".to_owned(),
                Size {
                    width: 1280,
                    height: 720,
                },
            ),
            (
                "right".to_owned(),
                Size {
                    width: 1080,
                    height: 1920,
                },
            ),
        ];
        Manifest::generate(&screens, 0, Placement::default()).expect("manifest")
    }

    fn truth() -> [Quad; 2] {
        let quad = |values: [[f64; 2]; 4]| Quad::new(values.map(Point::from));
        [
            quad([
                [100.0, 120.0],
                [820.0, 100.0],
                [840.0, 520.0],
                [110.0, 540.0],
            ]),
            quad([
                [1000.0, 60.0],
                [1300.0, 80.0],
                [1290.0, 900.0],
                [980.0, 880.0],
            ]),
        ]
    }

    /// Detections of every marker where `truth` places it, with a small
    /// deterministic offset on every other corner.
    fn detections(manifest: &Manifest, skip: &[u32], jitter: f64) -> Detections {
        let mut markers = Vec::new();
        for (screen, quad) in manifest.screens.iter().zip(truth()) {
            let fit = Homography::unit_square_to_quad(&quad).expect("truth");
            for marker in screen
                .markers
                .iter()
                .filter(|marker| !skip.contains(&marker.tag))
            {
                let mut sign = 1.0;
                let corners = Quad::new(marker.corners.corners().map(|local| {
                    sign = -sign;
                    let point = fit.map(local).expect("finite");
                    Point::new(point.x + sign * jitter, point.y - sign * jitter)
                }));
                markers.push(DetectedMarker {
                    tag: marker.tag,
                    corners,
                    hamming: Some(0),
                    margin: Some(100.0),
                });
            }
        }
        Detections {
            version: DETECTIONS_VERSION,
            family: FAMILY.to_owned(),
            detector: "test".to_owned(),
            photo: Photo {
                path: None,
                width: 1600,
                height: 1000,
            },
            markers,
        }
    }

    fn assert_corners(actual: Option<Quad>, expected: Quad, tolerance: f64) {
        let actual = actual.expect("corners");
        for (a, e) in actual.corners().iter().zip(expected.corners()) {
            assert!(distance(*a, e) < tolerance, "{a:?} != {e:?}");
        }
    }

    #[test]
    fn complete_screens_recover_their_corners() {
        let manifest = manifest();
        let calibration = solve(
            &manifest,
            &detections(&manifest, &[], 0.3),
            SolveOptions::default(),
        );
        for (screen, expected) in calibration.screens.iter().zip(truth()) {
            assert_eq!(screen.status, Status::Complete, "{}", screen.id);
            assert_corners(screen.corners, expected, 1.0);
            assert!(screen.rms_error.is_some_and(|rms| rms < 0.5), "{screen:?}");
            assert!(screen.notes.is_empty(), "{:?}", screen.notes);
        }
        assert_eq!(calibration.missing(), Vec::<&str>::new());
        let json = calibration.to_json().expect("serialize");
        assert_eq!(Calibration::from_json(&json).expect("parse"), calibration);
    }

    #[test]
    fn missing_markers_are_reported_not_invented() {
        let manifest = manifest();
        // Two of "left" and three of "right" missing.
        let calibration = solve(
            &manifest,
            &detections(&manifest, &[0, 2, 4, 5, 6], 0.0),
            SolveOptions::default(),
        );
        let [left, right] = &calibration.screens[..] else {
            panic!("two screens");
        };
        assert_eq!(left.status, Status::Partial);
        assert_corners(left.corners, truth()[0], 1e-6);
        let states: Vec<_> = left.markers.iter().map(|marker| marker.state).collect();
        assert_eq!(
            states,
            [
                MarkerState::Missing,
                MarkerState::Used,
                MarkerState::Missing,
                MarkerState::Used
            ]
        );
        assert_eq!(right.status, Status::Missing);
        assert_eq!(right.corners, None);
        assert!(
            right.notes.iter().any(|note| note.contains("found 1 of 4")),
            "{:?}",
            right.notes
        );
        assert_eq!(calibration.missing(), ["right"]);

        let single = solve(
            &manifest,
            &detections(&manifest, &[4, 5, 6], 0.0),
            SolveOptions { min_markers: 1 },
        );
        assert_eq!(single.screens[1].status, Status::Partial);
    }

    #[test]
    fn inconsistent_markers_are_rejected() {
        let manifest = manifest();
        let mut detections = detections(&manifest, &[], 0.0);
        // Tag 1 appears 40 px away from where the other markers put it.
        let moved = detections
            .markers
            .iter_mut()
            .find(|marker| marker.tag == 1)
            .expect("tag 1");
        moved.corners = moved.corners.map(|p| Point::new(p.x + 40.0, p.y));
        let calibration = solve(&manifest, &detections, SolveOptions::default());
        let left = &calibration.screens[0];
        assert_eq!(left.status, Status::Partial);
        assert_eq!(left.markers[1].state, MarkerState::Rejected);
        assert!(left.markers[1].error.is_some_and(|error| error > 20.0));
        assert_corners(left.corners, truth()[0], 1e-6);
        assert!(
            left.notes.iter().any(|note| note.contains("tag 1")),
            "{:?}",
            left.notes
        );
    }

    #[test]
    fn duplicates_keep_the_most_confident_detection_and_unknown_tags_are_listed() {
        let manifest = manifest();
        let mut detections = detections(&manifest, &[], 0.0);
        let mut reflection = *detections.markers.first().expect("tag 0");
        reflection.corners = reflection.corners.map(|p| Point::new(p.x, p.y + 300.0));
        reflection.margin = Some(5.0);
        detections.markers.push(reflection);
        let mut stray = reflection;
        stray.tag = 99;
        detections.markers.push(stray);
        let calibration = solve(&manifest, &detections, SolveOptions::default());
        assert_eq!(calibration.screens[0].status, Status::Complete);
        assert!(calibration.screens[0].notes[0].contains("more than once"));
        let unknown: Vec<_> = calibration
            .unknown_markers
            .iter()
            .map(|marker| marker.tag)
            .collect();
        assert_eq!(unknown, [99]);
    }

    #[test]
    fn hand_edited_calibrations_are_validated() {
        let manifest = manifest();
        let mut calibration = solve(
            &manifest,
            &detections(&manifest, &[], 0.0),
            SolveOptions::default(),
        );
        let corners = calibration.screens[0].corners.expect("corners");
        let [a, b, c, d] = corners.corners();
        calibration.screens[0].corners = Some(Quad::new([b, a, d, c]));
        let error = calibration.validate().expect_err("mirrored").to_string();
        assert!(error.contains("counter-clockwise"), "{error}");
        calibration.screens[0].corners = Some(Quad::new([a, c, b, d]));
        assert!(
            calibration
                .validate()
                .expect_err("bow tie")
                .to_string()
                .contains("intersect")
        );
    }
}
