use gst::glib;
use gst::prelude::*;

mod imp;
mod observe;
mod state;

glib::wrapper! {
    pub struct RelationMetaEvents(ObjectSubclass<imp::RelationMetaEvents>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "relationmetaevents",
        gst::Rank::NONE,
        RelationMetaEvents::static_type(),
    )
}
