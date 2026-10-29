use gst::glib;
use gst::prelude::*;

mod imp;
mod warp;

/// How output pixels sample the source.
#[derive(Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstSmithWallWarpInterpolation")]
pub enum Interpolation {
    #[enum_value(
        name = "Nearest neighbour: copy the closest source pixel",
        nick = "nearest"
    )]
    Nearest = 0,
    #[default]
    #[enum_value(
        name = "Bilinear: blend the four closest source pixels",
        nick = "bilinear"
    )]
    Bilinear = 1,
}

glib::wrapper! {
    pub struct WallWarp(ObjectSubclass<imp::WallWarp>)
        @extends gst_video::VideoFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    Interpolation::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    gst::Element::register(
        Some(plugin),
        "wallwarp",
        gst::Rank::NONE,
        WallWarp::static_type(),
    )
}
