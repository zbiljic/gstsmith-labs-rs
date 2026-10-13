//! `GStreamer` passthrough tap recording buffers, caps, and EOS as JSON lines.

use gst::glib;

mod jsontap;
mod record;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    jsontap::register(plugin)
}

gst::plugin_define!(
    jsontap,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "Apache-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
