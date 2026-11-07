//! Analytics metadata bridges for `GStreamer` 1.28 relation meta.
//!
//! - [`json`]: `JsonMtd`, a schema-tagged JSON document type shared by every
//!   plugin in the process.
//! - [`segmentation`]: binary GRAY8 segmentation masks.

use std::sync::LazyLock;

pub mod json;
pub mod segmentation;

pub use json::{JsonDocument, JsonMtd, JsonMtdExt};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "analytics-meta",
        gst::DebugColorFlags::empty(),
        Some("Analytics metadata bridges"),
    )
});

