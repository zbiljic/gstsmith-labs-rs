//! Packed RGB24 copies of negotiated RGB frames, which `GStreamer` pads to
//! 4-byte rows, and optional downscaling of the analyzed copy.

use std::fmt::Display;

fn fail(message: impl Display) -> String {
    message.to_string()
}

/// Bytes of a packed `width` × `height` RGB24 copy, if within `maximum`.
pub fn packed_size(width: u32, height: u32, maximum: u32) -> Result<usize, String> {
    if width == 0 || height == 0 {
        return Err(fail("empty RGB frame"));
    }
    let size = usize::try_from(width)
        .ok()
        .and_then(|w| w.checked_mul(3)?.checked_mul(usize::try_from(height).ok()?))
        .ok_or_else(|| fail("RGB frame size overflow"))?;
    if size > usize::try_from(maximum).unwrap_or(usize::MAX) {
        return Err(format!(
            "a {width}x{height} frame needs {size} bytes, more than max-frame-bytes ({maximum})"
        ));
    }
    Ok(size)
}

/// Copies rows of `width * 3` bytes from `data` at `offset`, `stride` apart
/// (negative strides run upwards), into a packed buffer.
pub fn copy_rows(
    data: &[u8],
    offset: usize,
    stride: i32,
    width: u32,
    height: u32,
    maximum: u32,
) -> Result<Vec<u8>, String> {
    let size = packed_size(width, height, maximum)?;
    let row = size / usize::try_from(height).map_err(fail)?;
    if usize::try_from(stride.unsigned_abs()).map_err(fail)? < row {
        return Err(fail("RGB stride is shorter than a row"));
    }
    let mut packed = Vec::new();
    packed
        .try_reserve_exact(size)
        .map_err(|err| format!("allocating the packed RGB copy: {err}"))?;
    for index in 0..i64::from(height) {
        let start = i64::try_from(offset)
            .ok()
            .and_then(|offset| offset.checked_add(i64::from(stride).checked_mul(index)?))
            .and_then(|start| usize::try_from(start).ok())
            .ok_or_else(|| fail("RGB row precedes mapped memory"))?;
        let line = start
            .checked_add(row)
            .and_then(|end| data.get(start..end))
            .ok_or_else(|| fail("RGB row exceeds mapped memory"))?;
        packed.extend_from_slice(line);
    }
    Ok(packed)
}

/// Copies a mapped RGB frame, honouring its `VideoMeta` layout.
pub fn packed_rgb(
    buffer: &gst::BufferRef,
    info: &gst_video::VideoInfo,
    maximum: u32,
) -> Result<Vec<u8>, String> {
    if buffer.size() > usize::try_from(maximum).unwrap_or(usize::MAX) {
        return Err(fail("the RGB frame exceeds max-frame-bytes"));
    }
    let (offset, stride) = match buffer.meta::<gst_video::VideoMeta>() {
        Some(meta) => {
            if meta.format() != gst_video::VideoFormat::Rgb
                || meta.width() != info.width()
                || meta.height() != info.height()
            {
                return Err(fail("VideoMeta does not match the negotiated RGB frame"));
            }
            (
                *meta
                    .offset()
                    .first()
                    .ok_or_else(|| fail("missing offset"))?,
                *meta
                    .stride()
                    .first()
                    .ok_or_else(|| fail("missing stride"))?,
            )
        }
        None => (
            *info
                .offset()
                .first()
                .ok_or_else(|| fail("missing offset"))?,
            *info
                .stride()
                .first()
                .ok_or_else(|| fail("missing stride"))?,
        ),
    };
    let map = buffer
        .map_readable()
        .map_err(|err| format!("mapping the frame: {err}"))?;
    copy_rows(
        map.as_slice(),
        offset,
        stride,
        info.width(),
        info.height(),
        maximum,
    )
}

/// The analyzed size: the frame scaled so its longer side is at most
/// `max_side`, keeping the aspect ratio; 0 keeps the frame size.
#[must_use]
pub fn analysis_size(width: u32, height: u32, max_side: u32) -> (u32, u32) {
    let longer = width.max(height);
    if max_side == 0 || longer <= max_side {
        return (width, height);
    }
    let scale = |side: u32| {
        u32::try_from(u64::from(side) * u64::from(max_side) / u64::from(longer))
            .unwrap_or(1)
            .max(1)
    };
    (scale(width), scale(height))
}

/// Downscales a packed RGB copy to `size`.
pub fn resize(packed: Vec<u8>, from: (u32, u32), to: (u32, u32)) -> Result<Vec<u8>, String> {
    if from == to {
        return Ok(packed);
    }
    let image = image::RgbImage::from_raw(from.0, from.1, packed)
        .ok_or_else(|| fail("packed RGB size mismatch"))?;
    Ok(
        image::imageops::resize(&image, to.0, to.1, image::imageops::FilterType::Triangle)
            .into_raw(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_padded_and_upside_down_rows() {
        // Two 1-pixel rows padded to 4 bytes.
        let data = [1, 2, 3, 0, 4, 5, 6, 0];
        assert_eq!(
            copy_rows(&data, 0, 4, 1, 2, 1024),
            Ok(vec![1, 2, 3, 4, 5, 6])
        );
        assert_eq!(
            copy_rows(&data, 4, -4, 1, 2, 1024),
            Ok(vec![4, 5, 6, 1, 2, 3])
        );
        assert!(copy_rows(&data, 0, 2, 1, 2, 1024).is_err(), "short stride");
        assert!(copy_rows(&data, 4, 4, 1, 2, 1024).is_err(), "past the end");
        assert!(
            copy_rows(&data, 0, 4, 1, 2, 5).is_err(),
            "over max-frame-bytes"
        );
    }

    #[test]
    fn analysis_size_keeps_the_aspect_ratio() {
        assert_eq!(analysis_size(1920, 1080, 0), (1920, 1080));
        assert_eq!(analysis_size(1920, 1080, 4096), (1920, 1080));
        assert_eq!(analysis_size(1920, 1080, 960), (960, 540));
        assert_eq!(analysis_size(1080, 1920, 960), (540, 960));
        assert_eq!(analysis_size(4000, 1, 100), (100, 1));
    }

    #[test]
    fn resizes_the_analyzed_copy() {
        let packed = vec![200u8; 4 * 2 * 3];
        let small = resize(packed, (4, 2), (2, 1)).expect("resize");
        assert_eq!(small, vec![200u8; 2 * 3]);
    }
}
