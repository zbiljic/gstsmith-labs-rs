use gst::{glib, prelude::*};

pub mod imp;

glib::wrapper! {
    pub struct UslsSegment(ObjectSubclass<imp::UslsSegment>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "uslssegment",
        gst::Rank::NONE,
        UslsSegment::static_type(),
    )
}
