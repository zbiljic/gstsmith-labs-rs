//! Analytics metadata bridges for `GStreamer` 1.28 relation meta.
//!
//! - [`json`]: `JsonMtd`, a schema-tagged JSON document type shared by every
//!   plugin in the process.
//! - [`schema`]: generic `x-*` documents (polygons, relation predicates,
//!   captions, text, provenance) and their relation conventions.
//! - [`record`]: a JSON record of a whole relation meta.
//! - [`segmentation`]: binary GRAY8 segmentation masks.
//! - [`unpool`]: stop buffer pools from keeping a cleared relation meta.

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

/// Stops a buffer pool from keeping this relation meta when the buffer is
/// recycled.
///
/// Upstream flags a relation meta on a pooled buffer `GST_META_FLAG_POOLED`,
/// so the pool keeps it, cleared, for reuse: the recycled buffer comes back
/// with an empty relation meta, which consumers read as "analyzed, saw
/// nothing". A producer that attaches results to only some frames calls
/// this, so the pool frees the meta instead.
#[expect(unsafe_code, reason = "no binding clears a meta flag")]
pub fn unpool(
    meta: &mut gst::MetaRefMut<'_, gst_analytics::AnalyticsRelationMeta, gst::meta::Standalone>,
) {
    let raw = meta.upcast_mut().as_mut_ptr();
    // SAFETY: `raw` is the live `GstMeta` header of the exclusively
    // borrowed meta; clearing one flag bit is what upstream does with
    // GST_META_FLAG_UNSET, and no other code reads it concurrently.
    unsafe { (*raw).flags &= !gst::ffi::GST_META_FLAG_POOLED };
}

#[cfg(test)]
mod tests {
    use gst::prelude::*;
    use gst_analytics::prelude::*;

    fn pool() -> gst::BufferPool {
        let pool = gst::BufferPool::new();
        let mut config = pool.config();
        config.set_params(None, 16, 1, 1);
        pool.set_config(config).expect("pool config");
        pool.set_active(true).expect("pool active");
        pool
    }

    #[test]
    fn pools_keep_a_cleared_relation_meta_unless_it_is_unpooled() {
        gst::init().expect("GStreamer initializes");
        let pool = pool();
        let mut buffer = pool.acquire_buffer(None).expect("buffer");
        let mut meta = crate::relation_meta_mut(buffer.make_mut()).expect("meta");
        meta.add_one_cls_mtd(0.5, gst::glib::Quark::from_str("x"))
            .expect("cls");
        assert!(meta.flags().contains(gst::MetaFlags::POOLED));
        drop(buffer);
        let recycled = pool.acquire_buffer(None).expect("recycled");
        let kept = recycled
            .meta::<gst_analytics::AnalyticsRelationMeta>()
            .expect("the pool kept the meta");
        assert!(kept.is_empty(), "cleared on recycle");
        drop(recycled);

        let mut buffer = pool.acquire_buffer(None).expect("buffer");
        let mut meta = crate::relation_meta_mut(buffer.make_mut()).expect("meta");
        crate::unpool(&mut meta);
        assert!(!meta.flags().contains(gst::MetaFlags::POOLED));
        drop(buffer);
        let recycled = pool.acquire_buffer(None).expect("recycled");
        assert!(
            recycled
                .meta::<gst_analytics::AnalyticsRelationMeta>()
                .is_none(),
            "an unpooled meta is freed"
        );
        pool.set_active(false).expect("pool inactive");
    }
}
