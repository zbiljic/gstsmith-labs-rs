//! Photo-based calibration of irregular video walls.
//!
//! Every display shows `AprilTag` `tag36h11` markers at known positions, listed in
//! a [`manifest`]. One photo of the wall is searched for markers
//! ([`detect`]), each display's corners are fitted from its markers
//! ([`calibration`]), and the photo-space result becomes a `wall-geometry`
//! layout ([`canvas`]) in one of two explicit modes:
//!
//! - *viewpoint*: the canvas is the photo itself. Content looks right from the
//!   camera position, whatever the screens' arrangement in space.
//! - *planar*: coplanar screens are rectified using a reference screen of known
//!   aspect ratio, removing the camera's perspective. Physical units need a
//!   measured reference size; a single photo cannot provide them.
//!
//! Coordinates in photos use pixel `(i, j)` covering `[i, i + 1) × [j, j + 1)`,
//! as in `wall-geometry`.

pub mod calibration;
pub mod canvas;
pub mod detect;
pub mod manifest;
pub mod pattern;
pub mod preview;
pub mod synth;
pub mod tag;
