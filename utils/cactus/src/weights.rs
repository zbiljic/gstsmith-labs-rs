//! Classifies `.cact` archives before the engine loads one, because loading
//! replaces a process-global model slot.
//!
//! A `.cact` archive (upstream `needle/model/export.py`) is a header of 48
//! little-endian `u32` fields and one `f32`, a `f32` codebook, a directory of
//! 44-byte tensor records, then the tensor data. The format tag does not say
//! which kind of model follows; the record after `final_norm` does: a head
//! manifest for text, an audio manifest for speech, a vision manifest for
//! Mirage. This mirrors `weightsKind` in `cactus-go`, including its head
//! code and audio manifest checks.

/// The `.cact` generation tag of engine 4.x weights.
pub const GENERATION_TAG: u32 = 0x05e1_2a84;

const HEADER_SIZE: usize = 49 * 4;
const RECORD_SIZE: usize = 44;
const TENSOR_COUNT: usize = 1;
const CODEBOOK_SIZE: usize = 2;
const MODEL_WIDTH: usize = 7;
const LAYERS: usize = 10;
const QKV_CONV_TAPS: usize = 19;
const ENGRAM_SITES: usize = 31;
const MAX_ENGRAM_SITES: u64 = 16;
const FIXED_TENSORS: u64 = 13;
const LAYER_TENSORS: u64 = 24;
const LAYER_TAP_TENSORS: u64 = 3;
const ENGRAM_TENSORS: u64 = 4;
const DTYPE_FP16: u8 = 1;
const DTYPE_FP32: u8 = 2;
const DTYPE_RAW: u8 = 4;
const MAX_HEADS: u32 = 3;
const AUDIO_MANIFEST_LENGTH: u32 = 13;
const VISION_MANIFEST_LENGTH: u32 = 36;
const VISION_MANIFEST_MAGIC: f32 = 4721.0;
const AUDIO_MANIFEST_MAGIC: f32 = 3246.0;
const AUDIO_MANIFEST_VERSION: f32 = 1.0;
const AUDIO_MANIFEST_RATE: usize = 9;
const SPEECH_SAMPLE_RATE: f32 = 16_000.0;
/// `heads.manifest` codes, stored as the FP16 values 1, 2 and 3.
const HEAD_CODES: [u16; 3] = [0x3c00, 0x4000, 0x4200];

/// Head codes must be strictly increasing and drawn from the known kinds.
fn valid_heads(data: &[u8]) -> bool {
    let (codes, rest) = data.as_chunks::<2>();
    rest.is_empty()
        && codes
            .iter()
            .map(|code| u16::from_le_bytes(*code))
            .try_fold(0u16, |previous, code| {
                (code > previous && HEAD_CODES.contains(&code)).then_some(code)
            })
            .is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Speech,
    Vision,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::Speech => "speech",
            Self::Vision => "vision",
        })
    }
}

fn u32_at(blob: &[u8], offset: usize) -> Option<u32> {
    let bytes = blob.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn u64_at(blob: &[u8], offset: usize) -> Option<u64> {
    let bytes = blob.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

fn field(blob: &[u8], index: usize) -> Option<u64> {
    u32_at(blob, index.checked_mul(4)?).map(u64::from)
}

/// Returns the archive's generation tag, if it has a header at all.
#[must_use]
pub fn tag(blob: &[u8]) -> Option<u32> {
    u32_at(blob, 0)
}

/// Classifies an archive of engine 4.x weights; `None` for anything else.
#[must_use]
pub fn kind(blob: &[u8]) -> Option<Kind> {
    if blob.len() < HEADER_SIZE || tag(blob)? != GENERATION_TAG {
        return None;
    }
    let len = u64::try_from(blob.len()).ok()?;
    let count = field(blob, TENSOR_COUNT)?;
    let directory =
        u64::try_from(HEADER_SIZE).ok()? + field(blob, CODEBOOK_SIZE)?.checked_mul(4)?;
    let (layers, sites) = (field(blob, LAYERS)?, field(blob, ENGRAM_SITES)?);
    let record_size = u64::try_from(RECORD_SIZE).ok()?;
    if directory > len
        || count > (len - directory) / record_size
        || field(blob, MODEL_WIDTH)? == 0
        || layers == 0
        || sites > MAX_ENGRAM_SITES
    {
        return None;
    }
    let per_layer = LAYER_TENSORS
        + if field(blob, QKV_CONV_TAPS)? == 0 {
            0
        } else {
            LAYER_TAP_TENSORS
        };
    let index = FIXED_TENSORS
        .checked_add(layers.checked_mul(per_layer)?)?
        .checked_add(sites.checked_mul(ENGRAM_TENSORS)?)?;
    if index >= count {
        return None;
    }
    let record = usize::try_from(directory + index * record_size).ok()?;
    let dtype = *blob.get(record)?;
    let ndim = *blob.get(record + 1)?;
    let pad = blob.get(record + 2..record + 4)?;
    if pad != [0, 0] || u32_at(blob, record + 36)? != 0 || u32_at(blob, record + 40)? != 0 {
        return None;
    }
    let shape = u32_at(blob, record + 4)?;
    if (1..4).any(|i| u32_at(blob, record + 4 + i * 4) != Some(0)) {
        return None;
    }
    let (offset, size) = (u64_at(blob, record + 20)?, u64_at(blob, record + 28)?);
    if offset < directory + count * record_size || offset > len || size == 0 || size > len - offset
    {
        return None;
    }
    let data = blob.get(usize::try_from(offset).ok()?..usize::try_from(offset + size).ok()?)?;
    let value = |i: usize| u32_at(data, i * 4).map(f32::from_bits);
    let last = index + 1 == count;
    match (dtype, ndim) {
        (DTYPE_RAW, 0) if shape == 0 && last => Some(Kind::Text),
        (DTYPE_FP16, 1) if (1..=MAX_HEADS).contains(&shape) && size == u64::from(shape) * 2 => {
            valid_heads(data).then_some(Kind::Text)
        }
        (DTYPE_FP32, 1)
            if !last
                && shape == AUDIO_MANIFEST_LENGTH
                && size == u64::from(shape) * 4
                && value(0) == Some(AUDIO_MANIFEST_MAGIC)
                && value(1) == Some(AUDIO_MANIFEST_VERSION)
                && value(AUDIO_MANIFEST_RATE) == Some(SPEECH_SAMPLE_RATE) =>
        {
            Some(Kind::Speech)
        }
        (DTYPE_FP32, 1)
            if !last
                && shape == VISION_MANIFEST_LENGTH
                && size == u64::from(shape) * 4
                && value(0) == Some(VISION_MANIFEST_MAGIC) =>
        {
            Some(Kind::Vision)
        }
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal archive whose record after `final_norm` is an FP32 manifest
    /// of `length` values starting with `magic`, followed by one more tensor.
    pub(crate) fn archive(length: u32, magic: f32) -> Vec<u8> {
        let layers = 1u64;
        let index = FIXED_TENSORS + layers * LAYER_TENSORS;
        let count = index + 2;
        let mut blob = vec![0u8; HEADER_SIZE];
        let mut set = |i: usize, v: u32| blob[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        set(0, GENERATION_TAG);
        set(TENSOR_COUNT, u32::try_from(count).unwrap());
        set(MODEL_WIDTH, 8);
        set(LAYERS, 1);
        let directory = blob.len();
        let data_offset = directory + usize::try_from(count).unwrap() * RECORD_SIZE;
        blob.resize(data_offset, 0);
        let record = directory + usize::try_from(index).unwrap() * RECORD_SIZE;
        blob[record] = DTYPE_FP32;
        blob[record + 1] = 1;
        blob[record + 4..record + 8].copy_from_slice(&length.to_le_bytes());
        blob[record + 20..record + 28]
            .copy_from_slice(&u64::try_from(data_offset).unwrap().to_le_bytes());
        blob[record + 28..record + 36].copy_from_slice(&(u64::from(length) * 4).to_le_bytes());
        blob.extend_from_slice(&magic.to_le_bytes());
        blob.resize(data_offset + usize::try_from(length).unwrap() * 4, 0);
        blob
    }

    #[test]
    fn classifies_vision_and_speech_manifests() {
        assert_eq!(kind(&archive(36, 4721.0)), Some(Kind::Vision));
        let mut speech = archive(13, 3246.0);
        assert_eq!(kind(&speech), None, "no layout version or sample rate");
        let data = speech.len() - 13 * 4;
        speech[data + 4..data + 8].copy_from_slice(&1.0f32.to_le_bytes());
        speech[data + 36..data + 40].copy_from_slice(&16_000.0f32.to_le_bytes());
        assert_eq!(kind(&speech), Some(Kind::Speech));
        assert_eq!(kind(&archive(36, 1.0)), None);
        assert_eq!(kind(&archive(12, 4721.0)), None);
    }

    #[test]
    fn rejects_other_generations_and_truncated_archives() {
        let mut blob = archive(36, 4721.0);
        assert_eq!(kind(&blob[..100]), None);
        assert_eq!(kind(&blob[..blob.len() - 1]), None);
        blob[0..4].copy_from_slice(&0x05e1_2a83u32.to_le_bytes());
        assert_eq!(kind(&blob), None);
        assert_eq!(tag(&blob), Some(0x05e1_2a83));
        assert_eq!(kind(&[]), None);
    }

    #[test]
    fn head_codes_must_increase_from_known_kinds() {
        let codes = |values: &[u16]| {
            values
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>()
        };
        assert!(valid_heads(&codes(&[0x3c00, 0x4000])));
        assert!(valid_heads(&codes(&[0x4200])));
        assert!(!valid_heads(&codes(&[0x4000, 0x3c00])));
        assert!(!valid_heads(&codes(&[0x3c00, 0x3c00])));
        assert!(!valid_heads(&codes(&[0x1234])));
        assert!(!valid_heads(&[0x00]));
    }
}
