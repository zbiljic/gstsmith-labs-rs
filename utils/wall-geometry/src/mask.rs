use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{GeometryError, Point};

/// Most points a mask may have, over all its polygons.
pub const MAX_MASK_POINTS: usize = 256;

/// Largest coordinate a mask point may have, positive or negative: far
/// outside any display, and well within single precision, in which GPUs test
/// masks.
pub const MAX_MASK_COORDINATE: f64 = 1e6;

/// The part of a display that shows the picture, for displays that are not
/// rectangular or are partly covered.
///
/// Polygons are in display-local coordinates, `[0, 1]²` across the output
/// with `(0, 0)` its top-left corner, and serialized as
/// `[[[x, y], [x, y], [x, y], ...], ...]`. A pixel shows the picture when its
/// center is inside an odd number of polygons, so a polygon inside another
/// cuts a hole in it. The empty mask shows the picture everywhere.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Vec<Point>>", into = "Vec<Vec<Point>>")]
pub struct Mask {
    polygons: Vec<Vec<Point>>,
}

impl Mask {
    /// A mask of `polygons`, each of at least three points with coordinates
    /// within ±[`MAX_MASK_COORDINATE`], with at most [`MAX_MASK_POINTS`]
    /// points in all.
    pub fn new(polygons: Vec<Vec<Point>>) -> Result<Self, GeometryError> {
        if polygons.iter().any(|polygon| polygon.len() < 3) {
            return Err(GeometryError::Parse(
                "every mask polygon needs at least three points".to_owned(),
            ));
        }
        let points = polygons.iter().map(Vec::len).sum::<usize>();
        if points > MAX_MASK_POINTS {
            return Err(GeometryError::Parse(format!(
                "a mask has at most {MAX_MASK_POINTS} points, got {points}"
            )));
        }
        if !polygons.iter().flatten().all(|point| point.is_finite()) {
            return Err(GeometryError::NonFinite);
        }
        let within = |value: f64| value.abs() <= MAX_MASK_COORDINATE;
        if !polygons
            .iter()
            .flatten()
            .all(|point| within(point.x) && within(point.y))
        {
            return Err(GeometryError::Parse(format!(
                "mask coordinates must be within ±{MAX_MASK_COORDINATE}"
            )));
        }
        Ok(Self { polygons })
    }

    /// Whether the mask shows the picture everywhere.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.polygons.is_empty()
    }

    #[must_use]
    pub fn polygons(&self) -> &[Vec<Point>] {
        &self.polygons
    }

    /// Every polygon edge, from each point to the next and from the last
    /// back to the first.
    pub fn edges(&self) -> impl Iterator<Item = (Point, Point)> + '_ {
        self.polygons.iter().flat_map(|polygon| {
            polygon
                .iter()
                .zip(polygon.iter().cycle().skip(1))
                .map(|(from, to)| (*from, *to))
        })
    }

    /// Whether the display shows the picture at display-local `(u, v)`.
    #[must_use]
    pub fn shows(&self, u: f64, v: f64) -> bool {
        self.is_empty() || self.edges().filter(|edge| crosses(*edge, u, v)).count() % 2 == 1
    }

    /// Where the edges cross the horizontal line at `v`, as the `u` of each
    /// crossing, unsorted: [`Mask::shows`] holds at `(u, v)` when an odd
    /// number of them are greater than `u`.
    pub fn crossings(&self, v: f64) -> impl Iterator<Item = f64> + '_ {
        self.edges()
            .filter(move |(from, to)| (from.y > v) != (to.y > v))
            .map(move |(from, to)| crossing(from, to, v))
    }
}

/// The `u` where the edge from `from` to `to` crosses the line at `v`.
fn crossing(from: Point, to: Point, v: f64) -> f64 {
    (to.x - from.x) * (v - from.y) / (to.y - from.y) + from.x
}

/// Whether a ray from `(u, v)` to the right crosses the edge. An edge counts
/// at its lower end and not at its upper one, so a ray through a corner
/// crosses once or twice as the outline does.
fn crosses((from, to): (Point, Point), u: f64, v: f64) -> bool {
    (from.y > v) != (to.y > v) && u < crossing(from, to, v)
}

impl TryFrom<Vec<Vec<Point>>> for Mask {
    type Error = GeometryError;

    fn try_from(polygons: Vec<Vec<Point>>) -> Result<Self, Self::Error> {
        Self::new(polygons)
    }
}

impl From<Mask> for Vec<Vec<Point>> {
    fn from(mask: Mask) -> Self {
        mask.polygons
    }
}

/// Polygons separated by `;`, each as `x,y` pairs separated by commas and/or
/// whitespace: `0,0 1,0 0.5,1` is a triangle.
impl fmt::Display for Mask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, polygon) in self.polygons.iter().enumerate() {
            if index > 0 {
                f.write_str(";")?;
            }
            for (index, point) in polygon.iter().enumerate() {
                if index > 0 {
                    f.write_str(" ")?;
                }
                write!(f, "{},{}", point.x, point.y)?;
            }
        }
        Ok(())
    }
}

/// The text [`Mask`]'s `Display` writes; empty or blank text is the empty
/// mask.
impl FromStr for Mask {
    type Err = GeometryError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let polygons = text
            .split(';')
            .filter(|polygon| !polygon.trim().is_empty())
            .map(|polygon| {
                let values = polygon
                    .split(|c: char| c == ',' || c.is_whitespace())
                    .filter(|token| !token.is_empty())
                    .map(|token| {
                        token.parse::<f64>().map_err(|_err| {
                            GeometryError::Parse(format!("{token:?} is not a number"))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let (pairs, rest) = values.as_chunks::<2>();
                if !rest.is_empty() {
                    return Err(GeometryError::Parse(
                        "a mask polygon needs x,y pairs".to_owned(),
                    ));
                }
                Ok(pairs.iter().map(|&pair| Point::from(pair)).collect())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(polygons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(text: &str) -> Mask {
        text.parse().expect("mask")
    }

    #[test]
    fn empty_masks_show_everything() {
        for text in ["", "  ", ";"] {
            let mask = mask(text);
            assert!(mask.is_empty());
            assert!(mask.shows(0.5, 0.5));
            assert!(mask.shows(-3.0, 7.0));
        }
    }

    #[test]
    fn points_may_lie_outside_the_display() {
        // A triangle past the right edge clips the top-right corner.
        let mask = mask("0,0 0.8,0 2,1.2 0,1");
        assert!(mask.shows(0.5, 0.5));
        assert!(!mask.shows(0.95, 0.05));
    }

    #[test]
    fn polygons_show_their_inside_and_holes_hide() {
        // A diamond with a square hole in the middle.
        let mask = mask("0.5,0 1,0.5 0.5,1 0,0.5; 0.4,0.4 0.6,0.4 0.6,0.6 0.4,0.6");
        assert!(mask.shows(0.5, 0.2));
        assert!(mask.shows(0.3, 0.5));
        assert!(!mask.shows(0.5, 0.5), "the hole");
        assert!(!mask.shows(0.1, 0.1), "outside the diamond");
        assert!(!mask.shows(0.9, 0.9));
        assert!(!mask.shows(1.5, 0.5));
    }

    #[test]
    fn crossings_agree_with_shows() {
        let mask = mask("0.1,0.1 0.9,0.2 0.5,0.5 0.8,0.9 0.2,0.7");
        for row in 0..50 {
            let v = (f64::from(row) + 0.5) / 50.0;
            let crossings: Vec<f64> = mask.crossings(v).collect();
            for column in 0..50 {
                let u = (f64::from(column) + 0.5) / 50.0;
                let right = crossings.iter().filter(|x| u < **x).count();
                assert_eq!(right % 2 == 1, mask.shows(u, v), "({u}, {v})");
            }
        }
    }

    #[test]
    fn rays_through_corners_count_once() {
        // The ray at v = 0.5 passes through the triangle's left corner.
        let mask = mask("0,0.5 1,0 1,1");
        assert!(mask.shows(0.5, 0.5));
        assert!(!mask.shows(-0.5, 0.5));
    }

    #[test]
    fn text_and_json_round_trip() {
        let mask = mask("0,0 1,0 1,1; 0.25,0.25 0.75,0.25 0.5,0.5");
        assert_eq!(mask.to_string().parse::<Mask>().expect("text"), mask);
        let json = serde_json::to_string(&mask).expect("json");
        assert_eq!(
            json,
            "[[[0.0,0.0],[1.0,0.0],[1.0,1.0]],[[0.25,0.25],[0.75,0.25],[0.5,0.5]]]"
        );
        assert_eq!(serde_json::from_str::<Mask>(&json).expect("json"), mask);
    }

    #[test]
    fn bad_masks_are_refused() {
        for text in [
            "0,0 1,0",
            "0,0 1,0 1",
            "0,0 1,0 x,1",
            "0,0 1,0 inf,1",
            "0,0 1,0 1e39,1",
            "0,0 1,0 1,-1000001",
        ] {
            assert!(text.parse::<Mask>().is_err(), "{text:?}");
        }
        let many = vec![Point::new(0.0, 0.0); MAX_MASK_POINTS + 1];
        Mask::new(vec![many]).expect_err("too many points");
        serde_json::from_str::<Mask>("[[[0,0],[1,0]]]").expect_err("too few points");
    }
}
