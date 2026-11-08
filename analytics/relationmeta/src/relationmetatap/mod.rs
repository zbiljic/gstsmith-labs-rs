use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct RelationMetaTap(ObjectSubclass<imp::RelationMetaTap>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "relationmetatap",
        gst::Rank::NONE,
        RelationMetaTap::static_type(),
    )
}
