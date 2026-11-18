//! `diarizer`: speakers for timed text cues.
//!
//! The element takes mono audio on `audio_sink` and timed text cues, such as
//! `cactuswhistle` output, on `text_sink`. It keeps recent audio by running
//! time. For each cue it embeds the cue's audio with a speaker model, assigns
//! the cue to a session speaker by online clustering, and pushes the cue on
//! `src` with an `x-speaker` analytics document, an optional label prefix,
//! and a `rstranscribe/speaker-change` event whenever the speaker changes.

use gst::glib;
use gst::prelude::*;

mod imp;
mod ring;

glib::wrapper! {
    pub struct Diarizer(ObjectSubclass<imp::Diarizer>)
        @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "diarizer",
        gst::Rank::NONE,
        Diarizer::static_type(),
    )
}
