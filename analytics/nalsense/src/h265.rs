use std::error::Error as StdError;
use std::fmt;

use crate::analyzer::PictureType;
use crate::codec::AccessUnit;

const MAX_PPS_ID: u32 = 63;
const MAX_SPS_ID: u32 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PpsPrefix {
    extra_slice_header_bits: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanError {
    MissingStartCode,
    NonZeroLeadingBytes,
    EmptyNal,
    TruncatedNalHeader,
    ForbiddenBit,
    TemporalIdZero,
    UnsupportedLayer(u8),
    UnsupportedVclNalType(u8),
    MissingPrimarySlice,
    MissingPps(u32),
    PpsIdOutOfRange(u32),
    SpsIdOutOfRange(u32),
    TruncatedHeader,
    ExpGolombOverflow,
    InvalidSliceType(u32),
    AccessUnitTooLarge,
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingStartCode => {
                formatter.write_str("H.265 access unit has no Annex-B start code")
            }
            Self::NonZeroLeadingBytes => formatter
                .write_str("H.265 access unit has non-zero bytes before its first start code"),
            Self::EmptyNal => formatter.write_str("H.265 access unit contains an empty NAL unit"),
            Self::TruncatedNalHeader => {
                formatter.write_str("H.265 NAL unit has a truncated two-byte header")
            }
            Self::ForbiddenBit => formatter.write_str("H.265 NAL forbidden_zero_bit is set"),
            Self::TemporalIdZero => formatter.write_str("H.265 NAL nuh_temporal_id_plus1 is zero"),
            Self::UnsupportedLayer(layer_id) => {
                write!(formatter, "H.265 layer {layer_id} is not supported")
            }
            Self::UnsupportedVclNalType(nal_type) => {
                write!(
                    formatter,
                    "H.265 reserved VCL NAL type {nal_type} is not supported"
                )
            }
            Self::MissingPrimarySlice => {
                formatter.write_str("H.265 access unit has no first independent VCL slice header")
            }
            Self::MissingPps(pps_id) => {
                write!(formatter, "H.265 slice references unknown PPS {pps_id}")
            }
            Self::PpsIdOutOfRange(pps_id) => {
                write!(formatter, "H.265 PPS id {pps_id} exceeds 63")
            }
            Self::SpsIdOutOfRange(sps_id) => {
                write!(formatter, "H.265 SPS id {sps_id} exceeds 15")
            }
            Self::TruncatedHeader => {
                formatter.write_str("H.265 parameter-set or slice header is truncated")
            }
            Self::ExpGolombOverflow => {
                formatter.write_str("H.265 Exp-Golomb value exceeds 32 bits")
            }
            Self::InvalidSliceType(value) => {
                write!(formatter, "H.265 slice_type {value} is outside 0 through 2")
            }
            Self::AccessUnitTooLarge => {
                formatter.write_str("H.265 VCL payload exceeds the supported 32-bit size")
            }
        }
    }
}

impl StdError for ScanError {}

#[derive(Debug)]
pub(crate) struct AccessUnitScanner {
    pps: [Option<PpsPrefix>; 64],
}

impl AccessUnitScanner {
    pub(crate) const fn new() -> Self {
        Self { pps: [None; 64] }
    }

    pub(crate) fn reset(&mut self) {
        self.pps.fill(None);
    }

    pub(crate) fn scan(
        &mut self,
        input: &[u8],
        delta_unit: bool,
    ) -> Result<Option<AccessUnit>, ScanError> {
        let Some((first_start, first_prefix)) = find_start_code(input, 0) else {
            return Err(ScanError::MissingStartCode);
        };
        if input
            .get(..first_start)
            .is_some_and(|leading| leading.iter().any(|byte| *byte != 0))
        {
            return Err(ScanError::NonZeroLeadingBytes);
        }

        let mut current = (first_start, first_prefix);
        let mut vcl_bytes = 0_u32;
        let mut has_vcl = false;
        let mut has_irap = false;
        let mut has_idr = false;
        let mut picture_type = None;
        let mut is_reference_picture = false;

        loop {
            let nal_start = current
                .0
                .checked_add(current.1)
                .ok_or(ScanError::AccessUnitTooLarge)?;
            let next = find_start_code(input, nal_start);
            let mut nal_end = next.map_or(input.len(), |(start, _prefix)| start);
            while nal_end > nal_start && input.get(nal_end - 1) == Some(&0) {
                nal_end -= 1;
            }
            let nal = input.get(nal_start..nal_end).ok_or(ScanError::EmptyNal)?;
            let header = NalHeader::parse(nal)?;
            if header.layer_id != 0 {
                return Err(ScanError::UnsupportedLayer(header.layer_id));
            }

            match header.nal_type {
                34 => self.update_pps(nal)?,
                nal_type if nal_type < 32 => {
                    if !is_supported_vcl(nal_type) {
                        return Err(ScanError::UnsupportedVclNalType(nal_type));
                    }
                    has_vcl = true;
                    has_irap |= is_irap(nal_type);
                    has_idr |= is_idr(nal_type);
                    is_reference_picture |= is_reference_vcl(nal_type);
                    if picture_type.is_none()
                        && let Some(current_type) = self.parse_primary_slice(nal, nal_type)?
                    {
                        picture_type = Some(current_type);
                    }
                    let length =
                        u32::try_from(nal.len()).map_err(|_error| ScanError::AccessUnitTooLarge)?;
                    vcl_bytes = vcl_bytes
                        .checked_add(length)
                        .ok_or(ScanError::AccessUnitTooLarge)?;
                }
                _ => {}
            }

            let Some(next) = next else {
                break;
            };
            current = next;
        }

        if !has_vcl {
            return Ok(None);
        }
        let picture_type = picture_type.ok_or(ScanError::MissingPrimarySlice)?;
        Ok(Some(AccessUnit {
            encoded_vcl_bytes: vcl_bytes,
            picture_type,
            is_reference_picture,
            luma_qp: None,
            is_keyframe: has_irap || !delta_unit,
            is_idr: has_idr,
        }))
    }

    fn update_pps(&mut self, nal: &[u8]) -> Result<(), ScanError> {
        let payload = nal.get(2..).ok_or(ScanError::TruncatedNalHeader)?;
        let mut reader = RbspBitReader::new(payload);
        let pps_id = reader.read_unsigned_exp_golomb()?;
        if pps_id > MAX_PPS_ID {
            return Err(ScanError::PpsIdOutOfRange(pps_id));
        }
        let sps_id = reader.read_unsigned_exp_golomb()?;
        if sps_id > MAX_SPS_ID {
            return Err(ScanError::SpsIdOutOfRange(sps_id));
        }
        let _dependent_slice_segments_enabled = reader.read_bit()?;
        let _output_flag_present = reader.read_bit()?;
        let extra_slice_header_bits = reader.read_bits(3)?;
        let index = usize::try_from(pps_id).map_err(|_error| ScanError::PpsIdOutOfRange(pps_id))?;
        let slot = self
            .pps
            .get_mut(index)
            .ok_or(ScanError::PpsIdOutOfRange(pps_id))?;
        *slot = Some(PpsPrefix {
            extra_slice_header_bits,
        });
        Ok(())
    }

    fn parse_primary_slice(
        &self,
        nal: &[u8],
        nal_type: u8,
    ) -> Result<Option<PictureType>, ScanError> {
        let payload = nal.get(2..).ok_or(ScanError::TruncatedNalHeader)?;
        let mut reader = RbspBitReader::new(payload);
        let first_slice_segment_in_pic = reader.read_bit()?;
        if is_irap(nal_type) {
            let _no_output_of_prior_pics = reader.read_bit()?;
        }
        let pps_id = reader.read_unsigned_exp_golomb()?;
        if pps_id > MAX_PPS_ID {
            return Err(ScanError::PpsIdOutOfRange(pps_id));
        }
        if !first_slice_segment_in_pic {
            return Ok(None);
        }
        let index = usize::try_from(pps_id).map_err(|_error| ScanError::PpsIdOutOfRange(pps_id))?;
        let prefix = self
            .pps
            .get(index)
            .copied()
            .flatten()
            .ok_or(ScanError::MissingPps(pps_id))?;
        for _ in 0..prefix.extra_slice_header_bits {
            let _reserved_flag = reader.read_bit()?;
        }
        picture_type_from_slice_type(reader.read_unsigned_exp_golomb()?).map(Some)
    }
}

#[derive(Debug, Clone, Copy)]
struct NalHeader {
    nal_type: u8,
    layer_id: u8,
}

impl NalHeader {
    fn parse(nal: &[u8]) -> Result<Self, ScanError> {
        let first = *nal.first().ok_or(ScanError::EmptyNal)?;
        let second = *nal.get(1).ok_or(ScanError::TruncatedNalHeader)?;
        if first & 0x80 != 0 {
            return Err(ScanError::ForbiddenBit);
        }
        if second.trailing_zeros() >= 3 {
            return Err(ScanError::TemporalIdZero);
        }
        Ok(Self {
            nal_type: (first >> 1) & 0x3f,
            layer_id: ((first & 1) << 5) | ((second >> 3) & 0x1f),
        })
    }
}

const fn is_supported_vcl(nal_type: u8) -> bool {
    matches!(nal_type, 0..=9 | 16..=21)
}

const fn is_irap(nal_type: u8) -> bool {
    matches!(nal_type, 16..=21)
}

const fn is_idr(nal_type: u8) -> bool {
    matches!(nal_type, 19 | 20)
}

const fn is_reference_vcl(nal_type: u8) -> bool {
    matches!(nal_type, 1 | 3 | 5 | 7 | 9 | 16..=21)
}

const fn picture_type_from_slice_type(slice_type: u32) -> Result<PictureType, ScanError> {
    match slice_type {
        0 => Ok(PictureType::B),
        1 => Ok(PictureType::P),
        2 => Ok(PictureType::I),
        value => Err(ScanError::InvalidSliceType(value)),
    }
}

struct RbspBitReader<'a> {
    ebsp: &'a [u8],
    byte_offset: usize,
    encoded_zero_count: u8,
    current_byte: u8,
    bits_remaining: u8,
}

impl<'a> RbspBitReader<'a> {
    const fn new(ebsp: &'a [u8]) -> Self {
        Self {
            ebsp,
            byte_offset: 0,
            encoded_zero_count: 0,
            current_byte: 0,
            bits_remaining: 0,
        }
    }

    fn read_unsigned_exp_golomb(&mut self) -> Result<u32, ScanError> {
        let mut leading_zero_bits = 0_u32;
        while !self.read_bit()? {
            leading_zero_bits = leading_zero_bits
                .checked_add(1)
                .ok_or(ScanError::ExpGolombOverflow)?;
            if leading_zero_bits >= u32::BITS {
                return Err(ScanError::ExpGolombOverflow);
            }
        }
        let suffix = self.read_bits_u32(leading_zero_bits)?;
        let prefix = 1_u32
            .checked_shl(leading_zero_bits)
            .ok_or(ScanError::ExpGolombOverflow)?
            .saturating_sub(1);
        prefix
            .checked_add(suffix)
            .ok_or(ScanError::ExpGolombOverflow)
    }

    fn read_bits(&mut self, count: u8) -> Result<u8, ScanError> {
        let mut value = 0_u8;
        for _ in 0..count {
            value = (value << 1) | u8::from(self.read_bit()?);
        }
        Ok(value)
    }

    fn read_bits_u32(&mut self, count: u32) -> Result<u32, ScanError> {
        let mut value = 0_u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.read_bit()?);
        }
        Ok(value)
    }

    fn read_bit(&mut self) -> Result<bool, ScanError> {
        if self.bits_remaining == 0 {
            self.current_byte = self.next_rbsp_byte()?;
            self.bits_remaining = 8;
        }
        self.bits_remaining -= 1;
        Ok(self.current_byte & (1 << self.bits_remaining) != 0)
    }

    fn next_rbsp_byte(&mut self) -> Result<u8, ScanError> {
        loop {
            let byte = *self
                .ebsp
                .get(self.byte_offset)
                .ok_or(ScanError::TruncatedHeader)?;
            self.byte_offset = self
                .byte_offset
                .checked_add(1)
                .ok_or(ScanError::ExpGolombOverflow)?;
            if self.encoded_zero_count == 2 && byte == 0x03 {
                self.encoded_zero_count = 0;
                continue;
            }
            if byte == 0 {
                self.encoded_zero_count = self.encoded_zero_count.saturating_add(1).min(2);
            } else {
                self.encoded_zero_count = 0;
            }
            return Ok(byte);
        }
    }
}

fn find_start_code(input: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut offset = from;
    while offset.checked_add(3)? <= input.len() {
        if input.get(offset) == Some(&0) && input.get(offset + 1) == Some(&0) {
            if input.get(offset + 2) == Some(&1) {
                return Some((offset, 3));
            }
            if offset.checked_add(4)? <= input.len()
                && input.get(offset + 2) == Some(&0)
                && input.get(offset + 3) == Some(&1)
            {
                return Some((offset, 4));
            }
        }
        offset = offset.checked_add(1)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pps() -> [u8; 7] {
        [0, 0, 0, 1, 0x44, 0x01, 0xc0]
    }

    fn slice(nal_type: u8, rbsp: u8) -> [u8; 7] {
        [0, 0, 0, 1, nal_type << 1, 0x01, rbsp]
    }

    #[test]
    fn classifies_x265_style_idr_and_retains_pps_state() {
        let mut scanner = AccessUnitScanner::new();
        let mut idr = pps().to_vec();
        idr.extend_from_slice(&slice(20, 0xac));
        assert_eq!(
            scanner.scan(&idr, false),
            Ok(Some(AccessUnit {
                encoded_vcl_bytes: 3,
                picture_type: PictureType::I,
                is_reference_picture: true,
                luma_qp: None,
                is_keyframe: true,
                is_idr: true,
            }))
        );

        assert_eq!(
            scanner.scan(&slice(1, 0xd0), true),
            Ok(Some(AccessUnit {
                encoded_vcl_bytes: 3,
                picture_type: PictureType::P,
                is_reference_picture: true,
                luma_qp: None,
                is_keyframe: false,
                is_idr: false,
            }))
        );
    }

    #[test]
    fn scans_project_generated_x265_idr_fixture() {
        let access_unit = AccessUnitScanner::new()
            .scan(
                include_bytes!("../tests/fixtures/hevc-idr-64x64.h265"),
                false,
            )
            .expect("valid project-generated x265 fixture")
            .expect("fixture contains one VCL picture");
        assert_eq!(access_unit.picture_type, PictureType::I);
        assert!(access_unit.encoded_vcl_bytes > 2);
        assert!(access_unit.is_reference_picture);
        assert!(access_unit.is_keyframe);
        assert!(access_unit.is_idr);
    }

    #[test]
    fn classifies_non_reference_b_picture() {
        let mut scanner = AccessUnitScanner::new();
        assert_eq!(scanner.scan(&pps(), true), Ok(None));
        assert_eq!(
            scanner.scan(&slice(0, 0xe0), true),
            Ok(Some(AccessUnit {
                encoded_vcl_bytes: 3,
                picture_type: PictureType::B,
                is_reference_picture: false,
                luma_qp: None,
                is_keyframe: false,
                is_idr: false,
            }))
        );
    }

    #[test]
    fn rejects_slice_without_known_pps() {
        assert_eq!(
            AccessUnitScanner::new().scan(&slice(1, 0xd0), true),
            Err(ScanError::MissingPps(0))
        );
    }

    #[test]
    fn reset_discards_parameter_sets() {
        let mut scanner = AccessUnitScanner::new();
        assert_eq!(scanner.scan(&pps(), true), Ok(None));
        scanner.reset();
        assert_eq!(
            scanner.scan(&slice(1, 0xd0), true),
            Err(ScanError::MissingPps(0))
        );
    }

    #[test]
    fn rejects_invalid_nal_headers() {
        assert_eq!(
            AccessUnitScanner::new().scan(&[0, 0, 1, 0x02], true),
            Err(ScanError::TruncatedNalHeader)
        );
        assert_eq!(
            AccessUnitScanner::new().scan(&[0, 0, 1, 0x82, 0x01, 0xd0], true),
            Err(ScanError::ForbiddenBit)
        );
        assert_eq!(
            AccessUnitScanner::new().scan(&[0, 0, 1, 0x02, 0x00, 0xd0], true),
            Err(ScanError::TemporalIdZero)
        );
        assert_eq!(
            AccessUnitScanner::new().scan(&[0, 0, 1, 0x03, 0x09, 0xd0], true),
            Err(ScanError::UnsupportedLayer(33))
        );
        assert_eq!(
            AccessUnitScanner::new().scan(&[0, 0, 1, 20, 0x01, 0xd0], true),
            Err(ScanError::UnsupportedVclNalType(10))
        );
    }

    #[test]
    fn arbitrary_short_inputs_never_panic() {
        for length in 0..=8 {
            for byte in u8::MIN..=u8::MAX {
                let input = vec![byte; length];
                let _result = AccessUnitScanner::new().scan(&input, true);
            }
        }
    }
}
