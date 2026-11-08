//! `GStreamer` elements that turn analytics relation metadata from any
//! producer into events, overlays and JSON records. `relationmetaevents` and
//! `relationmetaoverlay` read only standard Mtds and the generic `x-*`
//! documents of `analytics-meta`; `relationmetatap` records everything.

use gst::glib;

mod relationmetaevents;
mod relationmetaoverlay;
mod relationmetatap;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    relationmetaevents::register(plugin)?;
    relationmetaoverlay::register(plugin)?;
    relationmetatap::register(plugin)
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
