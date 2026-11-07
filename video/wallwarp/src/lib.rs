//! `GStreamer` perspective warps mapping a source quadrilateral onto one video
//! wall display, on the CPU (`wallwarp`) or in OpenGL (`glwallwarp`).

use gst::glib;

mod glwallwarp;
mod wallwarp;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    wallwarp::register(plugin)?;
    glwallwarp::register(plugin)
}

gst::plugin_define!(
    wallwarp,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "Apache-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
