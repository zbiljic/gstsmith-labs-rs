use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::GeometryError;

/// Edges shorter than this fraction of the longest edge, and corners whose
/// turn has a smaller sine, are treated as degenerate.
const DEGENERATE_TOLERANCE: f64 = 1e-9;

/// A 2D point, serialized as `[x, y]`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "[f64; 2]", into = "[f64; 2]")]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    #[must_use]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl From<[f64; 2]> for Point {
    fn from([x, y]: [f64; 2]) -> Self {
        Self { x, y }
    }
}

impl From<Point> for [f64; 2] {
    fn from(point: Point) -> Self {
        [point.x, point.y]
    }
}

fn edge(from: Point, to: Point) -> (f64, f64) {
    (to.x - from.x, to.y - from.y)
}

fn cross(a: (f64, f64), b: (f64, f64)) -> f64 {
    a.0 * b.1 - a.1 * b.0
}

/// Winding of a valid quadrilateral listed top-left, top-right, bottom-right,
/// bottom-left, in coordinates with Y increasing downwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    /// Clockwise on screen: the display shows an unmirrored image.
    Normal,
    /// Counter-clockwise on screen: the display shows a mirrored image.
    Mirrored,
}

/// Four corners in a display's own orientation, serialized as
/// `[[x, y], [x, y], [x, y], [x, y]]` in top-left, top-right, bottom-right,
/// bottom-left order.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "[Point; 4]", into = "[Point; 4]")]
pub struct Quad {
    pub top_left: Point,
    pub top_right: Point,
    pub bottom_right: Point,
    pub bottom_left: Point,
}

impl Quad {
    /// The unit square, which maps a display onto the whole source.
    pub const UNIT: Self = Self::new([
        Point::new(0.0, 0.0),
        Point::new(1.0, 0.0),
        Point::new(1.0, 1.0),
        Point::new(0.0, 1.0),
    ]);

    /// Corners in top-left, top-right, bottom-right, bottom-left order.
    #[must_use]
    pub const fn new([top_left, top_right, bottom_right, bottom_left]: [Point; 4]) -> Self {
        Self {
            top_left,
            top_right,
            bottom_right,
            bottom_left,
        }
    }

    /// Corners in top-left, top-right, bottom-right, bottom-left order.
    #[must_use]
    pub const fn corners(&self) -> [Point; 4] {
        [
            self.top_left,
            self.top_right,
            self.bottom_right,
            self.bottom_left,
        ]
    }

    /// Apply `f` to every corner.
    #[must_use]
    pub fn map(&self, f: impl Fn(Point) -> Point) -> Self {
        Self::new(self.corners().map(f))
    }

    /// Validate that the corners form a strictly convex quadrilateral and
    /// report its winding.
    pub fn orientation(&self) -> Result<Orientation, GeometryError> {
        let corners = self.corners();
        if !corners.iter().all(|corner| corner.is_finite()) {
            return Err(GeometryError::NonFinite);
        }
        let [a, b, c, d] = corners;
        let edges = [edge(a, b), edge(b, c), edge(c, d), edge(d, a)];
        let lengths = edges.map(|(x, y)| x.hypot(y));
        let longest = lengths.iter().copied().fold(0.0, f64::max);
        if !longest.is_finite() {
            return Err(GeometryError::NonFinite);
        }
        if lengths
            .iter()
            .any(|length| *length <= DEGENERATE_TOLERANCE * longest)
        {
            return Err(GeometryError::Degenerate);
        }

        let [e0, e1, e2, e3] = edges;
        let [l0, l1, l2, l3] = lengths;
        // Sine of the turn at each corner after the first edge.
        let turns = [
            cross(e0, e1) / (l0 * l1),
            cross(e1, e2) / (l1 * l2),
            cross(e2, e3) / (l2 * l3),
            cross(e3, e0) / (l3 * l0),
        ];
        if turns.iter().any(|turn| turn.abs() <= DEGENERATE_TOLERANCE) {
            return Err(GeometryError::Degenerate);
        }
        // Four turns of one sign close a convex loop; a bow tie turns twice
        // each way and a concave quadrilateral turns once against the others.
        match turns.iter().filter(|turn| **turn > 0.0).count() {
            4 => Ok(Orientation::Normal),
            0 => Ok(Orientation::Mirrored),
            2 => Err(GeometryError::SelfIntersecting),
            _ => Err(GeometryError::Concave),
        }
    }
}

impl From<[Point; 4]> for Quad {
    fn from(corners: [Point; 4]) -> Self {
        Self::new(corners)
    }
}

impl From<Quad> for [Point; 4] {
    fn from(quad: Quad) -> Self {
        quad.corners()
    }
}

/// Eight comma-separated coordinates: `x,y` for top-left, top-right,
/// bottom-right, and bottom-left. Values round-trip through [`FromStr`].
impl fmt::Display for Quad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, corner) in self.corners().iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            write!(f, "{},{}", corner.x, corner.y)?;
        }
        Ok(())
    }
}

/// Parse eight numbers separated by commas and/or whitespace. Only the syntax
/// is checked; call [`Quad::orientation`] to validate the shape.
impl FromStr for Quad {
    type Err = GeometryError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut values = [0.0; 8];
        let mut count = 0;
        for token in text
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|token| !token.is_empty())
        {
            let value = token
                .parse::<f64>()
                .map_err(|_err| GeometryError::Parse(format!("{token:?} is not a number")))?;
            if let Some(slot) = values.get_mut(count) {
                *slot = value;
            }
            count += 1;
        }
        if count != values.len() {
            return Err(GeometryError::Parse(format!(
                "expected 8 numbers (top-left, top-right, bottom-right, bottom-left x,y pairs), \
                 got {count}"
            )));
        }
        let [x0, y0, x1, y1, x2, y2, x3, y3] = values;
        Ok(Self::new([
            Point::new(x0, y0),
            Point::new(x1, y1),
            Point::new(x2, y2),
            Point::new(x3, y3),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad(values: [[f64; 2]; 4]) -> Quad {
        Quad::new(values.map(Point::from))
    }

    #[test]
    fn unit_square_winds_normally_and_reversal_mirrors() {
        assert_eq!(Quad::UNIT.orientation(), Ok(Orientation::Normal));
        let [a, b, c, d] = Quad::UNIT.corners();
        assert_eq!(
            Quad::new([b, a, d, c]).orientation(),
            Ok(Orientation::Mirrored)
        );
    }

    #[test]
    fn rotated_corner_orders_keep_their_winding() {
        let [a, b, c, d] = Quad::UNIT.corners();
        for corners in [[b, c, d, a], [c, d, a, b], [d, a, b, c]] {
            assert_eq!(Quad::new(corners).orientation(), Ok(Orientation::Normal));
        }
    }

    #[test]
    fn arbitrary_convex_quads_are_accepted() {
        let quad = quad([[80.0, 100.0], [780.0, 90.0], [800.0, 500.0], [90.0, 520.0]]);
        assert_eq!(quad.orientation(), Ok(Orientation::Normal));
    }

    #[test]
    fn invalid_shapes_are_classified() {
        let cases = [
            (
                quad([[0.0, 0.0], [1.0, 1.0], [1.0, 0.0], [0.0, 1.0]]),
                GeometryError::SelfIntersecting,
            ),
            (
                quad([[0.0, 0.0], [1.0, 0.0], [0.2, 0.2], [0.0, 1.0]]),
                GeometryError::Concave,
            ),
            (
                quad([[0.0, 0.0], [0.0, 0.0], [1.0, 1.0], [0.0, 1.0]]),
                GeometryError::Degenerate,
            ),
            (
                quad([[0.0, 0.0], [0.5, 0.0], [1.0, 0.0], [0.0, 1.0]]),
                GeometryError::Degenerate,
            ),
            (
                quad([[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [3.0, 0.0]]),
                GeometryError::Degenerate,
            ),
            (
                quad([[2.0, 2.0], [2.0, 2.0], [2.0, 2.0], [2.0, 2.0]]),
                GeometryError::Degenerate,
            ),
            (
                quad([[0.0, 0.0], [1.0, 0.0], [1.0, 1e-12], [0.0, 1e-12]]),
                GeometryError::Degenerate,
            ),
            (
                quad([[f64::NAN, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]),
                GeometryError::NonFinite,
            ),
            (
                quad([[0.0, 0.0], [f64::INFINITY, 0.0], [1.0, 1.0], [0.0, 1.0]]),
                GeometryError::NonFinite,
            ),
            (
                quad([[-f64::MAX, 0.0], [f64::MAX, 0.0], [1.0, 1.0], [0.0, 1.0]]),
                GeometryError::NonFinite,
            ),
        ];
        for (quad, error) in cases {
            assert_eq!(quad.orientation(), Err(error), "{quad}");
        }
    }

    #[test]
    fn text_round_trips_exactly() {
        let quad = quad([
            [0.1, 1.0 / 3.0],
            [0.75, -0.25],
            [1.5, 1e-7],
            [-0.0, 0.999_999_999_999],
        ]);
        assert_eq!(quad.to_string().parse::<Quad>(), Ok(quad));
        assert_eq!(Quad::UNIT.to_string(), "0,0,1,0,1,1,0,1");
    }

    #[test]
    fn text_accepts_commas_and_whitespace() {
        let expected = quad([[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]);
        for text in [
            "0,0,1,0,1,1,0,1",
            "0 0 1 0 1 1 0 1",
            " 0, 0,  1,0\t1 1 , 0 ,1 ",
            "0.0,0.0,1e0,0,1,1,0,1",
        ] {
            assert_eq!(text.parse::<Quad>(), Ok(expected), "{text:?}");
        }
    }

    #[test]
    fn malformed_text_is_rejected() {
        for text in ["", "0,0,1,0,1,1,0", "0,0,1,0,1,1,0,1,2", "0,0,1,0,1,1,0,x"] {
            assert!(
                matches!(text.parse::<Quad>(), Err(GeometryError::Parse(_))),
                "{text:?}"
            );
        }
        let parsed = "nan,0,1,0,1,1,0,1"
            .parse::<Quad>()
            .expect("syntax is valid");
        assert_eq!(parsed.orientation(), Err(GeometryError::NonFinite));
    }

    #[test]
    fn json_uses_nested_coordinate_arrays() {
        let quad = quad([[80.0, 100.0], [780.5, 90.0], [800.0, 500.0], [90.0, 520.0]]);
        let json = serde_json::to_string(&quad).expect("serialize");
        assert_eq!(
            json,
            "[[80.0,100.0],[780.5,90.0],[800.0,500.0],[90.0,520.0]]"
        );
        assert_eq!(serde_json::from_str::<Quad>(&json).expect("parse"), quad);
        assert_eq!(
            serde_json::from_str::<Quad>("[[80,100],[780.5,90],[800,500],[90,520]]")
                .expect("integers parse as coordinates"),
            quad
        );
        let _missing_corner =
            serde_json::from_str::<Quad>("[[0,0],[1,0],[1,1]]").expect_err("three corners");
    }
}
