//! `GStreamer` inference elements powered by `usls`.

use gst::glib;

mod backend;
mod frame;
mod uslsobjectdetect;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    uslsobjectdetect::register(plugin)
}

gst::plugin_define!(
    usls,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "Apache-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
