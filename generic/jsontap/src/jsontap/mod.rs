use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct JsonTap(ObjectSubclass<imp::JsonTap>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "jsontap",
        gst::Rank::NONE,
        JsonTap::static_type(),
    )
}
