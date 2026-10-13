//! `GStreamer` sampler keeping the first buffer of every running-time period.

use gst::glib;

mod ptssampler;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    ptssampler::register(plugin)
}

gst::plugin_define!(
    ptssampler,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "Apache-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
