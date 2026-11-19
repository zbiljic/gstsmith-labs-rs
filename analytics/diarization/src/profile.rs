//! Model profiles: how to feed a speaker model and how to read its scores.
//!
//! A profile is listed here only after it has been qualified: its features
//! match the model's reference front-end, and its separation, speed and
//! default thresholds were measured.
//!
//! 3D-Speaker `CAM++` is not listed yet: its `AveragePool` with `ceil_mode`
//! runs differently in the ONNX Runtime that `ort` 2.0.0-rc.13 bundles (1.28)
//! than in training (1.29 and later match it).
//!
//! Any other model whose front-end is built in runs with a `custom` profile
//! read from a TOML file ([`from_toml`]).

use serde::Deserialize;

use crate::fbank::{Fbank, FbankConfig};

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
/// An infinite threshold is never reached.
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

/// The names a `custom` profile cannot take: the element's other
/// `model-profile` values.
pub const RESERVED: [&str; 2] = ["auto", "custom"];

/// The highest `sample-rate` a `custom` profile may take.
const MAX_SAMPLE_RATE: u32 = 384_000;

/// A `custom` profile as written in TOML. Durations are in milliseconds,
/// like the element's properties.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct Config {
    name: String,
    input: String,
    output: String,
    min_duration: u32,
    max_duration: u32,
    match_threshold: f32,
    #[serde(default)]
    sha256: Vec<String>,
    front_end: FrontEndConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum FrontEndKind {
    KaldiFbank,
}

/// The front-end table. Unset fields take [`FbankConfig::default`]'s values;
/// `mean-normalize` defaults to off.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct FrontEndConfig {
    kind: FrontEndKind,
    sample_rate: Option<u32>,
    frame_length: Option<f32>,
    frame_shift: Option<f32>,
    mel_bins: Option<usize>,
    low_freq: Option<f32>,
    high_freq: Option<f32>,
    preemphasis: Option<f32>,
    input_scale: Option<f32>,
    #[serde(default)]
    mean_normalize: bool,
}

impl FrontEndConfig {
    fn front_end(self) -> FrontEnd {
        let FrontEndKind::KaldiFbank = self.kind;
        let defaults = FbankConfig::default();
        FrontEnd::KaldiFbank {
            config: FbankConfig {
                sample_rate: self.sample_rate.unwrap_or(defaults.sample_rate),
                frame_length_ms: self.frame_length.unwrap_or(defaults.frame_length_ms),
                frame_shift_ms: self.frame_shift.unwrap_or(defaults.frame_shift_ms),
                mel_bins: self.mel_bins.unwrap_or(defaults.mel_bins),
                low_freq: self.low_freq.unwrap_or(defaults.low_freq),
                high_freq: self.high_freq.unwrap_or(defaults.high_freq),
                preemphasis: self.preemphasis.unwrap_or(defaults.preemphasis),
                input_scale: self.input_scale.unwrap_or(defaults.input_scale),
            },
            mean_normalize: self.mean_normalize,
        }
    }
}

/// Reads a `custom` profile from TOML, or describes why it is unusable:
///
/// ```toml
/// name = "my-speaker-model"
/// input = "feats"
/// output = "embs"
/// min-duration = 500
/// max-duration = 10000
/// match-threshold = 0.4
///
/// [front-end]
/// kind = "kaldi-fbank"
/// mean-normalize = true
/// ```
///
/// Its stored-profile thresholds are infinite: stored profiles are not
/// bound or updated with a `custom` model.
pub fn from_toml(text: &str) -> Result<Profile, String> {
    let config: Config = toml::from_str(text).map_err(|err| {
        // The message alone does not always name the key; its line does.
        // A missing key spans the whole document, which has no one line.
        let line = err
            .span()
            .filter(|span| span.start > 0)
            .and_then(|span| text.get(..span.start))
            .map_or(0, |before| {
                before.bytes().filter(|&byte| byte == b'\n').count() + 1
            });
        if line > 0 {
            format!("line {line}: {}", err.message())
        } else {
            err.message().to_owned()
        }
    })?;
    let alphanumeric = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let name_ok = (1..=64).contains(&config.name.len())
        && config.name.bytes().next().is_some_and(alphanumeric)
        && config
            .name
            .bytes()
            .all(|byte| alphanumeric(byte) || b"._-".contains(&byte));
    if !name_ok {
        return Err(format!(
            "name {:?} must be 1 to 64 of a-z, 0-9, '.', '_' and '-', starting with a letter or digit",
            config.name
        ));
    }
    if RESERVED.contains(&config.name.as_str()) {
        return Err(format!("name {} is a model-profile value", config.name));
    }
    if by_name(&config.name).is_some() {
        return Err(format!(
            "name {} is taken by a built-in profile",
            config.name
        ));
    }
    if !(config.match_threshold > 0.0 && config.match_threshold <= 1.0) {
        return Err(format!(
            "match-threshold {} is not in (0, 1]",
            config.match_threshold
        ));
    }
    let mut checksums = Vec::with_capacity(config.sha256.len());
    for checksum in config.sha256 {
        let checksum = checksum.to_ascii_lowercase();
        if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("sha256 {checksum} is not 64 hex digits"));
        }
        checksums.push(checksum);
    }
    let front_end = config.front_end.front_end();
    let FrontEnd::KaldiFbank { config: fbank, .. } = &front_end;
    if fbank.sample_rate > MAX_SAMPLE_RATE {
        return Err(format!(
            "sample-rate {} is above {MAX_SAMPLE_RATE}",
            fbank.sample_rate
        ));
    }
    let rate = u64::from(fbank.sample_rate);
    let fbank = Fbank::new(fbank.clone()).map_err(|err| format!("front-end: {err}"))?;
    let samples = |ms: u32| usize::try_from(rate * u64::from(ms) / 1000).unwrap_or(usize::MAX);
    let (min_samples, max_samples) = (samples(config.min_duration), samples(config.max_duration));
    if min_samples == 0 || fbank.frames(min_samples) == 0 {
        return Err(format!(
            "min-duration {} ms is shorter than one frame",
            config.min_duration
        ));
    }
    if max_samples / 4 < min_samples {
        return Err(format!(
            "max-duration {} ms must be at least four times min-duration {} ms",
            config.max_duration, config.min_duration
        ));
    }
    Ok(Profile {
        name: config.name,
        front_end,
        input: config.input,
        output: config.output,
        min_samples,
        max_samples,
        thresholds: Thresholds {
            matching: config.match_threshold,
            bind: f32::INFINITY,
            update: f32::INFINITY,
        },
        checksums,
    })
}

#[cfg(test)]
mod tests {
    use super::{FrontEnd, builtin, by_checksum, by_name, from_toml, wespeaker_resnet34};

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

    /// The built-in profile written as a `custom` one.
    const WESPEAKER: &str = r#"
name = "wespeaker-custom"
input = "feats"
output = "embs"
min-duration = 500
max-duration = 10000
match-threshold = 0.4
sha256 = ["7BB2F06E9DF17CDF1EF14EE8A15AB08ED28E8D0EF5054EE135741560DF2EC068"]

[front-end]
kind = "kaldi-fbank"
sample-rate = 16000
frame-length = 25
frame-shift = 10.0
mel-bins = 80
low-freq = 20
high-freq = 0
preemphasis = 0.97
input-scale = 32768
mean-normalize = true
"#;

    #[test]
    fn a_custom_profile_can_repeat_a_builtin_one() {
        let custom = from_toml(WESPEAKER).expect("usable");
        let builtin = wespeaker_resnet34();
        assert_eq!(custom.name, "wespeaker-custom");
        assert_eq!(custom.front_end, builtin.front_end);
        assert_eq!(
            (&custom.input, &custom.output),
            (&builtin.input, &builtin.output)
        );
        assert_eq!(
            (custom.min_samples, custom.max_samples),
            (builtin.min_samples, builtin.max_samples)
        );
        assert!((custom.thresholds.matching - builtin.thresholds.matching).abs() < f32::EPSILON);
        assert!(custom.thresholds.bind.is_infinite() && custom.thresholds.update.is_infinite());
        assert_eq!(custom.checksums, builtin.checksums);
    }

    #[test]
    fn a_custom_profile_leaves_the_front_end_defaults() {
        let minimal = r#"
name = "m"
input = "x"
output = "y"
min-duration = 300
max-duration = 6000
match-threshold = 0.5
front-end = { kind = "kaldi-fbank" }
"#;
        let profile = from_toml(minimal).expect("usable");
        let FrontEnd::KaldiFbank {
            config,
            mean_normalize,
        } = &profile.front_end;
        assert_eq!(*config, crate::fbank::FbankConfig::default());
        assert!(!mean_normalize);
        assert_eq!((profile.min_samples, profile.max_samples), (4_800, 96_000));
        assert_eq!(profile.checksums, Vec::<String>::new());
    }

    #[test]
    fn every_front_end_key_has_its_effect() {
        let text = r#"
name = "eight"
input = "x"
output = "y"
min-duration = 500
max-duration = 2500
match-threshold = 0.3
front-end.kind = "kaldi-fbank"
front-end.sample-rate = 8000
front-end.frame-length = 32
front-end.frame-shift = 16
front-end.mel-bins = 40
front-end.low-freq = 60
front-end.high-freq = -200
front-end.preemphasis = 0.5
front-end.input-scale = 1
front-end.mean-normalize = true
"#;
        let profile = from_toml(text).expect("usable");
        let expected = crate::fbank::FbankConfig {
            sample_rate: 8_000,
            frame_length_ms: 32.0,
            frame_shift_ms: 16.0,
            mel_bins: 40,
            low_freq: 60.0,
            high_freq: -200.0,
            preemphasis: 0.5,
            input_scale: 1.0,
        };
        assert_eq!(
            profile.front_end,
            FrontEnd::KaldiFbank {
                config: expected,
                mean_normalize: true
            }
        );
        assert_eq!(profile.sample_rate(), 8_000);
        assert_eq!((profile.min_samples, profile.max_samples), (4_000, 20_000));
    }

    /// The README's example is the built-in profile written out.
    #[test]
    fn the_readme_example_is_the_builtin_profile() {
        let readme = include_str!("../README.md");
        let (_, rest) = readme.split_once("```toml\n").expect("a toml block");
        let (text, _) = rest.split_once("```").expect("its end");
        let mut custom = from_toml(text).expect("usable");
        assert_eq!(custom.name, "wespeaker-copy");
        let builtin = wespeaker_resnet34();
        assert!((custom.thresholds.matching - builtin.thresholds.matching).abs() < f32::EPSILON);
        // The rest is the built-in profile's, field for field.
        custom.name.clone_from(&builtin.name);
        custom.thresholds = builtin.thresholds;
        assert_eq!(custom, builtin);
    }

    #[test]
    fn unusable_custom_profiles_are_refused() {
        let with = |from: &str, to: &str| {
            let text = WESPEAKER.replacen(from, to, 1);
            assert_ne!(text, WESPEAKER, "{from}");
            from_toml(&text).expect_err(to)
        };
        for (from, to, error) in [
            ("wespeaker-custom", "wespeaker-resnet34", "taken"),
            ("wespeaker-custom", "custom", "model-profile value"),
            ("wespeaker-custom", "Big Name", "must be 1 to 64"),
            ("wespeaker-custom", "..", "must be 1 to 64"),
            ("min-duration = 500", "min-duration = \"500\"", "line 5"),
            (
                "sample-rate = 16000",
                "sample-rate = 1000000",
                "sample-rate",
            ),
            ("input = \"feats\"\n", "", "input"),
            ("mel-bins", "mel-bands", "mel-bands"),
            ("kaldi-fbank", "nemo-mel", "nemo-mel"),
            ("= 0.4", "= 0", "match-threshold"),
            ("= 0.4", "= 1.5", "match-threshold"),
            ("7BB2F06E", "7BB2F06", "sha256"),
            (
                "min-duration = 500",
                "min-duration = 10",
                "shorter than one frame",
            ),
            ("max-duration = 10000", "max-duration = 1000", "four times"),
            ("mel-bins = 80", "mel-bins = 2", "front-end"),
        ] {
            let message = with(from, to);
            assert!(message.contains(error), "{to}: {message}");
        }
    }
}
