//! Kaldi-compatible log mel filterbank features, as computed by Kaldi's
//! `compute-fbank-feats`, `kaldi-native-fbank` and
//! `torchaudio.compliance.kaldi.fbank` with dither off.
//!
//! Each frame has its DC offset removed, is pre-emphasized, has a Povey
//! window applied, and is zero-padded to a power of two. Its power spectrum
//! then goes through triangular mel filters, and the log is taken. Frames
//! never extend past the input (`snip_edges`), so input shorter than one
//! frame gives no frames.

use std::f64::consts::PI;

/// Filterbank settings. The defaults are those speaker models are trained
/// with: 25 ms frames every 10 ms, 80 mel bins from 20 Hz to the Nyquist
/// frequency, and samples scaled to the 16-bit range.
#[derive(Clone, Debug, PartialEq)]
pub struct FbankConfig {
    pub sample_rate: u32,
    pub frame_length_ms: f32,
    pub frame_shift_ms: f32,
    pub mel_bins: usize,
    pub low_freq: f32,
    /// The upper edge of the filters; zero or negative counts down from the
    /// Nyquist frequency.
    pub high_freq: f32,
    pub preemphasis: f32,
    /// Samples are multiplied by this first; models trained on 16-bit
    /// integer audio expect 32768.
    pub input_scale: f32,
}

impl Default for FbankConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16_000,
            frame_length_ms: 25.0,
            frame_shift_ms: 10.0,
            mel_bins: 80,
            low_freq: 20.0,
            high_freq: 0.0,
            preemphasis: 0.97,
            input_scale: 32_768.0,
        }
    }
}

/// One triangular mel filter: its weights start at FFT bin `first`.
#[derive(Debug)]
struct MelFilter {
    first: usize,
    weights: Vec<f32>,
}

/// A filterbank, with its window, FFT tables and mel filters precomputed.
#[derive(Debug)]
pub struct Fbank {
    config: FbankConfig,
    frame_length: usize,
    frame_shift: usize,
    window: Vec<f32>,
    fft: Fft,
    filters: Vec<MelFilter>,
}

impl Fbank {
    /// Builds a filterbank, or describes why `config` is unusable.
    pub fn new(config: FbankConfig) -> Result<Self, String> {
        let rate = f64::from(config.sample_rate);
        // Kaldi truncates `samp_freq * 0.001f * ms`, computed in f32.
        #[expect(clippy::cast_precision_loss, reason = "Kaldi computes this in f32")]
        let samples = |ms: f32| (config.sample_rate as f32 * 0.001 * ms).trunc();
        let frame_length = samples(config.frame_length_ms);
        let frame_shift = samples(config.frame_shift_ms);
        if !(2.0..=65_536.0).contains(&frame_length) || !(1.0..=65_536.0).contains(&frame_shift) {
            return Err(format!(
                "frames of {} ms every {} ms at {} Hz are not supported",
                config.frame_length_ms, config.frame_shift_ms, config.sample_rate
            ));
        }
        // Both were range-checked above, so these conversions are exact.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "range-checked"
        )]
        let (frame_length, frame_shift) = (frame_length as usize, frame_shift as usize);
        let padded = frame_length.next_power_of_two();
        let nyquist = 0.5 * rate;
        let high = if config.high_freq > 0.0 {
            f64::from(config.high_freq)
        } else {
            nyquist + f64::from(config.high_freq)
        };
        let low = f64::from(config.low_freq);
        if config.mel_bins < 3
            || !low.is_finite()
            || !high.is_finite()
            || low < 0.0
            || high > nyquist
            || low >= high
        {
            return Err(format!(
                "{} mel bins from {low} Hz to {high} Hz are not supported",
                config.mel_bins
            ));
        }
        if !(0.0..=1.0).contains(&config.preemphasis)
            || !config.input_scale.is_finite()
            || config.input_scale <= 0.0
        {
            return Err(format!(
                "pre-emphasis {} and input scale {} are not supported",
                config.preemphasis, config.input_scale
            ));
        }
        let filters = mel_filters(config.mel_bins, padded, rate, low, high);
        if let Some(empty) = filters.iter().position(|filter| filter.weights.is_empty()) {
            return Err(format!(
                "mel bin {empty} of {} covers no FFT bin; use fewer bins or longer frames",
                config.mel_bins
            ));
        }
        Ok(Self {
            window: povey_window(frame_length),
            fft: Fft::new(padded),
            filters,
            frame_length,
            frame_shift,
            config,
        })
    }

    #[must_use]
    pub fn config(&self) -> &FbankConfig {
        &self.config
    }

    #[must_use]
    pub fn mel_bins(&self) -> usize {
        self.config.mel_bins
    }

    /// The number of frames `samples` input samples give.
    #[must_use]
    pub fn frames(&self, samples: usize) -> usize {
        if samples < self.frame_length {
            0
        } else {
            1 + (samples - self.frame_length) / self.frame_shift
        }
    }

    /// Features of `pcm`, as `frames(pcm.len())` rows of `mel_bins()` values.
    /// A non-finite sample makes its frames NaN, as in Kaldi.
    #[must_use]
    pub fn compute(&self, pcm: &[f32]) -> Vec<f32> {
        let frames = self.frames(pcm.len());
        let mut out = Vec::with_capacity(frames * self.config.mel_bins);
        let mut frame = vec![0.0f32; self.frame_length];
        let mut re = vec![0.0f32; self.fft.len];
        let mut im = vec![0.0f32; self.fft.len];
        for chunk in pcm
            .windows(self.frame_length)
            .step_by(self.frame_shift)
            .take(frames)
        {
            for (dst, &src) in frame.iter_mut().zip(chunk) {
                *dst = src * self.config.input_scale;
            }
            #[expect(clippy::cast_precision_loss, reason = "frame lengths are small")]
            let mean = frame.iter().sum::<f32>() / frame.len() as f32;
            for value in &mut frame {
                *value -= mean;
            }
            let coeff = self.config.preemphasis;
            for i in (1..frame.len()).rev() {
                let previous = frame.get(i - 1).copied().unwrap_or(0.0);
                if let Some(value) = frame.get_mut(i) {
                    *value -= coeff * previous;
                }
            }
            if let Some(first) = frame.first_mut() {
                *first -= coeff * *first;
            }
            re.fill(0.0);
            im.fill(0.0);
            for ((dst, &value), &weight) in re.iter_mut().zip(&frame).zip(&self.window) {
                *dst = value * weight;
            }
            self.fft.forward(&mut re, &mut im);
            for filter in &self.filters {
                let energy: f32 = filter
                    .weights
                    .iter()
                    .zip(re.iter().zip(&im).skip(filter.first))
                    .map(|(&w, (&r, &i))| w * (r * r + i * i))
                    .sum();
                // Kaldi's floor; `max` would turn a NaN energy into the floor.
                let floored = if energy < f32::EPSILON {
                    f32::EPSILON
                } else {
                    energy
                };
                out.push(floored.ln());
            }
        }
        out
    }
}

/// Kaldi's Povey window: a Hann window raised to the power 0.85.
fn povey_window(len: usize) -> Vec<f32> {
    #[expect(clippy::cast_precision_loss, reason = "window lengths are small")]
    let step = 2.0 * PI / (len - 1) as f64;
    (0..len)
        .map(|i| {
            #[expect(clippy::cast_precision_loss, reason = "window lengths are small")]
            let x = 0.5 - 0.5 * (step * i as f64).cos();
            #[expect(clippy::cast_possible_truncation, reason = "f32 features")]
            let value = x.powf(0.85) as f32;
            value
        })
        .collect()
}

fn mel(freq: f64) -> f64 {
    1127.0 * (1.0 + freq / 700.0).ln()
}

/// Triangular filters equally spaced on the mel scale between `low` and
/// `high`, over the first `padded / 2` FFT bins (Kaldi leaves out the
/// Nyquist bin).
fn mel_filters(bins: usize, padded: usize, rate: f64, low: f64, high: f64) -> Vec<MelFilter> {
    #[expect(clippy::cast_precision_loss, reason = "FFT sizes are small")]
    let bin_width = rate / padded as f64;
    let (mel_low, mel_high) = (mel(low), mel(high));
    #[expect(clippy::cast_precision_loss, reason = "bin counts are small")]
    let delta = (mel_high - mel_low) / (bins + 1) as f64;
    (0..bins)
        .map(|bin| {
            #[expect(clippy::cast_precision_loss, reason = "bin counts are small")]
            let left = mel_low + bin as f64 * delta;
            let center = left + delta;
            let right = center + delta;
            let mut first = None;
            let mut weights = Vec::new();
            for i in 0..padded / 2 {
                #[expect(clippy::cast_precision_loss, reason = "FFT sizes are small")]
                let m = mel(bin_width * i as f64);
                if m > left && m < right {
                    let weight = if m <= center {
                        (m - left) / (center - left)
                    } else {
                        (right - m) / (right - center)
                    };
                    first.get_or_insert(i);
                    #[expect(clippy::cast_possible_truncation, reason = "f32 features")]
                    weights.push(weight as f32);
                }
            }
            MelFilter {
                first: first.unwrap_or(0),
                weights,
            }
        })
        .collect()
}

/// An in-place radix-2 complex FFT of one power-of-two size.
#[derive(Debug)]
struct Fft {
    len: usize,
    /// `(cos, -sin)` of `2πk/len` for `k < len / 2`.
    twiddles: Vec<(f32, f32)>,
}

impl Fft {
    fn new(len: usize) -> Self {
        let twiddles = (0..len / 2)
            .map(|k| {
                #[expect(clippy::cast_precision_loss, reason = "FFT sizes are small")]
                let angle = 2.0 * PI * k as f64 / len as f64;
                #[expect(clippy::cast_possible_truncation, reason = "f32 transform")]
                let twiddle = (angle.cos() as f32, -angle.sin() as f32);
                twiddle
            })
            .collect();
        Self { len, twiddles }
    }

    fn forward(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.len;
        let bits = n.trailing_zeros();
        if bits == 0 {
            return;
        }
        for i in 0..n {
            let j = i.reverse_bits() >> (usize::BITS - bits);
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut size = 2;
        while size <= n {
            let half = size / 2;
            let stride = n / size;
            for start in (0..n).step_by(size) {
                for k in 0..half {
                    let (Some(&(wr, wi)), Some(a), Some(b)) = (
                        self.twiddles.get(k * stride),
                        re.get(start + k).zip(im.get(start + k)),
                        re.get(start + k + half).zip(im.get(start + k + half)),
                    ) else {
                        continue;
                    };
                    let (ar, ai) = (*a.0, *a.1);
                    let (br, bi) = (*b.0, *b.1);
                    let tr = br * wr - bi * wi;
                    let ti = br * wi + bi * wr;
                    if let (Some(x), Some(y)) = (re.get_mut(start + k), im.get_mut(start + k)) {
                        *x = ar + tr;
                        *y = ai + ti;
                    }
                    if let (Some(x), Some(y)) =
                        (re.get_mut(start + k + half), im.get_mut(start + k + half))
                    {
                        *x = ar - tr;
                        *y = ai - ti;
                    }
                }
            }
            size *= 2;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Fbank, FbankConfig, Fft};

    /// Two tones and a little noise, generated the same way as the
    /// references below by `scripts/reference.py`.
    fn signal(n: usize, rate: u32) -> Vec<f32> {
        let mut state: u32 = 1;
        (0..n)
            .map(|i| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (f64::from(state >> 8) / 16_777_216.0 - 0.5) * 0.1;
                #[expect(clippy::cast_precision_loss, reason = "test signal")]
                let t = i as f64 / f64::from(rate);
                let tau = std::f64::consts::TAU;
                #[expect(clippy::cast_possible_truncation, reason = "test signal")]
                let sample = (0.3 * (tau * 440.0 * t).sin()
                    + 0.2 * (tau * 1250.0 * t + 0.5).sin()
                    + noise) as f32;
                sample
            })
            .collect()
    }

    // `mise run //analytics/diarization:references` prints these. The
    // filterbank is kaldi-native-fbank's with dither 0 and the rest default.
    // kaldi-native-fbank 1.23.1
    // signal(8000) at 16 kHz, 80 bins: 48 frames
    #[rustfmt::skip]
    const FRAME_0: [f32; 80] = [
        11.675, 12.269, 11.97, 10.947, 11.361, 11.709, 11.13, 13.336,
        12.304, 13.655, 16.194, 17.259, 20.12, 23.4, 24.152, 23.117,
        19.486, 16.997, 16.049, 16.217, 16.038, 16.239, 16.287, 17.738,
        18.029, 17.324, 15.994, 16.439, 16.972, 18.487, 22.796, 25.495,
        25.184, 20.259, 18.62, 17.226, 18.413, 18.321, 16.952, 18.861,
        19.263, 18.067, 18.311, 18.023, 18.842, 19.01, 19.645, 19.343,
        20.517, 19.506, 18.675, 19.445, 19.616, 19.458, 19.776, 20.474,
        20.02, 20.718, 21.272, 21.44, 20.298, 19.855, 20.378, 20.914,
        21.722, 20.919, 21.364, 21.55, 21.497, 22.316, 21.272, 21.796,
        20.875, 21.029, 21.496, 22.107, 22.107, 22.015, 22.807, 22.514,
    ];
    #[rustfmt::skip]
    const FRAME_23: [f32; 80] = [
        11.3, 11.317, 12.913, 12.617, 13.013, 14.133, 15.22, 15.777,
        14.485, 12.141, 15.046, 17.531, 20.192, 23.446, 24.203, 23.202,
        19.763, 16.898, 16.111, 15.766, 15.129, 14.475, 15.87, 16.93,
        16.638, 15.607, 17.059, 18.648, 17.64, 17.835, 22.699, 25.46,
        25.189, 20.299, 18.802, 16.968, 18.072, 18.637, 18.074, 18.469,
        19.109, 17.715, 18.507, 19.218, 18.206, 19.399, 20.409, 19.059,
        20.064, 19.747, 21.125, 20.737, 20.45, 20.907, 20.223, 20.317,
        20.196, 20.008, 20.174, 19.743, 21.442, 21.169, 20.719, 21.179,
        20.218, 20.484, 21.466, 21.821, 21.507, 20.254, 21.378, 21.579,
        21.927, 22.12, 22.292, 21.658, 22.308, 22.153, 22.661, 22.631,
    ];
    #[rustfmt::skip]
    const FRAME_47: [f32; 80] = [
        11.965, 12.573, 10.509, 10.794, 10.831, 11.312, 12.412, 13.472,
        14.748, 14.248, 15.046, 17.344, 20.146, 23.409, 24.169, 23.159,
        19.642, 16.78, 15.927, 16.77, 16.915, 16.751, 16.409, 16.319,
        17.181, 17.13, 16.637, 17.282, 16.705, 17.668, 22.641, 25.423,
        25.153, 20.264, 18.097, 17.691, 17.515, 16.055, 18.263, 19.318,
        19.541, 19.014, 19.351, 18.847, 17.765, 18.926, 18.141, 19.049,
        20.748, 20.028, 19.742, 19.943, 19.722, 20.601, 21.807, 20.649,
        19.674, 21.389, 20.535, 19.486, 20.69, 21.329, 22.28, 21.617,
        21.511, 21.547, 21.558, 21.891, 22.383, 22.053, 21.477, 20.99,
        21.582, 22.368, 22.417, 21.683, 22.374, 22.491, 21.871, 21.828,
    ];
    // signal(11025) at 22050 Hz, 64 bins: 48 frames
    #[rustfmt::skip]
    const FRAME_22050_LAST: [f32; 64] = [
        11.85, 13.613, 14.103, 14.342, 15.264, 13.765, 14.731, 17.011,
        20.365, 24.135, 24.69, 22.442, 16.672, 16.573, 16.906, 16.853,
        17.252, 17.253, 17.095, 18.315, 18.606, 23.032, 26.126, 25.05,
        18.88, 18.242, 19.149, 18.243, 17.851, 18.697, 19.326, 19.299,
        19.609, 20.009, 19.831, 21.051, 19.427, 19.311, 20.332, 20.708,
        21.23, 21.478, 20.794, 21.438, 22.099, 21.702, 20.959, 21.774,
        22.457, 22.111, 22.26, 22.98, 22.425, 22.325, 22.544, 22.851,
        23.431, 23.612, 23.419, 23.293, 23.56, 23.642, 23.656, 23.707,
    ];

    /// Checks `row` against a reference printed to five significant digits.
    fn assert_close(row: &[f32], expected: &[f32], what: &str) {
        assert_eq!(row.len(), expected.len(), "{what}");
        for (bin, (&got, &want)) in row.iter().zip(expected).enumerate() {
            assert!(
                (got - want).abs() < 0.01,
                "{what} bin {bin}: {got} != {want}"
            );
        }
    }

    #[test]
    fn matches_kaldi_native_fbank() {
        let fbank = Fbank::new(FbankConfig::default()).expect("valid");
        let features = fbank.compute(&signal(8000, 16_000));
        assert_eq!(features.len(), 48 * 80);
        for (frame, expected) in [(0, &FRAME_0), (23, &FRAME_23), (47, &FRAME_47)] {
            let row = features.get(frame * 80..(frame + 1) * 80).expect("frame");
            assert_close(row, expected, &format!("frame {frame}"));
        }
    }

    #[test]
    fn truncates_frame_sizes_like_kaldi() {
        // At 22050 Hz, 25 ms is 551.25 samples and 10 ms 220.5: Kaldi
        // truncates both, so the last frame starts at 47 * 220.
        let fbank = Fbank::new(FbankConfig {
            sample_rate: 22_050,
            mel_bins: 64,
            ..FbankConfig::default()
        })
        .expect("valid");
        assert_eq!(fbank.frames(551), 1);
        assert_eq!(fbank.frames(770), 1);
        assert_eq!(fbank.frames(771), 2);
        let features = fbank.compute(&signal(11_025, 22_050));
        assert_eq!(features.len(), 48 * 64);
        let last = features.get(47 * 64..).expect("last frame");
        assert_close(last, &FRAME_22050_LAST, "last frame");
    }

    #[test]
    fn non_finite_samples_give_nan_frames() {
        let fbank = Fbank::new(FbankConfig::default()).expect("valid");
        let mut pcm = signal(720, 16_000);
        if let Some(sample) = pcm.get_mut(500) {
            *sample = f32::NAN;
        }
        // Frame 0 (samples 0..400) is clean; frame 2 (320..720) holds the NaN.
        let features = fbank.compute(&pcm);
        assert!(
            features
                .get(..80)
                .expect("frame 0")
                .iter()
                .all(|v| v.is_finite())
        );
        assert!(
            features
                .get(160..)
                .expect("frame 2")
                .iter()
                .all(|v| v.is_nan())
        );
    }

    #[test]
    fn counts_frames_like_kaldi() {
        let fbank = Fbank::new(FbankConfig::default()).expect("valid");
        assert_eq!(fbank.frames(0), 0);
        assert_eq!(fbank.frames(399), 0);
        assert_eq!(fbank.frames(400), 1);
        assert_eq!(fbank.frames(559), 1);
        assert_eq!(fbank.frames(560), 2);
        assert_eq!(fbank.frames(16_000), 98);
        assert_eq!(fbank.compute(&[0.0; 399]), Vec::<f32>::new());
        // Silence hits the log floor rather than minus infinity.
        assert!(fbank.compute(&[0.0; 400]).iter().all(|v| v.is_finite()));
    }

    #[test]
    fn rejects_unusable_settings() {
        let config = |change: fn(&mut FbankConfig)| {
            let mut config = FbankConfig::default();
            change(&mut config);
            Fbank::new(config)
        };
        for change in [
            (|c: &mut FbankConfig| c.frame_length_ms = 0.0) as fn(&mut FbankConfig),
            |c| c.frame_shift_ms = 0.0,
            |c| c.mel_bins = 2,
            |c| c.low_freq = 9000.0,
            |c| c.high_freq = 9000.0,
            |c| c.low_freq = f32::NAN,
            |c| c.high_freq = f32::NAN,
            |c| c.frame_length_ms = f32::NAN,
            |c| c.preemphasis = f32::NAN,
            |c| c.preemphasis = 1.5,
            |c| c.input_scale = 0.0,
            |c| c.input_scale = f32::INFINITY,
            // 128 bins at 16 kHz with 25 ms frames leave a filter empty.
            |c| c.mel_bins = 128,
        ] {
            config(change).expect_err("unusable");
        }
    }

    #[test]
    fn fft_matches_a_direct_transform() {
        let n = 16;
        let fft = Fft::new(n);
        #[expect(clippy::cast_precision_loss, reason = "test signal")]
        let input: Vec<f32> = (0..n).map(|i| ((i * 7 % 5) as f32) - 2.0).collect();
        let (mut re, mut im) = (input.clone(), vec![0.0; n]);
        fft.forward(&mut re, &mut im);
        for k in 0..n {
            let (mut r, mut i) = (0.0f64, 0.0f64);
            for (t, &x) in input.iter().enumerate() {
                #[expect(clippy::cast_precision_loss, reason = "test signal")]
                let angle = -std::f64::consts::TAU * (k * t) as f64 / n as f64;
                r += f64::from(x) * angle.cos();
                i += f64::from(x) * angle.sin();
            }
            let (got_r, got_i) = (re.get(k).copied(), im.get(k).copied());
            assert!((f64::from(got_r.expect("bin")) - r).abs() < 1e-4, "re {k}");
            assert!((f64::from(got_i.expect("bin")) - i).abs() < 1e-4, "im {k}");
        }
    }
}
