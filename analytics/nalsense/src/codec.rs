use std::error::Error as StdError;
use std::fmt;

use crate::analyzer::PictureType;
use crate::{h264, h265};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    H264,
    H265,
}

impl Codec {
    pub(crate) fn from_media_type(media_type: &str) -> Option<Self> {
        match media_type {
            "video/x-h264" => Some(Self::H264),
            "video/x-h265" => Some(Self::H265),
            _ => None,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::H265 => "H.265",
        }
    }

    pub(crate) fn is_idr_candidate(self, input: &[u8]) -> Result<bool, ScanError> {
        match self {
            Self::H264 => h264::is_idr_candidate(input).map_err(ScanError::H264),
            Self::H265 => h265::is_idr_candidate(input).map_err(ScanError::H265),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AccessUnit {
    pub(crate) encoded_vcl_bytes: u32,
    pub(crate) picture_type: PictureType,
    pub(crate) is_reference_picture: bool,
    pub(crate) luma_qp: Option<i32>,
    pub(crate) is_keyframe: bool,
    pub(crate) is_idr: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanError {
    H264(h264::ScanError),
    H265(h265::ScanError),
}

impl fmt::Display for ScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::H264(error) => error.fmt(formatter),
            Self::H265(error) => error.fmt(formatter),
        }
    }
}

impl StdError for ScanError {}

#[derive(Debug)]
pub(crate) enum AccessUnitScanner {
    H264(h264::AccessUnitScanner),
    H265(h265::AccessUnitScanner),
}

impl AccessUnitScanner {
    pub(crate) fn new(codec: Codec, qp_diagnostics: bool) -> Self {
        match codec {
            Codec::H264 => Self::H264(h264::AccessUnitScanner::new(qp_diagnostics)),
            Codec::H265 => Self::H265(h265::AccessUnitScanner::new()),
        }
    }

    pub(crate) const fn codec(&self) -> Codec {
        match self {
            Self::H264(_) => Codec::H264,
            Self::H265(_) => Codec::H265,
        }
    }

    pub(crate) fn reset(&mut self) {
        match self {
            Self::H264(scanner) => scanner.reset(),
            Self::H265(scanner) => scanner.reset(),
        }
    }

    pub(crate) fn scan(
        &mut self,
        input: &[u8],
        delta_unit: bool,
    ) -> Result<Option<AccessUnit>, ScanError> {
        match self {
            Self::H264(scanner) => scanner.scan(input, delta_unit).map_err(ScanError::H264),
            Self::H265(scanner) => scanner.scan(input, delta_unit).map_err(ScanError::H265),
        }
    }
}

pub(crate) fn parsed_au_caps() -> gst::Caps {
    let structure = |media_type| {
        gst::Structure::builder(media_type)
            .field("parsed", true)
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build()
    };
    gst::Caps::builder_full()
        .structure(structure("video/x-h264"))
        .structure(structure("video/x-h265"))
        .build()
}
