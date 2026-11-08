use gst::glib;
use gst::prelude::*;

mod describer;
mod frame;
mod imp;
mod latest;
mod mapping;
mod settings;

glib::wrapper! {
    pub struct CactusMirage(ObjectSubclass<imp::CactusMirage>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "cactusmirage",
        gst::Rank::NONE,
        CactusMirage::static_type(),
    )
}
