//! Perspective resampling of packed four-byte pixels.

use std::fmt;
use std::sync::{Mutex, PoisonError};

use wall_geometry::Homography;

use super::Interpolation;

const CHANNELS: usize = 4;
/// Bilinear weights carry eight fractional bits.
const WEIGHT_ONE: u16 = 256;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum WarpError {
    /// A plane has different dimensions than the warp was prepared for.
    Size {
        expected: (u32, u32),
        actual: (u32, u32),
    },
    /// A plane's stride is shorter than a row, or its memory than its rows.
    Layout,
    /// The dimensions or transform cannot produce finite coordinates.
    Unrepresentable,
    /// The source is too wide or tall to locate samples in.
    TooLarge,
}

impl fmt::Display for WarpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Size { expected, actual } => write!(
                f,
                "frame is {}x{}, expected {}x{}",
                actual.0, actual.1, expected.0, expected.1
            ),
            Self::Layout => f.write_str("frame rows exceed the mapped memory"),
            Self::Unrepresentable => f.write_str("transform is not representable"),
            Self::TooLarge => {
                f.write_str("sources over 65534 pixels wide or 65535 tall are not supported")
            }
        }
    }
}

fn row_bytes(width: u32) -> Option<usize> {
    usize::try_from(width).ok()?.checked_mul(CHANNELS)
}

/// Validate a plane layout and return its row length in bytes.
fn check_layout(len: usize, stride: usize, width: u32, height: u32) -> Result<usize, WarpError> {
    let row = row_bytes(width).ok_or(WarpError::Layout)?;
    let rows = usize::try_from(height.saturating_sub(1)).map_err(|_err| WarpError::Layout)?;
    let needed = stride
        .checked_mul(rows)
        .and_then(|bytes| bytes.checked_add(row))
        .ok_or(WarpError::Layout)?;
    if width == 0 || height == 0 || stride < row || len < needed {
        return Err(WarpError::Layout);
    }
    Ok(row)
}

/// Read-only rows of packed four-byte pixels.
pub(super) struct Plane<'a> {
    data: &'a [u8],
    stride: usize,
    width: u32,
    height: u32,
}

impl<'a> Plane<'a> {
    pub(super) fn new(data: &'a [u8], stride: usize, width: u32, height: u32) -> Self {
        Self {
            data,
            stride,
            width,
            height,
        }
    }

    /// The pixel starting at byte `start`.
    fn texel(&self, start: usize) -> Option<[u8; CHANNELS]> {
        self.data.get(start..start + CHANNELS)?.try_into().ok()
    }

    /// The pixel starting at byte `start` and the next one in memory, or the
    /// first one twice where the plane ends.
    fn pair(&self, start: usize) -> Option<[[u8; CHANNELS]; 2]> {
        match self.data.get(start..start + 2 * CHANNELS) {
            Some(bytes) => {
                let (pair, _) = bytes.as_chunks::<CHANNELS>();
                Some([*pair.first()?, *pair.get(1)?])
            }
            None => self.texel(start).map(|texel| [texel; 2]),
        }
    }
}

/// Writable rows of packed four-byte pixels.
pub(super) struct PlaneMut<'a> {
    data: &'a mut [u8],
    stride: usize,
    width: u32,
    height: u32,
}

impl<'a> PlaneMut<'a> {
    pub(super) fn new(data: &'a mut [u8], stride: usize, width: u32, height: u32) -> Self {
        Self {
            data,
            stride,
            width,
            height,
        }
    }
}

/// Output pixels blended together: the pixels around their samples are read
/// into a batch first, so that blending runs as vector instructions.
const BATCH: usize = 64;

/// The column of a sample outside the source.
const OUTSIDE: u16 = u16::MAX;

/// Where every output pixel samples the source, row after row: the sampled
/// pixel, or for bilinear sampling the top-left one of four, and the weights
/// of the right and lower pixels.
#[derive(Default)]
struct Samples {
    /// Columns, or [`OUTSIDE`].
    columns: Vec<u16>,
    rows: Vec<u16>,
    /// Bilinear weights; empty for nearest sampling.
    across: Vec<u8>,
    down: Vec<u8>,
}

/// The pixels around a batch of bilinear samples and their weights.
struct Batch {
    /// The top-left, top-right, bottom-left and bottom-right pixels.
    corners: [[[u8; CHANNELS]; BATCH]; 4],
    /// Weights of the right and lower pixels, repeated per channel.
    across: [[u16; CHANNELS]; BATCH],
    down: [[u16; CHANNELS]; BATCH],
}

impl Batch {
    fn new() -> Self {
        Self {
            corners: [[[0; CHANNELS]; BATCH]; 4],
            across: [[0; CHANNELS]; BATCH],
            down: [[0; CHANNELS]; BATCH],
        }
    }
}

/// Blend the gathered pixels of `batch` into `output`, one channel at a time.
fn blend(batch: &Batch, output: &mut [u8]) {
    // Each row's blend is at most 255 × WEIGHT_ONE, within a u16.
    let scale = u32::from(WEIGHT_ONE) * u32::from(WEIGHT_ONE);
    let [top_left, top_right, bottom_left, bottom_right] = &batch.corners;
    for ((((((value, tl), tr), bl), br), across), down) in output
        .iter_mut()
        .zip(top_left.as_flattened())
        .zip(top_right.as_flattened())
        .zip(bottom_left.as_flattened())
        .zip(bottom_right.as_flattened())
        .zip(batch.across.as_flattened())
        .zip(batch.down.as_flattened())
    {
        let upper = u16::from(*tl) * (WEIGHT_ONE - across) + u16::from(*tr) * across;
        let lower = u16::from(*bl) * (WEIGHT_ONE - across) + u16::from(*br) * across;
        let total =
            u32::from(upper) * u32::from(WEIGHT_ONE - down) + u32::from(lower) * u32::from(*down);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a rounded blend of bytes is at most 255"
        )]
        let rounded = ((total + scale / 2) / scale) as u8;
        *value = rounded;
    }
}

/// A perspective warp prepared for fixed source and output dimensions.
pub(super) struct Warp {
    /// Maps output pixel coordinates to source pixel coordinates, where pixel
    /// `(i, j)` covers `[i, i + 1) × [j, j + 1)`.
    matrix: [[f64; 3]; 3],
    interpolation: Interpolation,
    background: [u8; CHANNELS],
    source: (u32, u32),
    output: (u32, u32),
    samples: Samples,
}

impl Warp {
    /// Prepare a warp from a map of display-local `[0, 1]²` coordinates to
    /// normalized source coordinates, locating every output pixel's sample.
    #[expect(
        clippy::many_single_char_names,
        reason = "matrix entries follow the a to i naming of wall_geometry::Homography"
    )]
    pub(super) fn new(
        local_to_source: &Homography,
        source: (u32, u32),
        output: (u32, u32),
        interpolation: Interpolation,
        background: [u8; CHANNELS],
    ) -> Result<Self, WarpError> {
        if source.0 == 0 || source.1 == 0 || output.0 == 0 || output.1 == 0 {
            return Err(WarpError::Unrepresentable);
        }
        // Columns and rows are stored in 16 bits, with one value marking
        // samples outside the source.
        if source.0 >= u32::from(OUTSIDE) || source.1 > u32::from(u16::MAX) {
            return Err(WarpError::TooLarge);
        }
        let [[a, b, c], [d, e, f], [g, h, i]] = local_to_source.matrix();
        let (source_width, source_height) = (f64::from(source.0), f64::from(source.1));
        let (output_width, output_height) = (f64::from(output.0), f64::from(output.1));
        // Scale to source pixels after the homography and from output pixels
        // before it. Dividing, rather than multiplying by reciprocals, keeps
        // equal source and output sizes exact.
        let matrix = [
            [
                source_width * a / output_width,
                source_width * b / output_height,
                source_width * c,
            ],
            [
                source_height * d / output_width,
                source_height * e / output_height,
                source_height * f,
            ],
            [g / output_width, h / output_height, i],
        ];
        if !matrix.iter().flatten().all(|value| value.is_finite()) {
            return Err(WarpError::Unrepresentable);
        }
        let mut warp = Self {
            matrix,
            interpolation,
            background,
            source,
            output,
            samples: Samples::default(),
        };
        warp.samples = match interpolation {
            Interpolation::Nearest => warp.locate::<false>()?,
            Interpolation::Bilinear => warp.locate::<true>()?,
        };
        Ok(warp)
    }

    /// Locate every output pixel's sample. Branch-free and in floating
    /// point, so that it vectorizes.
    ///
    /// Bilinear sampling clamps to the edge through its weights: a position
    /// outside the outermost pixel centers puts all the weight on the
    /// outermost pixel, which sums the same as blending it with itself.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "columns, rows and weights are whole and fit their types once clamped to the source"
    )]
    #[expect(
        clippy::needless_bitwise_bool,
        reason = "evaluating every comparison keeps the loops free of branches"
    )]
    #[expect(
        clippy::many_single_char_names,
        reason = "matrix entries follow the a to i naming of wall_geometry::Homography"
    )]
    fn locate<const BILINEAR: bool>(&self) -> Result<Samples, WarpError> {
        let [[a, b, c], [d, e, f], [g, h, i]] = self.matrix;
        let (width, height) = (f64::from(self.source.0), f64::from(self.source.1));
        let row = usize::try_from(self.output.0).map_err(|_err| WarpError::TooLarge)?;
        let count = usize::try_from(self.output.1)
            .ok()
            .and_then(|rows| rows.checked_mul(row))
            .ok_or(WarpError::TooLarge)?;
        let weights = if BILINEAR { count } else { 0 };
        let mut samples = Samples {
            columns: vec![0; count],
            rows: vec![0; count],
            across: vec![0; weights],
            down: vec![0; weights],
        };
        let one = f64::from(WEIGHT_ONE);
        // The pixel left of or above a position between pixel centers, and
        // the weight of the next one.
        let split = |position: f64, last: f64| {
            let fixed = ((position - 0.5) * one).floor();
            let pixel = (fixed / one).floor();
            let weight = if (0.0 <= pixel) & (pixel < last) {
                fixed - pixel * one
            } else {
                0.0
            };
            // Unlike `clamp`, which panics on unordered bounds, this leaves
            // the loop free of branches.
            (pixel.max(0.0).min(last), weight)
        };
        // Weights stay empty for nearest sampling, so walk them only when
        // there are some.
        let mut across = samples.across.chunks_mut(row);
        let mut down = samples.down.chunks_mut(row);
        for ((y, columns), rows) in (0..self.output.1)
            .zip(samples.columns.chunks_mut(row))
            .zip(samples.rows.chunks_mut(row))
        {
            let v = f64::from(y) + 0.5;
            let offsets = (b * v + c, e * v + f, h * v + i);
            let (across, down) = (
                across.next().unwrap_or_default(),
                down.next().unwrap_or_default(),
            );
            // Sample at pixel centers.
            let position = |x: u32| {
                let u = f64::from(x) + 0.5;
                let w = g * u + offsets.2;
                let (x, y) = ((a * u + offsets.0) / w, (d * u + offsets.1) / w);
                let inside = (w > 0.0) & (0.0 <= x) & (x < width) & (0.0 <= y) & (y < height);
                (x, y, inside)
            };
            if BILINEAR {
                for ((((column, row), across), down), x) in columns
                    .iter_mut()
                    .zip(rows.iter_mut())
                    .zip(across.iter_mut())
                    .zip(down.iter_mut())
                    .zip(0..)
                {
                    let (x, y, inside) = position(x);
                    let ((left, wx), (top, wy)) = (split(x, width - 1.0), split(y, height - 1.0));
                    *column = if inside { left as u16 } else { OUTSIDE };
                    (*row, *across, *down) = (top as u16, wx as u8, wy as u8);
                }
            } else {
                for ((column, row), x) in columns.iter_mut().zip(rows.iter_mut()).zip(0..) {
                    let (x, y, inside) = position(x);
                    *column = if inside { x as u16 } else { OUTSIDE };
                    *row = y as u16;
                }
            }
        }
        Ok(samples)
    }

    /// Fill every output pixel, leaving row padding untouched, with up to
    /// `threads` threads each filling bands of rows.
    pub(super) fn apply(
        &self,
        source: &Plane<'_>,
        target: &mut PlaneMut<'_>,
        threads: usize,
    ) -> Result<(), WarpError> {
        for (expected, actual) in [
            (self.source, (source.width, source.height)),
            (self.output, (target.width, target.height)),
        ] {
            if expected != actual {
                return Err(WarpError::Size { expected, actual });
            }
        }
        check_layout(
            source.data.len(),
            source.stride,
            source.width,
            source.height,
        )?;
        let row_len = check_layout(
            target.data.len(),
            target.stride,
            target.width,
            target.height,
        )?;
        // Several bands per thread even out bands that cost more, where
        // more of them falls inside the source.
        let height = usize::try_from(target.height).map_err(|_err| WarpError::Layout)?;
        let threads = threads.clamp(1, height);
        let band = height.div_ceil(if threads == 1 { 1 } else { threads * 4 }.min(height));
        let bands = Mutex::new(
            (0..)
                .step_by(band)
                .zip(target.data.chunks_mut(band * target.stride)),
        );
        let stride = target.stride;
        let work = || loop {
            // Take the next band without holding the lock while filling it.
            let next = bands.lock().unwrap_or_else(PoisonError::into_inner).next();
            let Some((first, band)) = next else {
                break;
            };
            self.fill(source, band, stride, row_len, first);
        };
        std::thread::scope(|scope| {
            for _ in 1..threads {
                // Without more threads, the ones running take the rest.
                if std::thread::Builder::new()
                    .name("wallwarp".to_owned())
                    .spawn_scoped(scope, work)
                    .is_err()
                {
                    break;
                }
            }
            work();
        });
        Ok(())
    }

    /// Fill the output rows in `band`, the first of them row `first`.
    fn fill(
        &self,
        source: &Plane<'_>,
        band: &mut [u8],
        stride: usize,
        row_len: usize,
        first: usize,
    ) {
        let width = row_len / CHANNELS;
        let background = self.background;
        let samples = &self.samples;
        let start = |column: u16, row: u16| {
            (column != OUTSIDE)
                .then(|| usize::from(row) * source.stride + usize::from(column) * CHANNELS)
        };
        let mut batch = Batch::new();
        for (y, row) in (first..).zip(band.chunks_mut(stride)) {
            let Some(row) = row.get_mut(..row_len) else {
                continue;
            };
            let range = y * width..(y + 1) * width;
            let (Some(columns), Some(rows)) = (
                samples.columns.get(range.clone()),
                samples.rows.get(range.clone()),
            ) else {
                continue;
            };
            let pixels = row.as_chunks_mut::<CHANNELS>().0;
            match self.interpolation {
                Interpolation::Nearest => {
                    for ((pixel, column), row) in pixels.iter_mut().zip(columns).zip(rows) {
                        *pixel = start(*column, *row)
                            .and_then(|start| source.texel(start))
                            .unwrap_or(background);
                    }
                }
                Interpolation::Bilinear => {
                    let (Some(across), Some(down)) =
                        (samples.across.get(range.clone()), samples.down.get(range))
                    else {
                        continue;
                    };
                    for ((((pixels, columns), rows), across), down) in pixels
                        .chunks_mut(BATCH)
                        .zip(columns.chunks(BATCH))
                        .zip(rows.chunks(BATCH))
                        .zip(across.chunks(BATCH))
                        .zip(down.chunks(BATCH))
                    {
                        let [tl, tr, bl, br] = &mut batch.corners;
                        for (((((((column, row), tl), tr), bl), br), wx), wy) in columns
                            .iter()
                            .zip(rows)
                            .zip(tl.iter_mut())
                            .zip(tr.iter_mut())
                            .zip(bl.iter_mut())
                            .zip(br.iter_mut())
                            .zip(batch.across.iter_mut().zip(across))
                            .zip(batch.down.iter_mut().zip(down))
                        {
                            // Outside the source, four background pixels
                            // blend to the background. Pixels past the
                            // right or bottom edge have no weight.
                            let start = start(*column, *row);
                            let upper = start.and_then(|start| source.pair(start));
                            let lower = start.and_then(|start| source.pair(start + source.stride));
                            let upper = upper.unwrap_or([background; 2]);
                            [[*tl, *tr], [*bl, *br]] = [upper, lower.unwrap_or(upper)];
                            (*wx.0, *wy.0) =
                                ([u16::from(*wx.1); CHANNELS], [u16::from(*wy.1); CHANNELS]);
                        }
                        blend(&batch, pixels.as_flattened_mut());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use wall_geometry::{Point, Quad};

    use super::*;

    const BACKGROUND: [u8; 4] = [1, 2, 3, 4];
    const PADDING: u8 = 0xee;

    /// A padded plane whose pixel (x, y) is [x, y, x ^ y, 255].
    fn grid(width: u32, height: u32, padding: usize) -> (Vec<u8>, usize) {
        let stride = row_bytes(width).expect("row") + padding;
        let mut data = vec![PADDING; stride * usize::try_from(height).expect("height")];
        for (y, row) in (0u8..).zip(data.chunks_mut(stride)) {
            for (x, pixel) in (0u8..).zip(row.as_chunks_mut::<4>().0).take(width as usize) {
                *pixel = [x, y, x ^ y, 255];
            }
        }
        (data, stride)
    }

    fn warp(
        quad: Quad,
        source: (u32, u32),
        output: (u32, u32),
        interpolation: Interpolation,
    ) -> Warp {
        let homography = Homography::unit_square_to_quad(&quad).expect("valid quad");
        Warp::new(&homography, source, output, interpolation, BACKGROUND).expect("warp")
    }

    fn run(
        warp: &Warp,
        source: (u32, u32),
        output: (u32, u32),
        padding: usize,
    ) -> (Vec<u8>, usize) {
        let (input, input_stride) = grid(source.0, source.1, 3 * 4);
        let stride = row_bytes(output.0).expect("row") + padding;
        let mut data = vec![PADDING; stride * output.1 as usize];
        warp.apply(
            &Plane::new(&input, input_stride, source.0, source.1),
            &mut PlaneMut::new(&mut data, stride, output.0, output.1),
            1,
        )
        .expect("apply");
        (data, stride)
    }

    fn pixel(data: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
        data[y * stride + x * 4..][..4].try_into().expect("pixel")
    }

    /// The per-pixel sampler that the batched kernel replaced, kept as its
    /// specification: pixel-center coordinates, truncation for nearest, and
    /// 8-bit fixed-point weights clamped to the edge for bilinear.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::many_single_char_names,
        reason = "a direct transcription of the sampling rules"
    )]
    fn reference(warp: &Warp, data: &[u8], stride: usize) -> Vec<[u8; 4]> {
        let [[a, b, c], [d, e, f], [g, h, i]] = warp.matrix;
        let (width, height) = warp.source;
        let at = |x: usize, y: usize| pixel(data, stride, x, y);
        let neighbours = |position: f64, size: u32| {
            let fixed = (position * 256.0).floor() as i64;
            let (first, weight) = (fixed.div_euclid(256), fixed.rem_euclid(256) as u32);
            let last = size as usize - 1;
            if first < 0 {
                (0, 0, weight)
            } else {
                let first = (first as usize).min(last);
                (first, (first + 1).min(last), weight)
            }
        };
        let mut pixels = Vec::new();
        for y in 0..warp.output.1 {
            let v = f64::from(y) + 0.5;
            for x in 0..warp.output.0 {
                let u = f64::from(x) + 0.5;
                let w = g * u + (h * v + i);
                let (sx, sy) = ((a * u + (b * v + c)) / w, (d * u + (e * v + f)) / w);
                let inside = w > 0.0
                    && (0.0..f64::from(width)).contains(&sx)
                    && (0.0..f64::from(height)).contains(&sy);
                pixels.push(if !inside {
                    BACKGROUND
                } else if warp.interpolation == Interpolation::Nearest {
                    at(sx as usize, sy as usize)
                } else {
                    let (left, right, wx) = neighbours(sx - 0.5, width);
                    let (top, bottom, wy) = neighbours(sy - 0.5, height);
                    let corners = [
                        at(left, top),
                        at(right, top),
                        at(left, bottom),
                        at(right, bottom),
                    ];
                    std::array::from_fn(|channel| {
                        let [tl, tr, bl, br] = corners.map(|corner| u32::from(corner[channel]));
                        let upper = tl * (256 - wx) + tr * wx;
                        let lower = bl * (256 - wx) + br * wx;
                        ((upper * (256 - wy) + lower * wy + 32768) >> 16) as u8
                    })
                });
            }
        }
        pixels
    }

    #[test]
    fn batches_sample_like_the_reference() {
        // Bytes from a fixed xorshift sequence, so neighbours differ.
        let mut state = 0x2545_f491_u32;
        let mut noise = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        };
        let quads = [
            "0.0417,0.0926,0.4063,0.0833,0.4167,0.4630,0.0469,0.4815",
            "0.7135,0.0556,0.6927,0.8519,0.5521,0.8333,0.5729,0.0278",
            "0.25,0,0.75,0,1,1,0,1",
            "-0.3,-0.2,1.2,0.1,1.4,1.3,-0.1,1.1",
        ];
        let mut homographies: Vec<Homography> = quads
            .iter()
            .map(|quad| {
                Homography::unit_square_to_quad(&quad.parse().expect("quad")).expect("quad")
            })
            .collect();
        homographies.push(
            Homography::from_matrix([[1.0, 0.2, 0.0], [0.1, 1.0, 0.0], [-1.5, 0.3, 1.0]])
                .expect("projective"),
        );
        let sizes = [
            ((37, 23), (41, 29)),
            ((1, 9), (67, 5)),
            ((9, 1), (5, 70)),
            ((1, 1), (130, 3)),
        ];
        for (source, output) in sizes {
            let stride = row_bytes(source.0).expect("row") + 12;
            let data: Vec<u8> = (0..stride * source.1 as usize).map(|_| noise()).collect();
            for homography in &homographies {
                for (interpolation, threads) in [Interpolation::Nearest, Interpolation::Bilinear]
                    .into_iter()
                    .flat_map(|interpolation| [(interpolation, 1), (interpolation, 3)])
                {
                    let warp = Warp::new(homography, source, output, interpolation, BACKGROUND)
                        .expect("warp");
                    let mut target =
                        vec![PADDING; row_bytes(output.0).expect("row") * output.1 as usize];
                    warp.apply(
                        &Plane::new(&data, stride, source.0, source.1),
                        &mut PlaneMut::new(
                            &mut target,
                            row_bytes(output.0).expect("row"),
                            output.0,
                            output.1,
                        ),
                        threads,
                    )
                    .expect("apply");
                    let (pixels, _) = target.as_chunks::<4>();
                    assert!(
                        pixels == reference(&warp, &data, stride),
                        "{interpolation:?} on {threads} threads, {source:?} to {output:?} by {:?}",
                        homography.matrix()
                    );
                }
            }
        }
    }

    #[test]
    fn identity_copies_exactly_with_both_interpolations() {
        for interpolation in [Interpolation::Nearest, Interpolation::Bilinear] {
            let size = (7, 5);
            let warp = warp(Quad::UNIT, size, size, interpolation);
            let (output, stride) = run(&warp, size, size, 8);
            let (expected, expected_stride) = grid(7, 5, 8);
            assert_eq!(
                (output, stride),
                (expected, expected_stride),
                "{interpolation:?}"
            );
        }
    }

    #[test]
    fn row_padding_is_left_untouched() {
        let warp = warp(Quad::UNIT, (4, 4), (3, 5), Interpolation::Bilinear);
        let (output, stride) = run(&warp, (4, 4), (3, 5), 5);
        for row in output.chunks(stride) {
            assert!(row[12..].iter().all(|byte| *byte == PADDING));
        }
    }

    #[test]
    fn bilinear_blends_between_pixel_centers() {
        let homography = Homography::IDENTITY;
        let warp = Warp::new(
            &homography,
            (2, 1),
            (4, 1),
            Interpolation::Bilinear,
            BACKGROUND,
        )
        .expect("warp");
        let input = [0, 0, 0, 0, 200, 100, 40, 255];
        let mut output = [0; 16];
        warp.apply(
            &Plane::new(&input, 8, 2, 1),
            &mut PlaneMut::new(&mut output, 16, 4, 1),
            1,
        )
        .expect("apply");
        // Samples at 0.25, 0.75, 1.25, 1.75 source pixels: edges clamp, the
        // middle two blend a quarter and three quarters of the way across.
        assert_eq!(
            output,
            [
                0, 0, 0, 0, 50, 25, 10, 64, 150, 75, 30, 191, 200, 100, 40, 255
            ]
        );
    }

    #[test]
    fn points_behind_the_projection_use_the_background() {
        // w = 1 - 2u is not positive across the right half of the output.
        let homography =
            Homography::from_matrix([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [-2.0, 0.0, 1.0]])
                .expect("projective");
        let warp = Warp::new(
            &homography,
            (4, 4),
            (4, 1),
            Interpolation::Nearest,
            BACKGROUND,
        )
        .expect("warp");
        let (output, stride) = run(&warp, (4, 4), (4, 1), 0);
        assert_ne!(pixel(&output, stride, 0, 0), BACKGROUND);
        assert_eq!(pixel(&output, stride, 2, 0), BACKGROUND);
        assert_eq!(pixel(&output, stride, 3, 0), BACKGROUND);
    }

    #[test]
    fn samples_outside_the_source_use_the_background() {
        let quad = Quad::new([
            Point::new(-0.5, -0.5),
            Point::new(1.5, -0.5),
            Point::new(1.5, 1.5),
            Point::new(-0.5, 1.5),
        ]);
        for interpolation in [Interpolation::Nearest, Interpolation::Bilinear] {
            let warp = warp(quad, (4, 4), (8, 8), interpolation);
            let (output, stride) = run(&warp, (4, 4), (8, 8), 0);
            for y in 0..8 {
                for x in 0..8 {
                    let expected = if (2..6).contains(&x) && (2..6).contains(&y) {
                        let (sx, sy) = (
                            u8::try_from(x - 2).expect("column"),
                            u8::try_from(y - 2).expect("row"),
                        );
                        [sx, sy, sx ^ sy, 255]
                    } else {
                        BACKGROUND
                    };
                    assert_eq!(pixel(&output, stride, x, y), expected, "({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn mismatched_or_short_planes_are_rejected() {
        let warp = warp(Quad::UNIT, (2, 2), (2, 2), Interpolation::Nearest);
        let input = [0; 16];
        let mut output = [0; 16];
        let cases: [(Plane<'_>, (usize, u32, u32), WarpError); 4] = [
            (
                Plane::new(&input, 8, 3, 2),
                (8, 2, 2),
                WarpError::Size {
                    expected: (2, 2),
                    actual: (3, 2),
                },
            ),
            (Plane::new(&input, 4, 2, 2), (8, 2, 2), WarpError::Layout),
            (
                Plane::new(&input[..12], 8, 2, 2),
                (8, 2, 2),
                WarpError::Layout,
            ),
            (Plane::new(&input, 8, 2, 2), (6, 2, 2), WarpError::Layout),
        ];
        for (source, (stride, width, height), error) in cases {
            let mut target = PlaneMut::new(&mut output, stride, width, height);
            assert_eq!(warp.apply(&source, &mut target, 1), Err(error));
        }
        assert!(
            Warp::new(
                &Homography::IDENTITY,
                (0, 1),
                (1, 1),
                Interpolation::Nearest,
                BACKGROUND
            )
            .is_err()
        );
    }
}
