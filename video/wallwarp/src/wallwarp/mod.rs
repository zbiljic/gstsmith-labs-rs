use gst::glib;
use gst::prelude::*;
use wall_geometry::{Homography, Mask, Quad};

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

/// A quad from its property text, checked to have a perspective transform.
pub(crate) fn parse_quad(text: &str) -> Result<Quad, String> {
    let quad = text.parse::<Quad>().map_err(|err| err.to_string())?;
    Homography::unit_square_to_quad(&quad).map_err(|err| err.to_string())?;
    Ok(quad)
}

/// The `mask` property's description.
pub(crate) const MASK_BLURB: &str = "Where the output shows the picture, other pixels showing the \
    background: polygons separated by ';', each as normalized x,y output points (0,0 is the \
    output top-left, 1,1 its bottom-right); a pixel inside an odd number of polygons shows the \
    picture. Empty shows it everywhere";

/// A mask from its property text, or why it is unusable.
pub(crate) fn parse_mask(text: &str) -> Result<Mask, String> {
    text.parse::<Mask>().map_err(|err| err.to_string())
}

/// Big-endian ARGB to RGBA bytes.
pub(crate) fn rgba(argb: u32) -> [u8; 4] {
    let [a, r, g, b] = argb.to_be_bytes();
    [r, g, b, a]
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
