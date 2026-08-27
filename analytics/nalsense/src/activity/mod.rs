use gst::glib;
use gst::prelude::*;

mod event;
mod imp;

glib::wrapper! {
    pub struct NalSenseActivity(ObjectSubclass<imp::NalSenseActivity>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "nalsenseactivity",
        gst::Rank::NONE,
        NalSenseActivity::static_type(),
    )
}
