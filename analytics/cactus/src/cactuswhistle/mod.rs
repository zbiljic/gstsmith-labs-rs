//! `cactuswhistle`: speech to text with Cactus Whistle on rolling windows.
//!
//! The element takes 16 kHz mono F32LE. Whenever `chunk-duration` of new
//! audio has arrived, it transcribes a window of `context-duration` of
//! already committed audio plus the new audio, with word times. Each call is
//! stateless, so several elements can share the engine. The stitcher
//! commits each word once, from the window that heard it with enough audio
//! on both sides, and groups the words into `text/x-raw,format=utf8` cues
//! on the input's timeline. With `chunk-duration=0` the whole recording is
//! transcribed at EOS.

use gst::glib;
use gst::prelude::*;

mod imp;
mod stitch;
mod transcriber;

glib::wrapper! {
    pub struct CactusWhistle(ObjectSubclass<imp::CactusWhistle>)
        @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "cactuswhistle",
        gst::Rank::NONE,
        CactusWhistle::static_type(),
    )
}
