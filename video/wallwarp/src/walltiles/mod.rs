use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct WallTiles(ObjectSubclass<imp::WallTiles>)
        @extends gst::Bin, gst::Element, gst::Object,
        @implements gst::ChildProxy;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "walltiles",
        gst::Rank::NONE,
        WallTiles::static_type(),
    )
}
