//! Binary segmentation masks, until gstreamer-analytics exposes segmentation metadata.

#![expect(unsafe_code, reason = "implements an Mtd type over the C API")]

use gst::glib::{
    self,
    translate::{IntoGlibPtr as _, from_glib_full},
};
use gst_analytics::{AnalyticsRelationMeta, ffi};

/// Attach a full-frame binary GRAY8 mask. Region 0 is background, region 1 foreground.
pub fn add(
    meta: &mut gst::MetaRefMut<'_, AnalyticsRelationMeta, gst::meta::Standalone>,
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    semantic: bool,
) -> Result<u32, glib::BoolError> {
    let stride =
        i32::try_from(width).map_err(|err| glib::bool_error!("mask width overflow: {err}"))?;
    if width == 0
        || height == 0
        || height > i32::MAX as u32
        || (width as usize).checked_mul(height as usize) != Some(pixels.len())
        || pixels.iter().any(|&value| value > 1)
    {
        return Err(glib::bool_error!(
            "invalid binary mask dimensions or region ids"
        ));
    }
    let mut buffer = gst::Buffer::from_mut_slice(pixels);
    gst_video::VideoMeta::add_full(
        buffer.make_mut(),
        gst_video::VideoFrameFlags::empty(),
        gst_video::VideoFormat::Gray8,
        width,
        height,
        &[0],
        &[stride],
    )?;
    let buffer_ptr = buffer.into_glib_ptr();
    let mut ids = [0, 1];
    let mut result = std::mem::MaybeUninit::uninit();
    // SAFETY: meta is exclusively borrowed; buffer and ids remain live. C copies
    // the ids, takes ownership of the buffer on success, and initializes result.
    let success = unsafe {
        ffi::gst_analytics_relation_meta_add_segmentation_mtd(
            meta.as_mut_ptr(),
            buffer_ptr,
            if semantic {
                ffi::GST_SEGMENTATION_TYPE_SEMANTIC
            } else {
                ffi::GST_SEGMENTATION_TYPE_INSTANCE
            },
            ids.len(),
            ids.as_mut_ptr(),
            0,
            0,
            width,
            height,
            result.as_mut_ptr(),
        )
    };
    if success == glib::ffi::GFALSE {
        // SAFETY: failure left the transferred reference unconsumed.
        drop(unsafe { gst::Buffer::from_glib_full(buffer_ptr) });
        return Err(glib::bool_error!("could not attach segmentation mask"));
    }
    // SAFETY: the successful call initialized the complete stack handle.
    Ok(unsafe { result.assume_init() }.id)
}

fn handle(
    meta: &gst::MetaRef<'_, AnalyticsRelationMeta>,
    id: u32,
) -> Option<ffi::GstAnalyticsSegmentationMtd> {
    let mut handle = std::mem::MaybeUninit::uninit();
    // SAFETY: meta stays borrowed; the C API checks the ID and metadata type.
    let found = unsafe {
        ffi::gst_analytics_relation_meta_get_segmentation_mtd(
            meta.as_ptr().cast_mut(),
            id,
            handle.as_mut_ptr(),
        )
    };
    // SAFETY: a successful get initialized the handle, which stays tied to the
    // live meta borrow of the caller.
    (found != glib::ffi::GFALSE).then(|| unsafe { handle.assume_init() })
}

/// Retrieve an owned mask buffer by segmentation metadata ID.
#[must_use]
pub fn mask(meta: &gst::MetaRef<'_, AnalyticsRelationMeta>, id: u32) -> Option<gst::Buffer> {
    let handle = handle(meta, id)?;
    // SAFETY: optional rectangle pointers may be null; returned buffer owns a ref.
    let buffer = unsafe {
        ffi::gst_analytics_segmentation_mtd_get_mask(
            &raw const handle,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // SAFETY: get_mask transfers a full reference (or null), adopted exactly once.
    unsafe { from_glib_full(buffer) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mask_survives_copy_and_original_drop() {
        gst::init().expect("GStreamer initializes");
        let mut original = gst::Buffer::new();
        let id = {
            let mut meta = AnalyticsRelationMeta::add(original.make_mut());
            assert!(add(&mut meta, vec![0, 2], 2, 1, false).err().is_some());
            assert!(add(&mut meta, vec![0], 2, 1, false).err().is_some());
            add(&mut meta, vec![0, 1, 1, 0, 1, 0], 3, 2, false).expect("mask attaches")
        };
        let copied = original.copy_deep().expect("deep copy");
        drop(original);
        let meta = copied.meta::<AnalyticsRelationMeta>().expect("meta");
        assert!(mask(&meta, id + 100).is_none());
        let mask = mask(&meta, id).expect("mask");
        assert_eq!(
            mask.map_readable().expect("readable").as_slice(),
            [0, 1, 1, 0, 1, 0]
        );
        let video = mask.meta::<gst_video::VideoMeta>().expect("video meta");
        assert_eq!(
            (video.width(), video.height(), video.stride()),
            (3, 2, &[3][..])
        );
    }
}
