//! `GStreamer` elements that turn analytics relation metadata from any
//! producer into events and overlays. They read only standard Mtds and the
//! generic `x-*` documents of `analytics-meta`.

use gst::glib;

mod relationmetaevents;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    relationmetaevents::register(plugin)
}

gst::plugin_define!(
    relationmeta,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "Apache-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
