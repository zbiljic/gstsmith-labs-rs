use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct GlWallWarp(ObjectSubclass<imp::GlWallWarp>)
        @extends gst::Bin, gst::Element, gst::Object,
        @implements gst::ChildProxy;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "glwallwarp",
        gst::Rank::NONE,
        GlWallWarp::static_type(),
    )
}
