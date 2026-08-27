use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct NalSenseReplay(ObjectSubclass<imp::NalSenseReplay>)
        @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "nalsensereplay",
        gst::Rank::NONE,
        NalSenseReplay::static_type(),
    )
}
