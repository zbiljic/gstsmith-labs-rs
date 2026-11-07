//! Test-only plugin a: a `jsonmtdprobea` element that writes and reads
//! `JsonMtd` documents, so tests can load two plugin libraries that each
//! carry their own copy of `analytics-meta`.

use gst::glib;

const ELEMENT: &str = "jsonmtdprobea";
const TYPE_NAME: &str = "GstJsonMtdProbeA";

#[path = "../../probe.rs"]
mod probe;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    probe::register(plugin)
}

gst::plugin_define!(
    jsonmtdfixturea,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "Apache-2.0",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    "2026-11-07"
);
