//! Browser playback demo for irregular video walls.
//!
//! A small HTTP server ([`http`]) runs the whole workflow from one control
//! page ([`app`]): displays register by opening a URL with their ID, show
//! calibration patterns, a photo of the wall is uploaded and calibrated with
//! `wallcalibrate`, and a layout drives one `wallwarp` branch per display
//! whose output streams to that display's browser through `webrtcsink`
//! ([`player`]). It plays a test pattern, a video, a live stream or a web
//! page's video, or a camera or screen of the machine ([`source`],
//! [`capture`]). While measuring, every output carries its frame
//! number ([`framecode`]) and the browsers report when they present each
//! frame ([`drift`]); aligning delays the early displays to match ([`sync`]).
//! An optional token keeps the controls to those who know it ([`auth`]).
//!
//! None of this is part of the `wallwarp` element: the server, the pages,
//! and the signalling choices are one way to orchestrate it.

pub mod app;
pub mod auth;
pub mod capture;
pub mod drift;
pub mod framecode;
pub mod http;
pub mod player;
pub mod source;
pub mod sync;

/// Register the `wallwarp` and `glwallwarp` elements with this process's
/// `GStreamer`.
pub fn register_wallwarp() -> Result<(), gst::glib::BoolError> {
    gstwallwarp::plugin_register_static()
}
