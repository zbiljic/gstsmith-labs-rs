//! Speaker diarization for transcription cues.
//!
//! This first part holds the speaker models: a Kaldi-compatible filterbank
//! front-end, the qualified model profiles and an ONNX Runtime embedder.
//! The `diarizer` element that assigns speakers to cues builds on them.

use gst::glib;

pub mod embedder;
pub mod fbank;
pub mod profile;

#[expect(
    clippy::unnecessary_wraps,
    reason = "the signature plugin_define! expects"
)]
fn plugin_init(_plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    Ok(())
}

gst::plugin_define!(
    diarization,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "Apache-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
