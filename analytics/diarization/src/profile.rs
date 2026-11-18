//! Model profiles: how to feed a speaker model and how to read its scores.
//!
//! A profile is listed here only after it has been qualified: its features
//! match the model's reference front-end, and its separation, speed and
//! default thresholds were measured.
//!
//! 3D-Speaker `CAM++` is not listed yet: its `AveragePool` with `ceil_mode`
//! runs differently in the ONNX Runtime that `ort` 2.0.0-rc.13 bundles (1.28)
//! than in training (1.29 and later match it).

use crate::fbank::FbankConfig;

/// What turns a cue's audio into model input.
#[derive(Clone, Debug, PartialEq)]
pub enum FrontEnd {
    /// Kaldi log mel filterbank features, `(1, frames, mel_bins)`, with each
    /// bin's mean over the model's input (a segment, or a part of a long
    /// one) subtracted when `mean_normalize` is set.
    KaldiFbank {
        config: FbankConfig,
        mean_normalize: bool,
    },
}

/// Cosine similarity thresholds for one model's L2-normalized embeddings.
/// Each model spreads its scores differently, so they belong to the profile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    /// A cue at least this close to a speaker is that speaker.
    pub matching: f32,
    /// A session speaker at least this close to a stored profile is that
    /// profile's speaker.
    pub bind: f32,
    /// A cue at least this close to a stored profile updates it.
    pub update: f32,
}

/// A speaker embedding model: one segment of audio in, one vector out.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub name: String,
    pub front_end: FrontEnd,
    /// The model's input and output names.
    pub input: String,
    pub output: String,
    /// The shortest segment, in samples at `sample_rate()`, the model gives
    /// a usable embedding for. Shorter input can produce NaN rather than an
    /// error.
    pub min_samples: usize,
    /// Longer segments are embedded in parts of at most this many samples,
    /// and the parts' embeddings averaged.
    pub max_samples: usize,
    pub thresholds: Thresholds,
    /// SHA-256 of the model files this profile was qualified with.
    pub checksums: Vec<String>,
}

impl Profile {
    /// The sample rate the model takes.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        match &self.front_end {
            FrontEnd::KaldiFbank { config, .. } => config.sample_rate,
        }
    }
}

/// `WeSpeaker` `ResNet34-LM` trained on `VoxCeleb`, 256-dimensional (the
/// embedding model of pyannote's diarization pipelines).
#[must_use]
pub fn wespeaker_resnet34() -> Profile {
    Profile {
        name: "wespeaker-resnet34".to_owned(),
        front_end: FrontEnd::KaldiFbank {
            config: FbankConfig::default(),
            mean_normalize: true,
        },
        input: "feats".to_owned(),
        output: "embs".to_owned(),
        // NaN below ten frames; half a second keeps clear of that.
        min_samples: 8_000,
        max_samples: 160_000,
        // Matching was measured on cue clustering: 0.35 merged two speakers
        // of a two-person recording whose different-speaker cosines reach
        // 0.49, while 0.40 kept read-speech conversations as they were.
        // Recalibrated on more real audio before they are relied on.
        thresholds: Thresholds {
            matching: 0.4,
            bind: 0.6,
            update: 0.6,
        },
        checksums: vec![
            "7bb2f06e9df17cdf1ef14ee8a15ab08ed28e8d0ef5054ee135741560df2ec068".to_owned(),
        ],
    }
}

/// Every built-in profile, the default first.
#[must_use]
pub fn builtin() -> Vec<Profile> {
    vec![wespeaker_resnet34()]
}

/// The built-in profile called `name`.
#[must_use]
pub fn by_name(name: &str) -> Option<Profile> {
    builtin().into_iter().find(|profile| profile.name == name)
}

/// The built-in profile qualified with the model file whose SHA-256 is
/// `checksum` (lowercase hex).
#[must_use]
pub fn by_checksum(checksum: &str) -> Option<Profile> {
    builtin()
        .into_iter()
        .find(|profile| profile.checksums.iter().any(|known| known == checksum))
}

#[cfg(test)]
mod tests {
    use super::{builtin, by_checksum, by_name};

    #[test]
    fn profiles_are_found_by_name_and_checksum() {
        let names: Vec<_> = builtin().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["wespeaker-resnet34"]);
        for profile in builtin() {
            assert_eq!(by_name(&profile.name).as_ref(), Some(&profile));
            for checksum in &profile.checksums {
                assert_eq!(by_checksum(checksum).as_ref(), Some(&profile));
                assert_eq!(checksum.len(), 64);
            }
            assert!(profile.min_samples < profile.max_samples);
            assert_eq!(profile.sample_rate(), 16_000);
        }
        assert!(by_name("x").is_none());
        assert!(by_checksum("00").is_none());
    }
}
