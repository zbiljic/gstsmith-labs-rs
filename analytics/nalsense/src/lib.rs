//! Portable compressed-video analytics elements for `GStreamer`.

use gst::glib;

mod activity;
mod analyzer;
mod annex_b;
mod codec;
mod h264;
mod h265;
mod replay;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    activity::register(plugin)?;
    replay::register(plugin)
}

gst::plugin_define!(
    nalsense,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "Apache-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
