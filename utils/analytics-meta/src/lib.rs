//! Analytics metadata bridges for `GStreamer` 1.28 relation meta.
//!
//! - [`json`]: `JsonMtd`, a schema-tagged JSON document type shared by every
//!   plugin in the process.
//! - [`schema`]: generic `x-*` documents (polygons, relation predicates,
//!   captions, text, provenance) and their relation conventions.
//! - [`record`]: a JSON record of a whole relation meta.
//! - [`segmentation`]: binary GRAY8 segmentation masks.

use std::sync::LazyLock;

pub mod json;
pub mod record;
pub mod schema;
pub mod segmentation;

pub use json::{JsonDocument, JsonMtd, JsonMtdExt};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "analytics-meta",
        gst::DebugColorFlags::empty(),
        Some("Analytics metadata bridges"),
    )
});

/// Returns the buffer's relation meta, adding one if there is none.
pub fn relation_meta_mut(
    buffer: &mut gst::BufferRef,
) -> Result<
    gst::MetaRefMut<'_, gst_analytics::AnalyticsRelationMeta, gst::meta::Standalone>,
    gst::glib::BoolError,
> {
    use gst_analytics::AnalyticsRelationMeta;
    if buffer.meta::<AnalyticsRelationMeta>().is_some() {
        buffer
            .meta_mut::<AnalyticsRelationMeta>()
            .ok_or_else(|| gst::glib::bool_error!("relation meta is not writable"))
    } else {
        Ok(AnalyticsRelationMeta::add(buffer))
    }
}
