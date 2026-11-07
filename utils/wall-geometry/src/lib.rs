//! Video wall layout configuration and perspective mapping.
//!
//! A source video is mapped onto a virtual 2D canvas. Each physical display
//! shows the canvas region inside one quadrilateral. Canvas coordinates are in
//! pixels with the origin at the top-left and Y increasing downwards.
//!
//! A display's corners are listed in its *own* orientation: top-left,
//! top-right, bottom-right, bottom-left are where the display's output pixel
//! `(0, 0)`, the end of its first row, its last pixel, and the start of its
//! last row appear on the canvas. A display mounted rotated by 90° clockwise
//! therefore has its top-left corner near the canvas top-right. Listed in this
//! order, an unmirrored display winds clockwise on screen (positive signed area
//! with Y down); the opposite winding shows a mirrored image.
//!
//! Display-local coordinates `(u, v)` are normalized to `[0, 1]²` across the
//! display output. Source coordinates are normalized to `[0, 1]²` across the
//! source frame, which must have the canvas aspect ratio.
//!
//! The crate has no `GStreamer` or transport dependency.

mod error;
mod homography;
mod layout;
mod mask;
mod quad;

pub use error::{GeometryError, LayoutError};
pub use homography::Homography;
pub use layout::{
    LAYOUT_VERSION, Layout, MAX_DIMENSION, MAX_ID_LENGTH, Screen, Size, is_valid_screen_id,
};
pub use mask::{MAX_MASK_COORDINATE, MAX_MASK_POINTS, Mask};
pub use quad::{Orientation, Point, Quad};
