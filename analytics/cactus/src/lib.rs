//! `GStreamer` elements running models on Cactus engines. An engine holds
//! one model of each kind and runs one call at a time; the `engine` property
//! picks which engine an element uses, and differently named engines run at
//! the same time.

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
