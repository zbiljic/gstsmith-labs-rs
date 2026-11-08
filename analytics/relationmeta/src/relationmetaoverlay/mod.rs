use gst::glib;
use gst::prelude::*;

mod imp;
mod layout;
mod render;
mod scene;

glib::wrapper! {
    pub struct RelationMetaOverlay(ObjectSubclass<imp::RelationMetaOverlay>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "relationmetaoverlay",
        gst::Rank::NONE,
        RelationMetaOverlay::static_type(),
    )
}
