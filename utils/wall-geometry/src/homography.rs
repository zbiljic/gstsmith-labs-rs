use crate::{GeometryError, Point, Quad};

/// Smallest accepted `|det(H)| / ‖H‖_F³`. The ratio ignores the arbitrary
/// projective scale of `H` and falls towards zero as `H` approaches rank two.
const MIN_RELATIVE_DETERMINANT: f64 = 1e-12;

type Matrix = [[f64; 3]; 3];

/// A non-singular 3×3 projective transform of the plane.
///
/// A point `(x, y)` maps to `((a·x + b·y + c) / w, (d·x + e·y + f) / w)` with
/// `w = g·x + h·y + i`, for the row-major matrix `[[a, b, c], [d, e, f], [g, h, i]]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Homography {
    matrix: Matrix,
}

fn frobenius(matrix: &Matrix) -> f64 {
    matrix
        .iter()
        .flatten()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt()
}

#[expect(
    clippy::many_single_char_names,
    reason = "entries follow the row-major a to i naming of the type documentation"
)]
fn determinant(matrix: &Matrix) -> f64 {
    let [[a, b, c], [d, e, f], [g, h, i]] = *matrix;
    a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g)
}

#[expect(
    clippy::many_single_char_names,
    reason = "entries follow the row-major a to i naming of the type documentation"
)]
fn multiply(left: &Matrix, right: &Matrix) -> Matrix {
    let [[a, b, c], [d, e, f], [g, h, i]] = *left;
    let [[j, k, l], [m, n, o], [p, q, r]] = *right;
    [
        [
            a * j + b * m + c * p,
            a * k + b * n + c * q,
            a * l + b * o + c * r,
        ],
        [
            d * j + e * m + f * p,
            d * k + e * n + f * q,
            d * l + e * o + f * r,
        ],
        [
            g * j + h * m + i * p,
            g * k + h * n + i * q,
            g * l + h * o + i * r,
        ],
    ]
}

/// A similarity moving the centroid of `points` to the origin and their mean
/// distance from it to √2 (Hartley normalization).
fn normalization(points: impl Iterator<Item = Point> + Clone) -> Result<Matrix, GeometryError> {
    let (mut count, mut sum_x, mut sum_y) = (0.0, 0.0, 0.0);
    for point in points.clone() {
        count += 1.0;
        sum_x += point.x;
        sum_y += point.y;
    }
    let (center_x, center_y) = (sum_x / count, sum_y / count);
    let mean = points
        .map(|point| (point.x - center_x).hypot(point.y - center_y))
        .sum::<f64>()
        / count;
    if !mean.is_finite() {
        return Err(GeometryError::NonFinite);
    }
    if mean < f64::MIN_POSITIVE {
        return Err(GeometryError::Underdetermined);
    }
    let scale = std::f64::consts::SQRT_2 / mean;
    Ok([
        [scale, 0.0, -scale * center_x],
        [0.0, scale, -scale * center_y],
        [0.0, 0.0, 1.0],
    ])
}

/// Solve `matrix · x = rhs` by Gaussian elimination with partial pivoting, or
/// return `None` when the system is singular to working precision.
#[expect(
    clippy::indexing_slicing,
    clippy::needless_range_loop,
    reason = "elimination updates one row from another, with indices bounded by the array length N"
)]
fn solve<const N: usize>(mut matrix: [[f64; N]; N], mut rhs: [f64; N]) -> Option<[f64; N]> {
    let largest = matrix
        .iter()
        .flatten()
        .fold(0.0_f64, |max, value| max.max(value.abs()));
    if !largest.is_finite() || largest < f64::MIN_POSITIVE {
        return None;
    }
    for column in 0..N {
        let pivot = (column..N)
            .max_by(|&a, &b| matrix[a][column].abs().total_cmp(&matrix[b][column].abs()))?;
        if matrix[pivot][column].abs() <= largest * 1e-12 {
            return None;
        }
        matrix.swap(column, pivot);
        rhs.swap(column, pivot);
        for row in column + 1..N {
            let factor = matrix[row][column] / matrix[column][column];
            for entry in column..N {
                matrix[row][entry] -= factor * matrix[column][entry];
            }
            rhs[row] -= factor * rhs[column];
        }
    }
    let mut solution = [0.0; N];
    for row in (0..N).rev() {
        let known: f64 = (row + 1..N)
            .map(|entry| matrix[row][entry] * solution[entry])
            .sum();
        solution[row] = (rhs[row] - known) / matrix[row][row];
    }
    solution
        .iter()
        .all(|value| value.is_finite())
        .then_some(solution)
}

#[expect(
    clippy::many_single_char_names,
    reason = "entries follow the row-major a to i naming of the type documentation"
)]
fn apply(matrix: &Matrix, point: Point) -> (f64, f64) {
    let [[a, b, c], [d, e, f], _] = *matrix;
    (a * point.x + b * point.y + c, d * point.x + e * point.y + f)
}

/// Rescale so the bottom-right entry is one when it is usable, keeping
/// entries near unit magnitude without changing the mapping.
fn normalize(matrix: Matrix) -> Matrix {
    let [_, _, [_, _, corner]] = matrix;
    let norm = frobenius(&matrix);
    let scale = if corner.abs() > MIN_RELATIVE_DETERMINANT * norm {
        corner
    } else {
        norm
    };
    matrix.map(|row| row.map(|value| value / scale))
}

impl Homography {
    pub const IDENTITY: Self = Self {
        matrix: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    };

    /// Accept a row-major matrix that is finite and well conditioned.
    pub fn from_matrix(matrix: Matrix) -> Result<Self, GeometryError> {
        if !matrix.iter().flatten().all(|value| value.is_finite()) {
            return Err(GeometryError::NonFinite);
        }
        let norm = frobenius(&matrix);
        if !norm.is_finite() {
            return Err(GeometryError::NonFinite);
        }
        let determinant = determinant(&matrix);
        if !determinant.is_finite() {
            return Err(GeometryError::NonFinite);
        }
        if norm < f64::MIN_POSITIVE || determinant.abs() < f64::MIN_POSITIVE {
            return Err(GeometryError::Singular);
        }
        // Divide in steps so large but valid matrices do not overflow the cube.
        let relative = determinant.abs() / norm / norm / norm;
        if relative < MIN_RELATIVE_DETERMINANT {
            return Err(GeometryError::IllConditioned);
        }
        Ok(Self { matrix })
    }

    /// The row-major matrix.
    #[must_use]
    pub const fn matrix(&self) -> Matrix {
        self.matrix
    }

    /// Map the unit square corners `(0, 0)`, `(1, 0)`, `(1, 1)`, `(0, 1)` onto
    /// the quadrilateral's top-left, top-right, bottom-right, and bottom-left.
    ///
    /// The quadrilateral must be strictly convex; either winding is accepted.
    /// Axis-aligned and parallelogram inputs produce exact affine matrices.
    pub fn unit_square_to_quad(quad: &Quad) -> Result<Self, GeometryError> {
        quad.orientation()?;
        let [p0, p1, p2, p3] = quad.corners();
        // Heckbert, "Fundamentals of Texture Mapping and Image Warping" (1989),
        // square-to-quadrilateral mapping. Parallelograms give g = h = 0.
        let sx = p0.x - p1.x + p2.x - p3.x;
        let sy = p0.y - p1.y + p2.y - p3.y;
        let (dx1, dy1) = (p1.x - p2.x, p1.y - p2.y);
        let (dx2, dy2) = (p3.x - p2.x, p3.y - p2.y);
        let denominator = dx1 * dy2 - dx2 * dy1;
        if denominator.abs() < f64::MIN_POSITIVE {
            return Err(GeometryError::Degenerate);
        }
        let g = (sx * dy2 - dx2 * sy) / denominator;
        let h = (dx1 * sy - sx * dy1) / denominator;
        // A strictly convex image keeps the whole square in front of the
        // projection: w = g·u + h·v + 1 is positive at every corner.
        if ![1.0, 1.0 + g, 1.0 + g + h, 1.0 + h]
            .iter()
            .all(|w| *w > 0.0)
        {
            return Err(GeometryError::IllConditioned);
        }
        Self::from_matrix([
            [p1.x - p0.x + g * p1.x, p3.x - p0.x + h * p3.x, p0.x],
            [p1.y - p0.y + g * p1.y, p3.y - p0.y + h * p3.y, p0.y],
            [g, h, 1.0],
        ])
    }

    /// The transform taking each corner of `from` to the matching corner of `to`.
    pub fn from_correspondences(from: &Quad, to: &Quad) -> Result<Self, GeometryError> {
        Self::unit_square_to_quad(from)?
            .inverse()?
            .then(&Self::unit_square_to_quad(to)?)
    }

    /// The least-squares transform taking each `from` point to its `to` point,
    /// for four or more correspondences.
    ///
    /// This is the normalized direct linear transform: both point sets are
    /// centered and scaled (Hartley normalization) before minimizing the
    /// algebraic error, which keeps the solve well conditioned in pixel
    /// coordinates. Four exact correspondences reproduce
    /// [`from_correspondences`](Self::from_correspondences).
    #[expect(
        clippy::many_single_char_names,
        reason = "entries follow the row-major a to i naming of the type documentation"
    )]
    pub fn fit(pairs: &[(Point, Point)]) -> Result<Self, GeometryError> {
        if pairs.len() < 4 {
            return Err(GeometryError::Underdetermined);
        }
        if !pairs
            .iter()
            .all(|(from, to)| from.is_finite() && to.is_finite())
        {
            return Err(GeometryError::NonFinite);
        }
        let from_normalization = normalization(pairs.iter().map(|pair| pair.0))?;
        let to_normalization = normalization(pairs.iter().map(|pair| pair.1))?;
        // With both sets centered, the source centroid maps to a finite point,
        // so the bottom-right entry can be fixed at one, leaving eight unknowns.
        let mut normal = [[0.0; 8]; 8];
        let mut rhs = [0.0; 8];
        for (from, to) in pairs {
            let (x, y) = apply(&from_normalization, *from);
            let (u, v) = apply(&to_normalization, *to);
            for (row, target) in [
                ([x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y], u),
                ([0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y], v),
            ] {
                for ((normal_row, rhs_entry), a) in normal.iter_mut().zip(&mut rhs).zip(row) {
                    for (entry, b) in normal_row.iter_mut().zip(row) {
                        *entry += a * b;
                    }
                    *rhs_entry += a * target;
                }
            }
        }
        let [a, b, c, d, e, f, g, h] = solve(normal, rhs).ok_or(GeometryError::Underdetermined)?;
        let [[scale, _, offset_x], [_, _, offset_y], _] = to_normalization;
        let to_denormalization = [
            [1.0 / scale, 0.0, -offset_x / scale],
            [0.0, 1.0 / scale, -offset_y / scale],
            [0.0, 0.0, 1.0],
        ];
        let normalized = [[a, b, c], [d, e, f], [g, h, 1.0]];
        Self::from_matrix(normalize(multiply(
            &multiply(&to_denormalization, &normalized),
            &from_normalization,
        )))
    }

    /// The inverse transform.
    #[expect(
        clippy::many_single_char_names,
        reason = "entries follow the row-major a to i naming of the type documentation"
    )]
    pub fn inverse(&self) -> Result<Self, GeometryError> {
        let [[a, b, c], [d, e, f], [g, h, i]] = self.matrix;
        let determinant = determinant(&self.matrix);
        if determinant.abs() < f64::MIN_POSITIVE {
            return Err(GeometryError::Singular);
        }
        let adjugate = [
            [e * i - f * h, c * h - b * i, b * f - c * e],
            [f * g - d * i, a * i - c * g, c * d - a * f],
            [d * h - e * g, b * g - a * h, a * e - b * d],
        ];
        Self::from_matrix(normalize(
            adjugate.map(|row| row.map(|value| value / determinant)),
        ))
    }

    /// The transform applying `self` first and then `next`.
    pub fn then(&self, next: &Self) -> Result<Self, GeometryError> {
        Self::from_matrix(normalize(multiply(&next.matrix, &self.matrix)))
    }

    /// Map a point, or `None` when it maps to infinity.
    #[expect(
        clippy::many_single_char_names,
        reason = "entries follow the row-major a to i naming of the type documentation"
    )]
    #[must_use]
    pub fn map(&self, point: Point) -> Option<Point> {
        let [[a, b, c], [d, e, f], [g, h, i]] = self.matrix;
        let w = g * point.x + h * point.y + i;
        if w.abs() < f64::MIN_POSITIVE || !w.is_finite() {
            return None;
        }
        let mapped = Point::new(
            (a * point.x + b * point.y + c) / w,
            (d * point.x + e * point.y + f) / w,
        );
        mapped.is_finite().then_some(mapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOLERANCE: f64 = 1e-9;

    type Rotation = (Quad, fn(Point) -> Point);

    fn quad(values: [[f64; 2]; 4]) -> Quad {
        Quad::new(values.map(Point::from))
    }

    fn map(homography: &Homography, x: f64, y: f64) -> Point {
        homography.map(Point::new(x, y)).expect("finite mapping")
    }

    fn assert_near(actual: Point, expected: Point) {
        assert!(
            (actual.x - expected.x).abs() <= TOLERANCE
                && (actual.y - expected.y).abs() <= TOLERANCE,
            "{actual:?} != {expected:?}"
        );
    }

    fn grid() -> impl Iterator<Item = Point> {
        (0..=10).flat_map(|y| {
            (0..=10).map(move |x| Point::new(f64::from(x) / 10.0, f64::from(y) / 10.0))
        })
    }

    /// Intersection of the lines through `a`-`b` and `c`-`d`.
    fn intersection(start: Point, end: Point, other_start: Point, other_end: Point) -> Point {
        let along = (end.x - start.x, end.y - start.y);
        let other = (other_end.x - other_start.x, other_end.y - other_start.y);
        let offset = (other_start.x - start.x, other_start.y - start.y);
        let t = (offset.0 * other.1 - offset.1 * other.0) / (along.0 * other.1 - along.1 * other.0);
        Point::new(start.x + t * along.0, start.y + t * along.1)
    }

    fn arbitrary_quads() -> [Quad; 4] {
        [
            quad([[80.0, 100.0], [780.0, 90.0], [800.0, 500.0], [90.0, 520.0]]),
            quad([
                [1370.0, 60.0],
                [1330.0, 920.0],
                [1060.0, 900.0],
                [1100.0, 30.0],
            ]),
            quad([[0.1, 0.2], [0.9, 0.05], [0.7, 0.95], [0.2, 0.6]]),
            quad([[0.25, 0.0], [0.75, 0.0], [1.0, 1.0], [0.0, 1.0]]),
        ]
    }

    #[test]
    fn unit_square_maps_to_the_exact_identity() {
        let homography = Homography::unit_square_to_quad(&Quad::UNIT).expect("identity");
        for point in grid() {
            assert_eq!(homography.map(point), Some(point));
        }
    }

    #[test]
    fn rectangles_crop_affinely() {
        let crop = quad([[0.25, 0.5], [0.75, 0.5], [0.75, 1.0], [0.25, 1.0]]);
        let homography = Homography::unit_square_to_quad(&crop).expect("crop");
        let [_, _, [g, h, _]] = homography.matrix();
        assert!(g.abs() < f64::MIN_POSITIVE && h.abs() < f64::MIN_POSITIVE);
        for point in grid() {
            assert_eq!(
                homography.map(point),
                Some(Point::new(0.25 + 0.5 * point.x, 0.5 + 0.5 * point.y))
            );
        }
    }

    #[test]
    fn quarter_turns_rotate_display_coordinates() {
        let [a, b, c, d] = Quad::UNIT.corners();
        let rotations: [Rotation; 3] = [
            // Display rotated 90° clockwise: its top-left is the source's top-right.
            (Quad::new([b, c, d, a]), |p| Point::new(1.0 - p.y, p.x)),
            (Quad::new([c, d, a, b]), |p| {
                Point::new(1.0 - p.x, 1.0 - p.y)
            }),
            (Quad::new([d, a, b, c]), |p| Point::new(p.y, 1.0 - p.x)),
        ];
        for (quad, expected) in rotations {
            let homography = Homography::unit_square_to_quad(&quad).expect("rotation");
            for point in grid() {
                assert_eq!(homography.map(point), Some(expected(point)), "{quad}");
            }
        }
    }

    #[test]
    fn corners_map_exactly_and_centers_meet_the_diagonals() {
        for quad in arbitrary_quads() {
            let homography = Homography::unit_square_to_quad(&quad).expect("convex quad");
            for (corner, unit) in quad.corners().into_iter().zip(Quad::UNIT.corners()) {
                assert_near(homography.map(unit).expect("corner"), corner);
            }
            // Projective maps preserve incidence: the square's center is where
            // its diagonals cross, so it maps to where the quad's diagonals cross.
            let diagonals = intersection(
                quad.top_left,
                quad.bottom_right,
                quad.top_right,
                quad.bottom_left,
            );
            assert_near(map(&homography, 0.5, 0.5), diagonals);
        }
    }

    #[test]
    fn perspective_follows_vanishing_points() {
        // A trapezoid narrowing upwards: the left and right edges meet at a
        // vanishing point above it.
        let trapezoid = quad([[0.25, 0.0], [0.75, 0.0], [1.0, 1.0], [0.0, 1.0]]);
        let homography = Homography::unit_square_to_quad(&trapezoid).expect("trapezoid");
        let center = map(&homography, 0.5, 0.5);
        // The diagonals cross a third of the way down, not halfway as an
        // affine or bilinear map would place the center.
        assert_near(center, Point::new(0.5, 1.0 / 3.0));
        let [_, _, [g, h, _]] = homography.matrix();
        assert!(g.abs() > 0.1 || h.abs() > 0.1, "{homography:?}");

        // The vertical center line passes through the vanishing point, so the
        // image of (0.5, 0) lies on the line through it and the center.
        let skew = quad([[0.1, 0.2], [0.9, 0.05], [0.7, 0.95], [0.2, 0.6]]);
        let homography = Homography::unit_square_to_quad(&skew).expect("skew");
        let vanishing = intersection(
            skew.top_left,
            skew.bottom_left,
            skew.top_right,
            skew.bottom_right,
        );
        let center = map(&homography, 0.5, 0.5);
        for v in [0.0, 0.25, 1.0] {
            let point = map(&homography, 0.5, v);
            let collinear = (center.x - vanishing.x) * (point.y - vanishing.y)
                - (center.y - vanishing.y) * (point.x - vanishing.x);
            assert!(collinear.abs() < TOLERANCE, "{point:?} off the center line");
        }
    }

    #[test]
    fn inverses_round_trip() {
        for quad in arbitrary_quads() {
            let homography = Homography::unit_square_to_quad(&quad).expect("convex quad");
            let inverse = homography.inverse().expect("invertible");
            let identity = homography.then(&inverse).expect("composable");
            for point in grid() {
                assert_near(
                    inverse
                        .map(map(&homography, point.x, point.y))
                        .expect("inverse"),
                    point,
                );
                assert_near(identity.map(point).expect("identity"), point);
            }
            for corner in quad.corners() {
                assert!(inverse.map(corner).is_some());
            }
        }
    }

    #[test]
    fn correspondences_map_corner_to_corner() {
        let [from, .., to] = arbitrary_quads();
        let homography = Homography::from_correspondences(&from, &to).expect("correspondence");
        for (source, target) in from.corners().into_iter().zip(to.corners()) {
            let mapped = homography.map(source).expect("finite");
            assert!(
                (mapped.x - target.x).abs() < 1e-6 && (mapped.y - target.y).abs() < 1e-6,
                "{mapped:?} != {target:?}"
            );
        }
    }

    #[test]
    fn fits_reproduce_exact_correspondences() {
        for quad in arbitrary_quads() {
            let exact = Homography::unit_square_to_quad(&quad).expect("convex quad");
            let pairs: Vec<_> = Quad::UNIT
                .corners()
                .into_iter()
                .zip(quad.corners())
                .collect();
            let fitted = Homography::fit(&pairs).expect("four correspondences");
            for point in grid() {
                let (expected, actual) = (
                    map(&exact, point.x, point.y),
                    map(&fitted, point.x, point.y),
                );
                let scale = 1.0 + expected.x.abs().max(expected.y.abs());
                assert!(
                    (expected.x - actual.x).abs() < 1e-9 * scale
                        && (expected.y - actual.y).abs() < 1e-9 * scale,
                    "{actual:?} != {expected:?}"
                );
            }
        }
    }

    #[test]
    fn fits_average_noisy_correspondences() {
        // Sixteen marker corners on a display, seen in perspective with
        // deterministic ±0.5 px noise.
        let truth = Homography::unit_square_to_quad(&arbitrary_quads()[1]).expect("truth");
        let mut noise = [0.31, -0.42, 0.5, -0.13, 0.07, -0.5, 0.26, 0.44]
            .into_iter()
            .cycle();
        let mut pairs = Vec::new();
        for (u0, v0) in [(0.05, 0.05), (0.75, 0.05), (0.75, 0.7), (0.05, 0.7)] {
            for (du, dv) in [(0.0, 0.0), (0.2, 0.0), (0.2, 0.25), (0.0, 0.25)] {
                let local = Point::new(u0 + du, v0 + dv);
                let photo = map(&truth, local.x, local.y);
                let (nx, ny) = (noise.next().expect("noise"), noise.next().expect("noise"));
                pairs.push((local, Point::new(photo.x + nx, photo.y + ny)));
            }
        }
        let fitted = Homography::fit(&pairs).expect("sixteen correspondences");
        for corner in Quad::UNIT.corners() {
            let (expected, actual) = (
                map(&truth, corner.x, corner.y),
                map(&fitted, corner.x, corner.y),
            );
            let error = (expected.x - actual.x).hypot(expected.y - actual.y);
            assert!(error < 1.0, "display corner off by {error} px");
        }
    }

    #[test]
    fn fits_need_four_spread_points() {
        let point = |x, y| (Point::new(x, y), Point::new(2.0 * x, y + 1.0));
        let cases = [
            (
                vec![point(0.0, 0.0), point(1.0, 0.0), point(1.0, 1.0)],
                GeometryError::Underdetermined,
            ),
            (
                vec![
                    point(0.0, 0.0),
                    point(1.0, 1.0),
                    point(2.0, 2.0),
                    point(3.0, 3.0),
                    point(4.0, 4.0),
                ],
                GeometryError::Underdetermined,
            ),
            (vec![point(1.0, 1.0); 6], GeometryError::Underdetermined),
            (
                vec![
                    point(0.0, 0.0),
                    point(1.0, 0.0),
                    point(1.0, 1.0),
                    point(f64::NAN, 1.0),
                ],
                GeometryError::NonFinite,
            ),
        ];
        for (pairs, error) in cases {
            assert_eq!(Homography::fit(&pairs), Err(error), "{pairs:?}");
        }
    }

    #[test]
    fn invalid_matrices_are_rejected() {
        let cases = [
            ([[0.0; 3]; 3], GeometryError::Singular),
            (
                [[1.0, 2.0, 3.0], [2.0, 4.0, 6.0], [0.0, 0.0, 1.0]],
                GeometryError::Singular,
            ),
            (
                [[1.0, 0.0, 0.0], [0.0, 1e-13, 0.0], [0.0, 0.0, 1.0]],
                GeometryError::IllConditioned,
            ),
            (
                [[f64::NAN, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                GeometryError::NonFinite,
            ),
            (
                [[f64::MAX, f64::MAX, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                GeometryError::NonFinite,
            ),
        ];
        for (matrix, error) in cases {
            assert_eq!(Homography::from_matrix(matrix), Err(error), "{matrix:?}");
        }
        assert_eq!(
            Homography::from_matrix(Homography::IDENTITY.matrix()),
            Ok(Homography::IDENTITY)
        );
    }

    #[test]
    fn invalid_quads_have_no_homography() {
        let bow_tie = quad([[0.0, 0.0], [1.0, 1.0], [1.0, 0.0], [0.0, 1.0]]);
        let collinear = quad([[0.0, 0.0], [0.5, 0.0], [1.0, 0.0], [0.0, 1.0]]);
        assert_eq!(
            Homography::unit_square_to_quad(&bow_tie),
            Err(GeometryError::SelfIntersecting)
        );
        assert_eq!(
            Homography::unit_square_to_quad(&collinear),
            Err(GeometryError::Degenerate)
        );
        assert_eq!(
            Homography::from_correspondences(&Quad::UNIT, &bow_tie),
            Err(GeometryError::SelfIntersecting)
        );
    }

    #[test]
    fn points_at_infinity_do_not_map() {
        let homography =
            Homography::from_matrix([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 0.0, 1.0]])
                .expect("projective");
        assert_eq!(homography.map(Point::new(-1.0, 0.5)), None);
        assert_eq!(homography.map(Point::new(f64::NAN, 0.5)), None);
        assert_eq!(
            homography.map(Point::new(1.0, 0.5)),
            Some(Point::new(0.5, 0.25))
        );
    }
}
