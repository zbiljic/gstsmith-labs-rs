//! Speaker embeddings of audio segments with ONNX Runtime.

use std::fmt;
use std::io::Read as _;
use std::ops::Range;
use std::path::{Path, PathBuf};

use ort::session::Session;
use ort::value::Tensor;
use sha2::{Digest as _, Sha256};

use crate::fbank::Fbank;
use crate::profile::{self, FrontEnd, Profile};

/// Why a model could not be loaded or a segment embedded.
///
/// `TooShort`, `InvalidAudio` and `NoEmbedding` concern one segment: its
/// speaker is unknown, and the model can still embed others. The rest mean
/// the model or profile is unusable.
#[derive(Debug)]
pub enum Error {
    /// The model file could not be read.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// No built-in profile has this name.
    UnknownProfile(String),
    /// The model file is not one a built-in profile was qualified with.
    UnknownModel { checksum: String },
    /// The model or profile is unusable.
    Model(String),
    /// The segment is shorter than the profile's minimum.
    TooShort { samples: usize, min: usize },
    /// The segment holds a NaN or infinite sample.
    InvalidAudio { index: usize },
    /// The model gave no usable embedding for this segment (zero or
    /// non-finite).
    NoEmbedding,
}

impl Error {
    /// Whether the error concerns one segment rather than the model.
    #[must_use]
    pub fn is_segment_error(&self) -> bool {
        matches!(
            self,
            Self::TooShort { .. } | Self::InvalidAudio { .. } | Self::NoEmbedding
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "reading {}: {source}", path.display()),
            Self::UnknownProfile(name) => {
                let known: Vec<_> = profile::builtin().into_iter().map(|p| p.name).collect();
                write!(f, "no model profile {name}; known: {}", known.join(", "))
            }
            Self::UnknownModel { checksum } => write!(
                f,
                "the model (SHA-256 {checksum}) is not one a profile was qualified with; name a profile"
            ),
            Self::Model(detail) => f.write_str(detail),
            Self::TooShort { samples, min } => {
                write!(f, "{samples} samples are fewer than the model's {min}")
            }
            Self::InvalidAudio { index } => write!(f, "sample {index} is not a finite number"),
            Self::NoEmbedding => f.write_str("the model returned no usable embedding"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl<R> From<ort::Error<R>> for Error {
    fn from(err: ort::Error<R>) -> Self {
        Self::Model(err.to_string())
    }
}

/// SHA-256 of a file, as lowercase hex.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = match file.read(&mut buffer) {
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        };
        let Some(chunk) = buffer.get(..read).filter(|chunk| !chunk.is_empty()) else {
            break;
        };
        hasher.update(chunk);
    }
    Ok(hasher
        .finalize()
        .iter()
        .flat_map(|byte| [byte >> 4, byte & 0xf])
        .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
        .collect())
}

/// The built-in profile for a model whose SHA-256 is `checksum`: the one
/// named `name`, or with `None` the one qualified with this exact file.
pub fn profile_for(name: Option<&str>, checksum: &str) -> Result<Profile, Error> {
    match name {
        Some(name) => profile::by_name(name).ok_or_else(|| Error::UnknownProfile(name.to_owned())),
        None => profile::by_checksum(checksum).ok_or_else(|| Error::UnknownModel {
            checksum: checksum.to_owned(),
        }),
    }
}

/// Splits `len` samples into consecutive parts of at most `max` samples,
/// whose lengths differ by at most one. With `max >= 4 * min` and
/// `len >= min`, every part holds at least `min` samples: a part of several
/// is at least `max / 2` long, less one sample.
fn parts(len: usize, max: usize) -> impl Iterator<Item = Range<usize>> {
    let count = len.div_ceil(max.max(1));
    let (part, longer) = (len / count.max(1), len % count.max(1));
    (0..count).map(move |i| {
        let start = i * part + i.min(longer);
        start..start + part + usize::from(i < longer)
    })
}

/// Low-level noise from a fixed LCG: input every model can embed, unlike
/// digital silence, which some normalizations turn into NaN.
fn probe(len: usize) -> Vec<f32> {
    let mut state: u32 = 1;
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            #[expect(clippy::cast_precision_loss, reason = "24 bits fit an f32")]
            let unit = (state >> 8) as f32 / 16_777_216.0;
            (unit - 0.5) * 0.02
        })
        .collect()
}

/// A model that turns one segment of audio into one speaker embedding.
pub trait SegmentEmbedder: Send {
    /// The profile the model runs with.
    fn profile(&self) -> &Profile;
    /// The L2-normalized embedding of one segment of mono PCM at the
    /// profile's sample rate.
    fn embed(&mut self, pcm: &[f32]) -> Result<Vec<f32>, Error>;
}

impl SegmentEmbedder for Embedder {
    fn profile(&self) -> &Profile {
        Self::profile(self)
    }

    fn embed(&mut self, pcm: &[f32]) -> Result<Vec<f32>, Error> {
        Self::embed(self, pcm)
    }
}

/// A loaded speaker model.
pub struct Embedder {
    session: Session,
    profile: Profile,
    checksum: String,
    fbank: Fbank,
    mean_normalize: bool,
    dim: usize,
}

impl fmt::Debug for Embedder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Embedder")
            .field("profile", &self.profile.name)
            .field("checksum", &self.checksum)
            .field("dim", &self.dim)
            .finish_non_exhaustive()
    }
}

impl Embedder {
    /// Loads `model` with the built-in profile `profile`, or with `None` the
    /// one qualified with this exact file. `threads` 0 leaves the thread
    /// count to ONNX Runtime.
    pub fn load(model: &Path, profile: Option<&str>, threads: usize) -> Result<Self, Error> {
        let checksum = checksum(model)?;
        let profile = profile_for(profile, &checksum)?;
        Self::with_checksum(model, profile, checksum, threads)
    }

    /// Loads `model` with `profile`, which need not be built in.
    pub fn new(model: &Path, profile: Profile, threads: usize) -> Result<Self, Error> {
        let checksum = checksum(model)?;
        Self::with_checksum(model, profile, checksum, threads)
    }

    /// Loads the model and embeds a probe once, to check that the model takes
    /// the profile's input and gives a `(1, D)` or `(D)` output, and to learn
    /// `D`.
    fn with_checksum(
        model: &Path,
        profile: Profile,
        checksum: String,
        threads: usize,
    ) -> Result<Self, Error> {
        // Parts of a long segment are longer than half the maximum, less a
        // sample per part, so a maximum of four times the minimum keeps
        // every part above the minimum.
        if profile.min_samples == 0 || profile.max_samples / 4 < profile.min_samples {
            return Err(Error::Model(format!(
                "profile {} has unusable segment limits",
                profile.name
            )));
        }
        let FrontEnd::KaldiFbank {
            config,
            mean_normalize,
        } = profile.front_end.clone();
        let fbank = Fbank::new(config).map_err(Error::Model)?;
        if fbank.frames(profile.min_samples) == 0 {
            return Err(Error::Model(format!(
                "profile {}: its shortest segment gives no frames",
                profile.name
            )));
        }
        // Threads that spin between runs would hold cores busy in a pipeline
        // that embeds a cue every few seconds.
        let mut builder = Session::builder()?
            .with_inter_threads(1)?
            .with_intra_op_spinning(false)?;
        if threads > 0 {
            builder = builder.with_intra_threads(threads)?;
        }
        let session = builder.commit_from_file(model)?;
        for (kind, names, wanted) in [
            ("input", session.inputs(), &profile.input),
            ("output", session.outputs(), &profile.output),
        ] {
            if !names.iter().any(|outlet| outlet.name() == wanted.as_str()) {
                return Err(Error::Model(format!(
                    "the model has no {kind} {wanted} that profile {} needs",
                    profile.name
                )));
            }
        }
        let mut embedder = Self {
            session,
            fbank,
            mean_normalize,
            dim: 0,
            checksum,
            profile,
        };
        let probe = probe(embedder.profile.min_samples);
        embedder.dim = match embedder.run(&probe) {
            Ok(embedding) => embedding.len(),
            Err(Error::NoEmbedding) => {
                return Err(Error::Model(
                    "the model returned no usable embedding for a test signal".to_owned(),
                ));
            }
            Err(err) => return Err(err),
        };
        Ok(embedder)
    }

    #[must_use]
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// SHA-256 of the model file, as lowercase hex.
    #[must_use]
    pub fn checksum(&self) -> &str {
        &self.checksum
    }

    /// Whether the profile was qualified with this exact model file.
    #[must_use]
    pub fn is_qualified(&self) -> bool {
        self.profile.checksums.contains(&self.checksum)
    }

    /// The length of the embeddings.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// The L2-normalized embedding of one segment of mono PCM in [-1, 1] at
    /// the profile's sample rate. A segment longer than the profile's
    /// maximum is embedded in equal parts, each mean-normalized on its own,
    /// whose embeddings are averaged by length.
    pub fn embed(&mut self, pcm: &[f32]) -> Result<Vec<f32>, Error> {
        let min = self.profile.min_samples;
        if pcm.len() < min {
            return Err(Error::TooShort {
                samples: pcm.len(),
                min,
            });
        }
        if let Some(index) = pcm.iter().position(|sample| !sample.is_finite()) {
            return Err(Error::InvalidAudio { index });
        }
        let mut sum = vec![0.0f32; self.dim];
        for range in parts(pcm.len(), self.profile.max_samples) {
            let part = pcm.get(range).unwrap_or_default();
            let embedding = self.run(part)?;
            #[expect(clippy::cast_precision_loss, reason = "a weight")]
            let weight = part.len() as f32;
            for (total, value) in sum.iter_mut().zip(embedding) {
                *total += weight * value;
            }
        }
        normalize(&mut sum)?;
        Ok(sum)
    }

    /// The normalized embedding of `pcm`, checked to be finite.
    fn run(&mut self, pcm: &[f32]) -> Result<Vec<f32>, Error> {
        let mut features = self.fbank.compute(pcm);
        let bins = self.fbank.mel_bins();
        let frames = features.len() / bins;
        if self.mean_normalize && frames > 0 {
            let mut means = vec![0.0f32; bins];
            for row in features.chunks_exact(bins) {
                for (mean, value) in means.iter_mut().zip(row) {
                    *mean += value;
                }
            }
            #[expect(clippy::cast_precision_loss, reason = "frame counts are small")]
            let count = frames as f32;
            for row in features.chunks_exact_mut(bins) {
                for (value, mean) in row.iter_mut().zip(&means) {
                    *value -= mean / count;
                }
            }
        }
        let input = Tensor::from_array(([1, frames, bins], features))?;
        let outputs = self
            .session
            .run(ort::inputs![self.profile.input.as_str() => input])?;
        let output = outputs
            .get(self.profile.output.as_str())
            .ok_or_else(|| Error::Model(format!("no output {}", self.profile.output)))?;
        let (shape, values) = output.try_extract_tensor::<f32>()?;
        let vector = match &shape[..] {
            [1, d] | [d] => usize::try_from(*d).ok(),
            _ => None,
        };
        if vector != Some(values.len()) {
            return Err(Error::Model(format!(
                "the model's output has shape {shape:?}, not one vector"
            )));
        }
        if self.dim > 0 && values.len() != self.dim {
            return Err(Error::Model(format!(
                "the model returned {} values, not {}",
                values.len(),
                self.dim
            )));
        }
        let mut embedding = values.to_vec();
        normalize(&mut embedding)?;
        Ok(embedding)
    }
}

fn checksum(model: &Path) -> Result<String, Error> {
    sha256_file(model).map_err(|source| Error::Io {
        path: model.to_owned(),
        source,
    })
}

/// Scales `v` to unit length. A zero or non-finite vector is
/// `NoEmbedding`: some models return NaN instead of failing on short input.
fn normalize(v: &mut [f32]) -> Result<(), Error> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return Err(Error::NoEmbedding);
    }
    for x in v {
        *x /= norm;
    }
    Ok(())
}

/// The cosine similarity of two L2-normalized embeddings, or `None` when
/// their lengths differ (embeddings from different models).
#[must_use]
pub fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    (a.len() == b.len()).then(|| a.iter().zip(b).map(|(x, y)| x * y).sum())
}

#[cfg(test)]
mod tests {
    use super::{Error, cosine, normalize, parts, profile_for, sha256_file};

    #[test]
    fn normalizes_and_rejects_unusable_vectors() {
        let mut v = [3.0, 4.0];
        normalize(&mut v).expect("usable");
        assert!((cosine(&v, &v).expect("same length") - 1.0).abs() < 1e-6);
        assert!(matches!(
            normalize(&mut [0.0, 0.0]),
            Err(Error::NoEmbedding)
        ));
        assert!(matches!(
            normalize(&mut [f32::NAN, 1.0]),
            Err(Error::NoEmbedding)
        ));
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), None);
    }

    #[test]
    fn splits_long_segments_into_equal_parts() {
        let (min, max) = (8_000, 160_000);
        let lengths = |len| parts(len, max).map(|r| r.len()).collect::<Vec<_>>();
        assert_eq!(lengths(min), [min]);
        assert_eq!(lengths(max), [max]);
        assert_eq!(lengths(max + 1), [80_001, 80_000]);
        assert_eq!(lengths(2 * max + 1), [106_667, 106_667, 106_667]);
        // Rounding each part up would leave a last part of one sample.
        let small = parts(639_201, 800).map(|r| r.len()).collect::<Vec<_>>();
        assert_eq!(small.len(), 800);
        assert!(small.iter().all(|&n| n == 799 || n == 800), "{small:?}");
        assert_eq!(parts(0, max).count(), 0);
        for len in [min, max - 1, max + 1, 3 * max + 7, 10 * max + 3] {
            let ranges: Vec<_> = parts(len, max).collect();
            assert_eq!(ranges.first().map(|r| r.start), Some(0));
            assert_eq!(ranges.last().map(|r| r.end), Some(len));
            assert!(ranges.windows(2).all(|w| w[0].end == w[1].start));
            assert!(
                ranges.iter().all(|r| (min..=max).contains(&r.len())),
                "{len}"
            );
        }
    }

    #[test]
    fn hashes_files() {
        let dir = std::env::temp_dir().join(format!("diarization-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("abc");
        std::fs::write(&path, b"abc").expect("written");
        let hex = sha256_file(&path).expect("hashed");
        std::fs::remove_dir_all(&dir).expect("removed");
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn finds_profiles_by_name_or_checksum() {
        let qualified = "7bb2f06e9df17cdf1ef14ee8a15ab08ed28e8d0ef5054ee135741560df2ec068";
        assert_eq!(
            profile_for(None, qualified).expect("known").name,
            "wespeaker-resnet34"
        );
        assert!(matches!(
            profile_for(None, "00"),
            Err(Error::UnknownModel { .. })
        ));
        assert_eq!(
            profile_for(Some("wespeaker-resnet34"), "00")
                .expect("named")
                .name,
            "wespeaker-resnet34"
        );
        assert!(matches!(
            profile_for(Some("nope"), qualified),
            Err(Error::UnknownProfile(_))
        ));
    }
}
