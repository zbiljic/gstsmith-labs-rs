use anyhow::{Context, Result, ensure};
use gst_video::prelude::*;

pub(crate) fn packed_size(width: u32, height: u32, maximum: u32) -> Result<usize> {
    ensure!(width > 0 && height > 0, "empty RGB frame");
    let size = usize::try_from(width)?
        .checked_mul(3)
        .and_then(|row| row.checked_mul(usize::try_from(height).ok()?))
        .context("RGB frame size overflow")?;
    ensure!(
        size <= usize::try_from(maximum)?,
        "RGB frame exceeds max-frame-bytes"
    );
    Ok(size)
}

fn validate_layout(size: usize, offset: usize, stride: i32, width: u32, height: u32) -> Result<()> {
    ensure!(width > 0 && height > 0, "empty RGB frame");
    let row = usize::try_from(width)?
        .checked_mul(3)
        .context("RGB row overflow")?;
    ensure!(
        usize::try_from(stride.unsigned_abs())? >= row,
        "RGB stride is shorter than a row"
    );
    let first = i64::try_from(offset)?;
    let last = first
        .checked_add(
            i64::from(stride)
                .checked_mul(i64::from(height - 1))
                .context("row offset overflow")?,
        )
        .context("row offset overflow")?;
    let minimum = usize::try_from(first.min(last)).context("row precedes mapped memory")?;
    let maximum = usize::try_from(first.max(last))?
        .checked_add(row)
        .context("row end overflow")?;
    ensure!(
        minimum <= size && maximum <= size,
        "RGB rows exceed carrier memory"
    );
    Ok(())
}

pub(crate) fn copy_rows(
    data: &[u8],
    offset: usize,
    stride: i32,
    width: u32,
    height: u32,
    maximum: u32,
) -> Result<Vec<u8>> {
    let size = packed_size(width, height, maximum)?;
    validate_layout(data.len(), offset, stride, width, height)?;
    let row = usize::try_from(width)?
        .checked_mul(3)
        .context("RGB row overflow")?;
    let mut packed = Vec::new();
    packed
        .try_reserve_exact(size)
        .context("allocating packed RGB image")?;
    for index in 0..height {
        let start = i64::try_from(offset)?
            .checked_add(
                i64::from(stride)
                    .checked_mul(i64::from(index))
                    .context("row offset overflow")?,
            )
            .context("row offset overflow")?;
        let start = usize::try_from(start).context("row precedes mapped memory")?;
        let end = start.checked_add(row).context("row end overflow")?;
        packed.extend_from_slice(
            data.get(start..end)
                .context("RGB row exceeds mapped memory")?,
        );
    }
    Ok(packed)
}

pub(crate) fn image_rgb(
    buffer: &gst::BufferRef,
    info: &gst_video::VideoInfo,
    maximum: u32,
) -> Result<usls::Image> {
    let (map, offset, stride) = map_rgb(buffer, info, maximum)?;
    let packed = copy_rows(
        map.as_slice(),
        offset,
        stride,
        info.width(),
        info.height(),
        maximum,
    )?;
    Ok(
        image::RgbImage::from_raw(info.width(), info.height(), packed)
            .context("invalid packed RGB dimensions")?
            .into(),
    )
}

pub(crate) fn validate_rgb(
    buffer: &gst::BufferRef,
    info: &gst_video::VideoInfo,
    maximum: u32,
) -> Result<()> {
    map_rgb(buffer, info, maximum)?;
    Ok(())
}

fn map_rgb<'a>(
    buffer: &'a gst::BufferRef,
    info: &gst_video::VideoInfo,
    maximum: u32,
) -> Result<(gst::BufferMap<'a, gst::buffer::Readable>, usize, i32)> {
    // Bound both carrier coalescing and image allocation before either map.
    ensure!(
        buffer.size() <= usize::try_from(maximum)?,
        "RGB carrier exceeds max-frame-bytes"
    );
    packed_size(info.width(), info.height(), maximum)?;
    let (offset, stride) = if let Some(meta) = buffer.meta::<gst_video::VideoMeta>() {
        ensure!(
            meta.format() == gst_video::VideoFormat::Rgb
                && meta.width() == info.width()
                && meta.height() == info.height(),
            "VideoMeta does not match negotiated RGB dimensions"
        );
        (
            *meta.offset().first().context("missing RGB offset")?,
            *meta.stride().first().context("missing RGB stride")?,
        )
    } else {
        (
            *info.offset().first().context("missing RGB offset")?,
            *info.stride().first().context("missing RGB stride")?,
        )
    };
    validate_layout(buffer.size(), offset, stride, info.width(), info.height())?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, info)
        .context("mapping RGB VideoFrame")?;
    ensure!(
        frame.format() == gst_video::VideoFormat::Rgb
            && frame.width() == info.width()
            && frame.height() == info.height(),
        "VideoMeta does not match negotiated RGB dimensions"
    );
    ensure!(
        frame.plane_stride().first() == Some(&stride),
        "mapped RGB stride changed"
    );
    // Safe carrier slices also handle negative stride and a last row without
    // trailing padding; the binding's plane_data() assumes a full positive plane.
    let map = buffer.map_readable().context("mapping RGB carrier")?;
    Ok((map, offset, stride))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_odd_rows_and_negative_stride() -> Result<()> {
        let data = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 99, 99, 99, 10, 11, 12, 13, 14, 15, 16, 17, 18,
        ];
        ensure!(
            copy_rows(&data, 0, 12, 3, 2, 64)? == (1..=18).collect::<Vec<_>>(),
            "fixture assertion failed"
        );
        ensure!(
            copy_rows(&data, 12, -12, 3, 2, 64)? == (10..=18).chain(1..=9).collect::<Vec<_>>(),
            "fixture assertion failed"
        );
        copy_rows(&data, 0, -12, 3, 2, 64).expect_err("negative row offset");
        copy_rows(&data, 0, 8, 3, 2, 64).expect_err("short stride");
        copy_rows(&data, 12, 12, 3, 2, 64).expect_err("truncated second row");
        packed_size(3, 2, 17).expect_err("packed limit");
        packed_size(u32::MAX, u32::MAX, u32::MAX).expect_err("overflow or limit");
        Ok(())
    }

    #[test]
    fn owned_images_honor_video_meta_stride_and_offset() -> Result<()> {
        gst::init()?;
        let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgb, 3, 2).build()?;
        for (offset, stride, expected) in [
            (1, 12, (1..=18).collect::<Vec<_>>()),
            (13, -12, (10..=18).chain(1..=9).collect::<Vec<_>>()),
        ] {
            let mut data = vec![99];
            data.extend(1..=9);
            data.extend([99; 3]);
            data.extend(10..=18);
            let mut buffer = gst::Buffer::from_mut_slice(data.clone());
            gst_video::VideoMeta::add_full(
                buffer.make_mut(),
                gst_video::VideoFrameFlags::empty(),
                gst_video::VideoFormat::Rgb,
                3,
                2,
                &[offset],
                &[stride],
            )?;
            let image = image_rgb(&buffer, &info, 1024)?;
            ensure!(image.as_raw() == &expected, "image rows");
            ensure!(buffer.map_readable()?.as_slice() == data, "carrier changed");
        }
        Ok(())
    }
    #[test]
    fn padded_carrier_is_bounded_before_mapping() -> Result<()> {
        gst::init()?;
        let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgb, 1, 1).build()?;
        let buffer = gst::Buffer::with_size(1024)?;
        image_rgb(&buffer, &info, 32)
            .expect_err("carrier exceeds limit even though packed pixels fit");
        Ok(())
    }
    #[test]
    fn malformed_video_meta_rows_are_rejected() -> Result<()> {
        gst::init()?;
        let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgb, 3, 2).build()?;
        for (size, offset, stride) in [(24, 20, 12), (20, 0, 12), (24, 0, -12)] {
            let mut buffer = gst::Buffer::with_size(size)?;
            gst_video::VideoMeta::add_full(
                buffer.make_mut(),
                gst_video::VideoFrameFlags::empty(),
                gst_video::VideoFormat::Rgb,
                3,
                2,
                &[offset],
                &[stride],
            )?;
            image_rgb(&buffer, &info, 1024).expect_err("malformed VideoMeta extent");
        }
        Ok(())
    }
}
