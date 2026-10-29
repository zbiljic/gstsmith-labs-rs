use std::fmt;

/// A quadrilateral or transform that cannot describe a valid display mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeometryError {
    /// A coordinate or matrix entry is NaN or infinite.
    NonFinite,
    /// Corners repeat or three consecutive corners are collinear.
    Degenerate,
    /// Opposite edges cross, as in a bow tie.
    SelfIntersecting,
    /// The quadrilateral is simple but not convex, so no homography maps a
    /// rectangle onto it.
    Concave,
    /// The matrix has no inverse.
    Singular,
    /// The matrix is too close to singular for reliable results.
    IllConditioned,
    /// A textual quadrilateral is malformed.
    Parse(String),
    /// Fewer than four point correspondences, or too few distinct
    /// non-collinear points, to determine a transform.
    Underdetermined,
}

impl fmt::Display for GeometryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite => f.write_str("coordinates must be finite numbers"),
            Self::Degenerate => {
                f.write_str("quadrilateral is degenerate (repeated or collinear corners)")
            }
            Self::SelfIntersecting => f.write_str("quadrilateral edges intersect each other"),
            Self::Concave => f.write_str("quadrilateral is not convex"),
            Self::Singular => f.write_str("transform is singular"),
            Self::IllConditioned => f.write_str("transform is numerically ill-conditioned"),
            Self::Parse(reason) => write!(f, "malformed quadrilateral: {reason}"),
            Self::Underdetermined => f.write_str(
                "at least four distinct, non-collinear point correspondences are required",
            ),
        }
    }
}

impl std::error::Error for GeometryError {}

/// A layout file that cannot be used.
#[derive(Debug)]
#[non_exhaustive]
pub enum LayoutError {
    /// The JSON is malformed or does not match the schema.
    Json(serde_json::Error),
    /// The `version` field names a schema this crate does not understand.
    UnsupportedVersion(u64),
    /// A width or height is zero or exceeds [`MAX_DIMENSION`](crate::MAX_DIMENSION).
    InvalidDimension { field: String, value: u32 },
    /// The layout contains no screens.
    NoScreens,
    /// A screen ID is empty, too long, or contains unsupported characters.
    InvalidId(String),
    /// Two screens share an ID.
    DuplicateId(String),
    /// A screen's corners wind counter-clockwise, which would mirror its image.
    Mirrored(String),
    /// A screen's corners do not form a usable quadrilateral.
    Geometry { id: String, error: GeometryError },
    /// The source frame aspect ratio differs from the canvas aspect ratio.
    SourceAspect {
        source: (u32, u32),
        canvas: (u32, u32),
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(err) => write!(f, "invalid layout JSON: {err}"),
            Self::UnsupportedVersion(version) => write!(
                f,
                "unsupported layout version {version}; expected {}",
                crate::LAYOUT_VERSION
            ),
            Self::InvalidDimension { field, value } => write!(
                f,
                "{field} must be between 1 and {}, got {value}",
                crate::MAX_DIMENSION
            ),
            Self::NoScreens => f.write_str("layout must contain at least one screen"),
            Self::InvalidId(id) => write!(
                f,
                "screen ID {id:?} must be 1 to {} ASCII letters, digits, '-', '_', or '.'",
                crate::MAX_ID_LENGTH
            ),
            Self::DuplicateId(id) => write!(f, "screen ID {id:?} is used more than once"),
            Self::Mirrored(id) => write!(
                f,
                "screen {id:?} corners wind counter-clockwise; list them top-left, top-right, \
                 bottom-right, bottom-left in the display's own orientation"
            ),
            Self::Geometry { id, error } => write!(f, "screen {id:?}: {error}"),
            Self::SourceAspect { source, canvas } => write!(
                f,
                "source {}x{} does not match the {}x{} canvas aspect ratio",
                source.0, source.1, canvas.0, canvas.1
            ),
        }
    }
}

impl std::error::Error for LayoutError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(err) => Some(err),
            Self::Geometry { error, .. } => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for LayoutError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}
