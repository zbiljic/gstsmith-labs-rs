//! Speaker diarization for transcription cues.
//!
//! - `diarizer`: the element that labels timed text cues with speakers.
//! - [`speakers`]: online clustering of cue embeddings, numbers, merges and
//!   labels.
//! - [`embedder`], [`fbank`], [`profile`]: the speaker models, a
//!   Kaldi-compatible filterbank front-end and the qualified model profiles.

use gst::glib;

mod diarizer;
pub mod embedder;
pub mod fbank;
pub mod profile;
pub mod speakers;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    diarizer::register(plugin)
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
