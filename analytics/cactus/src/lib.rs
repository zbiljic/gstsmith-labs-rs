//! `GStreamer` elements running models on the Cactus engine. The engine is
//! one process-wide object that holds every model, so its vision and speech
//! elements share this plugin and one lock.

use gst::glib;

mod cactusmirage;
mod cactuswhistle;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    cactusmirage::register(plugin)?;
    cactuswhistle::register(plugin)
}

gst::plugin_define!(
    cactus,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "Apache-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
