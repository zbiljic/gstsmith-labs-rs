//! A frame number drawn into every output while measuring, so each browser
//! can tell which frame it is presenting.
//!
//! The code is a strip of [`BITS`] columns of square blocks in the output's
//! bottom-left corner, most significant bit first. A column's top block is
//! white for a one and black for a zero, and its bottom block is the
//! opposite, so a reader compares the two blocks instead of guessing a
//! threshold, and rejects a column whose blocks are too alike, as in a frame
//! blurred by compression. Browsers read the code in `display.html`; [`read`]
//! does the same for tests. Codes go into RGBA frames, or into the luma of
//! I420 frames over neutral chroma.

use serde::Serialize;

/// Bits in the code; frame numbers wrap every `2^BITS` frames.
pub const BITS: u32 = 12;

/// Frame numbers repeat after this many frames.
pub const MODULUS: u64 = 1 << BITS;

/// Smallest difference between a column's two blocks, out of 255, for the
/// column to count as read.
pub const MIN_CONTRAST: u32 = 48;

/// Block sides, in output pixels, kept large enough to survive compression and
/// small enough to stay out of the way.
const BLOCK_SIDES: std::ops::RangeInclusive<u32> = 8..=32;

/// Where the code sits in an output, in output pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CodeGeometry {
    pub bits: u32,
    /// Side of one block.
    pub block: u32,
    pub left: u32,
    pub top: u32,
}

impl CodeGeometry {
    /// Blocks a fortieth of the output height, in its bottom-left corner, or
    /// `None` when the code does not fit.
    #[must_use]
    pub fn for_output(width: u32, height: u32) -> Option<Self> {
        let block = (height / 40).clamp(*BLOCK_SIDES.start(), *BLOCK_SIDES.end());
        (BITS * block <= width && 2 * block <= height).then(|| Self {
            bits: BITS,
            block,
            left: 0,
            top: height - 2 * block,
        })
    }

    fn block_origin(&self, bit: u32, bottom: bool) -> (usize, usize) {
        let x = self.left + bit * self.block;
        let y = self.top + if bottom { self.block } else { 0 };
        (x as usize, y as usize)
    }
}

/// How the pixels a code is drawn into are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packing {
    /// Four bytes per pixel: red, green, blue and alpha.
    Rgba,
    /// One byte of luma per pixel, as in the Y plane of I420.
    Luma,
}

impl Packing {
    const fn bytes(self) -> usize {
        match self {
            Self::Rgba => 4,
            Self::Luma => 1,
        }
    }
}

/// Draw `frame` modulo [`MODULUS`] into pixels packed as `packing`, with
/// rows `stride` bytes apart.
pub fn stamp(
    pixels: &mut [u8],
    stride: usize,
    packing: Packing,
    geometry: &CodeGeometry,
    frame: u64,
) {
    let (block, bytes) = (geometry.block as usize, packing.bytes());
    for bit in 0..geometry.bits {
        let one = (frame >> (geometry.bits - 1 - bit)) & 1 == 1;
        for bottom in [false, true] {
            let value = if one == bottom { 0 } else { 255 };
            let (x, y) = geometry.block_origin(bit, bottom);
            for row in y..y + block {
                let start = row * stride + x * bytes;
                if let Some(span) = pixels.get_mut(start..start + block * bytes) {
                    match packing {
                        Packing::Rgba => {
                            span.as_chunks_mut::<4>().0.fill([value, value, value, 255]);
                        }
                        Packing::Luma => span.fill(value),
                    }
                }
            }
        }
    }
}

/// Make the chroma under the code neutral, in a plane subsampled by two
/// both ways as I420's U and V planes are, so its blocks stay black and
/// white.
pub fn clear_chroma(chroma: &mut [u8], stride: usize, geometry: &CodeGeometry) {
    let (left, top) = (geometry.left as usize / 2, geometry.top as usize / 2);
    let right = (geometry.left + geometry.bits * geometry.block).div_ceil(2) as usize;
    let bottom = (geometry.top + 2 * geometry.block).div_ceil(2) as usize;
    for row in top..bottom {
        if let Some(span) = chroma.get_mut(row * stride + left..row * stride + right) {
            span.fill(128);
        }
    }
}

/// Mean luma of the middle half of one block, or `None` outside the image.
fn block_luma(
    pixels: &[u8],
    stride: usize,
    packing: Packing,
    (x, y): (usize, usize),
    block: usize,
) -> Option<u32> {
    let (start, end) = (block / 4, block - block / 4);
    let bytes = packing.bytes();
    let (mut sum, mut count) = (0_u32, 0_u32);
    for row in y + start..y + end {
        let offset = row * stride + (x + start) * bytes;
        let span = pixels.get(offset..offset + (end - start) * bytes)?;
        match packing {
            Packing::Rgba => {
                for [r, g, b, _] in span.as_chunks::<4>().0 {
                    sum += (u32::from(*r) + u32::from(*g) + u32::from(*b)) / 3;
                }
            }
            Packing::Luma => sum += span.iter().map(|value| u32::from(*value)).sum::<u32>(),
        }
        count += u32::try_from(end - start).ok()?;
    }
    sum.checked_div(count)
}

/// Read a code back from pixels packed as `packing`, or `None` if any
/// column is unreadable.
#[must_use]
pub fn read(
    pixels: &[u8],
    stride: usize,
    packing: Packing,
    geometry: &CodeGeometry,
) -> Option<u64> {
    let block = geometry.block as usize;
    let mut value = 0;
    for bit in 0..geometry.bits {
        let top = block_luma(
            pixels,
            stride,
            packing,
            geometry.block_origin(bit, false),
            block,
        )?;
        let bottom = block_luma(
            pixels,
            stride,
            packing,
            geometry.block_origin(bit, true),
            block,
        )?;
        if top.abs_diff(bottom) < MIN_CONTRAST {
            return None;
        }
        value = (value << 1) | u64::from(top > bottom);
    }
    Some(value)
}

/// The frame number with code `code` closest to `near`.
#[must_use]
pub fn unwrap(code: u64, near: u64) -> u64 {
    let base = near - near % MODULUS + code % MODULUS;
    [
        base.checked_sub(MODULUS),
        Some(base),
        base.checked_add(MODULUS),
    ]
    .into_iter()
    .flatten()
    .min_by_key(|candidate| candidate.abs_diff(near))
    .unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32, fill: u8) -> Vec<u8> {
        vec![fill; (width * height * 4) as usize]
    }

    #[test]
    fn codes_fit_outputs_and_scale_with_height() {
        let small = CodeGeometry::for_output(320, 180).expect("fits");
        assert_eq!((small.block, small.top), (8, 164));
        let large = CodeGeometry::for_output(3840, 2160).expect("fits");
        assert_eq!((large.block, large.top), (32, 2096));
        assert_eq!(CodeGeometry::for_output(90, 400), None);
        assert_eq!(CodeGeometry::for_output(400, 12), None);
    }

    #[test]
    fn stamped_codes_read_back() {
        let (width, height) = (1280, 720);
        let geometry = CodeGeometry::for_output(width, height).expect("fits");
        for fill in [0, 128, 255] {
            for frame in [0, 1, 0x0a5c, MODULUS - 1, MODULUS + 7] {
                for (packing, bytes) in [(Packing::Rgba, 4), (Packing::Luma, 1)] {
                    let stride = width as usize * bytes;
                    let mut pixels = vec![fill; stride * height as usize];
                    stamp(&mut pixels, stride, packing, &geometry, frame);
                    assert_eq!(
                        read(&pixels, stride, packing, &geometry),
                        Some(frame % MODULUS),
                        "{packing:?} frame {frame} over {fill}"
                    );
                }
            }
        }
    }

    #[test]
    fn padded_rows_and_soft_edges_still_read() {
        let (width, height) = (640, 360);
        let stride = width as usize * 4 + 64;
        let geometry = CodeGeometry::for_output(width, height).expect("fits");
        let mut pixels = vec![90; stride * height as usize];
        stamp(&mut pixels, stride, Packing::Rgba, &geometry, 1234);
        // Smear every block's edge pixels, as compression would.
        let block = geometry.block as usize;
        for row in geometry.top as usize..height as usize {
            for x in (0..geometry.bits as usize * block).filter(|x| x % block == 0) {
                if let Some(pixel) = pixels.get_mut(row * stride + x * 4..row * stride + x * 4 + 3)
                {
                    pixel.fill(128);
                }
            }
        }
        assert_eq!(read(&pixels, stride, Packing::Rgba, &geometry), Some(1234));
    }

    #[test]
    fn flat_or_missing_codes_are_unreadable() {
        let (width, height) = (640, 360);
        let geometry = CodeGeometry::for_output(width, height).expect("fits");
        let pixels = image(width, height, 128);
        let stride = width as usize * 4;
        assert_eq!(read(&pixels, stride, Packing::Rgba, &geometry), None);
        assert_eq!(
            read(&pixels[..1000], stride, Packing::Rgba, &geometry),
            None
        );
    }

    #[test]
    fn chroma_under_codes_turns_neutral() {
        // An odd-sized block still clears every chroma sample it touches.
        let geometry = CodeGeometry {
            bits: 2,
            block: 3,
            left: 1,
            top: 5,
        };
        let stride = 8;
        let mut chroma = vec![7; stride * 8];
        clear_chroma(&mut chroma, stride, &geometry);
        for (row, values) in chroma.chunks(stride).enumerate() {
            for (column, value) in values.iter().enumerate() {
                // Luma columns 1..7 and rows 5..11 halve to 0..4 and 2..6.
                let under = (0..4).contains(&column) && (2..6).contains(&row);
                assert_eq!(*value, if under { 128 } else { 7 }, "({column}, {row})");
            }
        }
    }

    #[test]
    fn codes_unwrap_to_the_nearest_frame() {
        assert_eq!(unwrap(5, 3), 5);
        assert_eq!(unwrap(MODULUS - 2, 3), MODULUS - 2);
        assert_eq!(unwrap(MODULUS - 2, MODULUS * 3 + 1), MODULUS * 3 - 2);
        assert_eq!(unwrap(3, MODULUS * 3 - 1), MODULUS * 3 + 3);
        assert_eq!(unwrap(100, MODULUS * 2 + 104), MODULUS * 2 + 100);
    }
}
