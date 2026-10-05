use std::error::Error as StdError;
use std::fmt;

#[cfg(feature = "qp-diagnostics")]
use h264_reader::Context;
#[cfg(feature = "qp-diagnostics")]
use h264_reader::nal::pps::PicParameterSet;
#[cfg(feature = "qp-diagnostics")]
use h264_reader::nal::slice::SliceHeader;
#[cfg(feature = "qp-diagnostics")]
use h264_reader::nal::sps::SeqParameterSet;
#[cfg(feature = "qp-diagnostics")]
use h264_reader::nal::{Nal, RefNal};

use crate::analyzer::PictureType;
use crate::annex_b::find_start_code;
use crate::codec::AccessUnit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanError {
    MissingStartCode,
    NonZeroLeadingBytes,
    EmptyNal,
    ForbiddenBit,
    MissingPrimarySlice,
    TruncatedSliceHeader,
    ExpGolombOverflow,
    InvalidSliceType(u32),
    AccessUnitTooLarge,
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingStartCode => {
                formatter.write_str("H.264 access unit has no Annex-B start code")
            }
            Self::NonZeroLeadingBytes => formatter
                .write_str("H.264 access unit has non-zero bytes before its first start code"),
            Self::EmptyNal => formatter.write_str("H.264 access unit contains an empty NAL unit"),
            Self::ForbiddenBit => formatter.write_str("H.264 NAL forbidden_zero_bit is set"),
            Self::MissingPrimarySlice => {
                formatter.write_str("H.264 access unit has no primary VCL slice header")
            }
            Self::TruncatedSliceHeader => {
                formatter.write_str("H.264 access unit has a truncated slice header")
            }
            Self::ExpGolombOverflow => {
                formatter.write_str("H.264 slice header Exp-Golomb value exceeds 32 bits")
            }
            Self::InvalidSliceType(value) => {
                write!(formatter, "H.264 slice_type {value} is outside 0 through 9")
            }
            Self::AccessUnitTooLarge => {
                formatter.write_str("H.264 VCL payload exceeds the supported 32-bit size")
            }
        }
    }
}

impl StdError for ScanError {}

#[derive(Debug)]
pub(crate) struct AccessUnitScanner {
    #[cfg(feature = "qp-diagnostics")]
    qp_context: Option<Context>,
}

impl AccessUnitScanner {
    pub(crate) fn new(qp_diagnostics: bool) -> Self {
        #[cfg(feature = "qp-diagnostics")]
        {
            Self {
                qp_context: if qp_diagnostics {
                    Some(Context::new())
                } else {
                    None
                },
            }
        }
        #[cfg(not(feature = "qp-diagnostics"))]
        {
            let _ = qp_diagnostics;
            Self {}
        }
    }

    pub(crate) fn reset(&mut self) {
        let qp_diagnostics = self.qp_diagnostics_enabled();
        *self = Self::new(qp_diagnostics);
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
        let mut has_idr = false;
        let mut picture_type = None;
        let mut is_reference_picture = false;
        let mut luma_qp = None;
        let mut luma_qp_is_complete = self.qp_diagnostics_enabled();

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
            let header = *nal.first().ok_or(ScanError::EmptyNal)?;
            if header & 0x80 != 0 {
                return Err(ScanError::ForbiddenBit);
            }
            let nal_type = header & 0x1f;
            if matches!(nal_type, 7 | 8) {
                self.update_parameter_set(nal_type, nal);
            }
            if (1..=5).contains(&nal_type) {
                has_vcl = true;
                has_idr |= nal_type == 5;
                if matches!(nal_type, 1 | 2 | 5) {
                    is_reference_picture |= header & 0x60 != 0;
                    match self.parse_luma_qp(nal) {
                        Some(current_qp) if luma_qp.is_none_or(|qp| qp == current_qp) => {
                            luma_qp = Some(current_qp);
                        }
                        Some(_) | None => luma_qp_is_complete = false,
                    }
                    let slice_type = parse_slice_type(nal.get(1..).ok_or(ScanError::EmptyNal)?)?;
                    let parsed_type = picture_type_from_slice_type(slice_type)?;
                    let current_type = if nal_type == 5 {
                        PictureType::I
                    } else {
                        parsed_type
                    };
                    picture_type = Some(merge_picture_types(picture_type, current_type));
                }
                let length =
                    u32::try_from(nal.len()).map_err(|_error| ScanError::AccessUnitTooLarge)?;
                vcl_bytes = vcl_bytes
                    .checked_add(length)
                    .ok_or(ScanError::AccessUnitTooLarge)?;
            }

            let Some(next) = next else {
                break;
            };
            current = next;
        }

        if !has_vcl {
            return Ok(None);
        }
        let picture_type = if has_idr {
            PictureType::I
        } else {
            picture_type.ok_or(ScanError::MissingPrimarySlice)?
        };
        Ok(Some(AccessUnit {
            encoded_vcl_bytes: vcl_bytes,
            picture_type,
            is_reference_picture,
            luma_qp: luma_qp.filter(|_qp| luma_qp_is_complete),
            is_keyframe: has_idr || !delta_unit,
            is_idr: has_idr,
        }))
    }

    #[cfg(feature = "qp-diagnostics")]
    fn qp_diagnostics_enabled(&self) -> bool {
        self.qp_context.is_some()
    }

    #[cfg(not(feature = "qp-diagnostics"))]
    #[expect(
        clippy::unused_self,
        reason = "keep the scanner call shape identical with and without QP diagnostics"
    )]
    const fn qp_diagnostics_enabled(&self) -> bool {
        false
    }

    #[cfg(feature = "qp-diagnostics")]
    fn update_parameter_set(&mut self, nal_type: u8, bytes: &[u8]) {
        let Some(context) = self.qp_context.as_mut() else {
            return;
        };
        let nal = RefNal::new(bytes, &[], true);
        match nal_type {
            7 => {
                if let Ok(sps) = SeqParameterSet::from_bits(nal.rbsp_bits()) {
                    context.put_seq_param_set(sps);
                }
            }
            8 => {
                if let Ok(pps) = PicParameterSet::from_bits(context, nal.rbsp_bits()) {
                    context.put_pic_param_set(pps);
                }
            }
            _ => {}
        }
    }

    #[cfg(not(feature = "qp-diagnostics"))]
    #[expect(
        clippy::unused_self,
        reason = "keep the scanner call shape identical with and without QP diagnostics"
    )]
    fn update_parameter_set(&mut self, _nal_type: u8, _bytes: &[u8]) {}

    #[cfg(feature = "qp-diagnostics")]
    fn parse_luma_qp(&self, bytes: &[u8]) -> Option<i32> {
        let context = self.qp_context.as_ref()?;
        let nal = RefNal::new(bytes, &[], true);
        let nal_header = nal.header().ok()?;
        let (slice, _sps, pps) =
            SliceHeader::from_bits(context, &mut nal.rbsp_bits(), nal_header, None).ok()?;
        26_i32
            .checked_add(pps.pic_init_qp_minus26)?
            .checked_add(slice.slice_qp_delta)
    }

    #[cfg(not(feature = "qp-diagnostics"))]
    #[expect(
        clippy::unused_self,
        reason = "keep the scanner call shape identical with and without QP diagnostics"
    )]
    const fn parse_luma_qp(&self, _bytes: &[u8]) -> Option<i32> {
        None
    }
}

pub(crate) fn is_idr_candidate(input: &[u8]) -> Result<bool, ScanError> {
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
    let mut has_idr = false;
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
        let header = *nal.first().ok_or(ScanError::EmptyNal)?;
        if header & 0x80 != 0 {
            return Err(ScanError::ForbiddenBit);
        }
        has_idr |= header & 0x1f == 5;

        let Some(next) = next else {
            break;
        };
        current = next;
    }

    Ok(has_idr)
}

#[cfg(test)]
fn scan_access_unit(input: &[u8], delta_unit: bool) -> Result<Option<AccessUnit>, ScanError> {
    AccessUnitScanner::new(false).scan(input, delta_unit)
}

fn parse_slice_type(ebsp: &[u8]) -> Result<u32, ScanError> {
    let mut reader = RbspBitReader::new(ebsp);
    let _first_mb_in_slice = reader.read_unsigned_exp_golomb()?;
    reader.read_unsigned_exp_golomb()
}

const fn picture_type_from_slice_type(slice_type: u32) -> Result<PictureType, ScanError> {
    match slice_type {
        0 | 3 | 5 | 8 => Ok(PictureType::P),
        1 | 6 => Ok(PictureType::B),
        2 | 4 | 7 | 9 => Ok(PictureType::I),
        value => Err(ScanError::InvalidSliceType(value)),
    }
}

const fn merge_picture_types(previous: Option<PictureType>, current: PictureType) -> PictureType {
    match (previous, current) {
        (Some(PictureType::B), _) | (_, PictureType::B) => PictureType::B,
        (Some(PictureType::P), _) | (_, PictureType::P) => PictureType::P,
        _ => PictureType::I,
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

        let mut suffix = 0_u32;
        for _ in 0..leading_zero_bits {
            suffix = (suffix << 1) | u32::from(self.read_bit()?);
        }
        let prefix = 1_u32
            .checked_shl(leading_zero_bits)
            .ok_or(ScanError::ExpGolombOverflow)?
            .saturating_sub(1);
        prefix
            .checked_add(suffix)
            .ok_or(ScanError::ExpGolombOverflow)
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
                .ok_or(ScanError::TruncatedSliceHeader)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_mixed_prefixes_and_sums_only_vcl_payloads() {
        let input = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x41, 0xe0, 1, 2, 0, 0, 0, 1, 0x41, 0xe0, 4,
        ];
        assert_eq!(
            scan_access_unit(&input, true),
            Ok(Some(AccessUnit {
                encoded_vcl_bytes: 7,
                picture_type: PictureType::P,
                is_reference_picture: true,
                luma_qp: None,
                is_keyframe: false,
                is_idr: false,
            }))
        );
    }

    #[test]
    fn identifies_idr_independently_of_buffer_flag() {
        let input = [0, 0, 1, 0x65, 0xb8, 2, 3];
        assert_eq!(
            scan_access_unit(&input, true),
            Ok(Some(AccessUnit {
                encoded_vcl_bytes: 4,
                picture_type: PictureType::I,
                is_reference_picture: true,
                luma_qp: None,
                is_keyframe: true,
                is_idr: true,
            }))
        );
    }

    #[test]
    fn inspects_idr_candidates_without_parsing_slice_headers() {
        assert_eq!(is_idr_candidate(&[0, 0, 1, 0x65]), Ok(true));
        assert_eq!(is_idr_candidate(&[0, 0, 1, 0x41, 0xe0]), Ok(false));
        assert_eq!(is_idr_candidate(&[0, 0, 1, 0x41, 0xb8]), Ok(false));
        assert_eq!(is_idr_candidate(&[0, 0, 1, 0x67, 1, 2]), Ok(false));
    }

    #[test]
    fn validates_every_nal_in_an_idr_candidate() {
        assert_eq!(is_idr_candidate(&[]), Err(ScanError::MissingStartCode));
        assert_eq!(is_idr_candidate(&[0, 0, 1]), Err(ScanError::EmptyNal));
        assert_eq!(
            is_idr_candidate(&[0, 0, 1, 0x65, 0, 0, 1, 0x80]),
            Err(ScanError::ForbiddenBit)
        );
    }

    #[test]
    fn honors_non_delta_buffer_flag() {
        let input = [0, 0, 1, 0x41, 0xb8];
        assert_eq!(
            scan_access_unit(&input, false),
            Ok(Some(AccessUnit {
                encoded_vcl_bytes: 2,
                picture_type: PictureType::I,
                is_reference_picture: true,
                luma_qp: None,
                is_keyframe: true,
                is_idr: false,
            }))
        );
    }

    #[test]
    fn ignores_parameter_set_only_access_unit() {
        assert_eq!(scan_access_unit(&[0, 0, 1, 0x67, 1, 2], false), Ok(None));
    }

    #[test]
    fn permits_zero_leading_and_trailing_bytes() {
        let input = [0, 0, 0, 0, 1, 0x41, 0xe0, 2, 0, 0];
        assert_eq!(
            scan_access_unit(&input, true),
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
    fn parses_all_standard_slice_type_values() {
        for (slice_type, expected) in [
            (0, PictureType::P),
            (1, PictureType::B),
            (2, PictureType::I),
            (3, PictureType::P),
            (4, PictureType::I),
            (5, PictureType::P),
            (6, PictureType::B),
            (7, PictureType::I),
            (8, PictureType::P),
            (9, PictureType::I),
        ] {
            assert_eq!(picture_type_from_slice_type(slice_type), Ok(expected));
        }
        assert_eq!(
            picture_type_from_slice_type(10),
            Err(ScanError::InvalidSliceType(10))
        );
    }

    #[test]
    fn parses_slice_type_through_emulation_prevention_bytes() {
        let ebsp = [0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x28];
        assert_eq!(parse_slice_type(&ebsp), Ok(1));
    }

    #[test]
    fn uses_most_dependent_type_for_mixed_slice_access_unit() {
        let input = [
            0, 0, 1, 0x41, 0xb8, // I slice
            0, 0, 1, 0x41, 0xe0, // P slice
            0, 0, 1, 0x41, 0xa8, // B slice
        ];
        assert_eq!(
            scan_access_unit(&input, true),
            Ok(Some(AccessUnit {
                encoded_vcl_bytes: 6,
                picture_type: PictureType::B,
                is_reference_picture: true,
                luma_qp: None,
                is_keyframe: false,
                is_idr: false,
            }))
        );
    }

    #[test]
    fn classifies_partitioned_slice_from_partition_a() {
        let input = [
            0, 0, 1, 0x42, 0xe0, // partition A with P slice header
            0, 0, 1, 0x43, 0xff, // partition B
            0, 0, 1, 0x44, 0xff, // partition C
        ];
        assert_eq!(
            scan_access_unit(&input, true),
            Ok(Some(AccessUnit {
                encoded_vcl_bytes: 6,
                picture_type: PictureType::P,
                is_reference_picture: true,
                luma_qp: None,
                is_keyframe: false,
                is_idr: false,
            }))
        );
    }

    #[test]
    fn distinguishes_reference_and_non_reference_pictures() {
        let non_reference_b = [0, 0, 1, 0x01, 0xa8];
        let reference_b = [0, 0, 1, 0x21, 0xa8];

        assert!(
            !scan_access_unit(&non_reference_b, true)
                .expect("valid non-reference B picture")
                .expect("VCL access unit")
                .is_reference_picture
        );
        assert!(
            scan_access_unit(&reference_b, true)
                .expect("valid reference B picture")
                .expect("VCL access unit")
                .is_reference_picture
        );
    }

    #[cfg(feature = "qp-diagnostics")]
    #[test]
    fn tracks_parameter_sets_and_reports_x264_slice_qp() {
        let sps = [
            0x67, 0x4d, 0x40, 0x0d, 0xec, 0xa0, 0xb8, 0xff, 0x2f, 0x80, 0xb7, 0x02, 0x02, 0x05,
            0x40, 0x00, 0x00, 0x03, 0x00, 0x40, 0x00, 0x00, 0x0c, 0x83, 0xc5, 0x0a, 0x65, 0x80,
        ];
        let pps = [0x68, 0xeb, 0xe3, 0xcb, 0x20];
        let idr_prefix = [
            0x65, 0x88, 0x84, 0x01, 0xff, 0xf3, 0x37, 0xed, 0xc5, 0x7f, 0x81, 0xab, 0x91, 0x37,
            0x14, 0x81, 0x8c, 0x82, 0x2f, 0xb5, 0xb9, 0x6e, 0x84, 0xce, 0xac, 0x0c, 0x09, 0xc5,
            0x00, 0x4b, 0x56, 0x24, 0xe4, 0x86, 0x4c, 0x77, 0x25, 0xf7, 0x56, 0xb4, 0xcb, 0xab,
            0xf2, 0xa2, 0xb9, 0x95, 0x03, 0xd5,
        ];
        let p_prefix = [
            0x41, 0x9a, 0x24, 0x6c, 0x7f, 0xda, 0xaf, 0x48, 0x39, 0x08, 0x66, 0x2b, 0xdf, 0xfc,
            0xf9, 0x3c, 0x11, 0x1c, 0x12, 0x77, 0x82, 0x91, 0x2c, 0x30, 0x9c, 0xc5, 0x93, 0xb2,
            0x5e, 0x7d, 0x0c, 0xe4, 0x97, 0x09, 0x70, 0x29, 0xb1, 0x4a, 0xda, 0xae, 0xd9, 0x09,
            0x19, 0xe4, 0xb4, 0xad, 0xf9, 0xd3,
        ];

        let mut first_access_unit = Vec::new();
        for nal in [&sps[..], &pps[..], &idr_prefix[..]] {
            first_access_unit.extend_from_slice(&[0, 0, 1]);
            first_access_unit.extend_from_slice(nal);
        }
        let mut scanner = AccessUnitScanner::new(true);
        let idr = scanner
            .scan(&first_access_unit, false)
            .expect("valid x264 IDR access unit")
            .expect("IDR VCL picture");
        assert_eq!(idr.luma_qp, Some(22));

        let mut second_access_unit = vec![0, 0, 1];
        second_access_unit.extend_from_slice(&p_prefix);
        let p_picture = scanner
            .scan(&second_access_unit, true)
            .expect("valid x264 P access unit")
            .expect("P VCL picture");
        assert_eq!(p_picture.luma_qp, Some(23));
    }

    #[test]
    fn rejects_malformed_input_without_panicking() {
        for (input, expected) in [
            (&[][..], ScanError::MissingStartCode),
            (&[1, 2, 3][..], ScanError::MissingStartCode),
            (&[9, 0, 0, 1, 0x41][..], ScanError::NonZeroLeadingBytes),
            (&[0, 0, 1][..], ScanError::EmptyNal),
            (&[0, 0, 1, 0x80][..], ScanError::ForbiddenBit),
            (&[0, 0, 1, 0x41][..], ScanError::TruncatedSliceHeader),
            (&[0, 0, 1, 0x43, 1][..], ScanError::MissingPrimarySlice),
        ] {
            assert_eq!(scan_access_unit(input, true), Err(expected));
        }
    }

    #[test]
    fn truncated_and_mutated_annex_b_inputs_never_panic() {
        for seed in [
            &[0, 0, 1, 0x65, 0xb8, 0x55][..],
            &[0, 0, 1, 0x41, 0, 0, 3, 0, 0x80, 0, 0, 0x28][..],
        ] {
            let mut scanner = AccessUnitScanner::new(true);
            assert!(scanner.scan(seed, true).expect("valid seed").is_some());
            for length in 0..=seed.len() {
                let input = seed.get(..length).expect("bounded truncation");
                let _scan = scanner.scan(input, true);
                let _idr = is_idr_candidate(input);
            }
            for index in 0..seed.len() {
                for byte in u8::MIN..=u8::MAX {
                    let mut input = seed.to_vec();
                    *input.get_mut(index).expect("bounded mutation") = byte;
                    let _scan = scanner.scan(&input, true);
                    let _idr = is_idr_candidate(&input);
                }
            }
        }
    }
}
