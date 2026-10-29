//! Marker detection in photos, and the detections format other detectors can
//! also write.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use image::{DynamicImage, ImageDecoder as _, ImageReader};
use kornia_apriltag::family::TagFamilyKind;
use kornia_apriltag::{AprilTagDecoder, DecodeTagsConfig};
use kornia_image::allocator::CpuAllocator;
use kornia_image::{Image, ImageSize};
use serde::{Deserialize, Serialize};
use wall_geometry::{Point, Quad};

use crate::tag::FAMILY;

/// The detections schema version this crate reads and writes.
pub const DETECTIONS_VERSION: u32 = 1;

/// Name and version of the built-in detector, recorded in its output.
pub const BUILTIN_DETECTOR: &str = "kornia-apriltag 0.1.10";

/// Markers found in one photo.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Detections {
    pub version: u32,
    pub family: String,
    /// The detector that produced the markers, for the record.
    pub detector: String,
    pub photo: Photo,
    pub markers: Vec<DetectedMarker>,
}

/// The photo markers were found in, after applying its EXIF orientation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Photo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub width: u32,
    pub height: u32,
}

/// One detected tag.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectedMarker {
    pub tag: u32,
    /// Black border corners in photo pixels, top-left, top-right,
    /// bottom-right, bottom-left in the tag's own orientation.
    pub corners: Quad,
    /// Corrected bit errors, when the detector reports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hamming: Option<u32>,
    /// Detector confidence, larger is better, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<f64>,
}

impl Detections {
    /// Parse and validate detections.
    pub fn from_json(json: &str) -> Result<Self> {
        let detections: Self = serde_json::from_str(json).context("invalid detections JSON")?;
        ensure!(
            detections.version == DETECTIONS_VERSION,
            "unsupported detections version {}; expected {DETECTIONS_VERSION}",
            detections.version
        );
        ensure!(
            detections.family == FAMILY,
            "detections use marker family {:?}; expected {FAMILY:?}",
            detections.family
        );
        for marker in &detections.markers {
            ensure!(
                marker
                    .corners
                    .corners()
                    .iter()
                    .all(|corner| corner.is_finite()),
                "tag {} has non-finite corners",
                marker.tag
            );
        }
        Ok(detections)
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// Whether `path` names a HEIF/HEIC photo, the default format of iPhone
/// cameras, by extension or by its ISO-BMFF brand.
fn is_heif(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    if matches!(extension.as_deref(), Some("heic" | "heif")) {
        return true;
    }
    let mut header = [0; 12];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok_and(|()| {
            let [_, _, _, _, f, t, y, p, brand @ ..] = header;
            [f, t, y, p] == *b"ftyp"
                && matches!(
                    &brand,
                    b"heic" | b"heix" | b"heim" | b"heis" | b"mif1" | b"msf1"
                )
        })
}

/// Open a photo with its EXIF orientation applied, so coordinates match what
/// image viewers and browsers show.
pub fn open_photo(path: &Path) -> Result<DynamicImage> {
    if is_heif(path) {
        bail!(
            "{} is a HEIC/HEIF photo, which the detectors cannot read; convert it first, for \
             example with libheif's `heif-convert {} photo.jpg`, or set the camera to \
             save JPEG",
            path.display(),
            path.display()
        );
    }
    let open = || -> Result<DynamicImage> {
        let mut decoder = ImageReader::open(path)?
            .with_guessed_format()?
            .into_decoder()?;
        let orientation = decoder.orientation()?;
        let mut image = DynamicImage::from_decoder(decoder)?;
        image.apply_orientation(orientation);
        Ok(image)
    };
    open().with_context(|| format!("reading photo {}", path.display()))
}

/// Shrink factors the built-in detector tries, finest first.
const SHRINK_FACTORS: [usize; 9] = [2, 3, 4, 6, 8, 12, 16, 24, 32];

/// The smallest longer side worth shrinking a photo to.
const MIN_SHRUNK_SIZE: usize = 200;

/// Largest corner distance between two detections of a tag that still
/// confirm each other: 2% of the tag's diagonal, and at least a pixel.
fn agreement(marker: &DetectedMarker) -> f64 {
    let [top_left, _, bottom_right, _] = marker.corners.corners();
    (0.02 * (top_left.x - bottom_right.x).hypot(top_left.y - bottom_right.y)).max(1.0)
}

fn corner_distance(a: &DetectedMarker, b: &DetectedMarker) -> f64 {
    a.corners
        .corners()
        .iter()
        .zip(b.corners.corners())
        .map(|(a, b)| (a.x - b.x).hypot(a.y - b.y))
        .fold(0.0, f64::max)
}

/// The finest of a tag's detections, finest first, that a detection at another
/// shrink factor confirms, or the finest when none is confirmed.
fn choose(detections: &[DetectedMarker]) -> Option<DetectedMarker> {
    let confirmed = detections.iter().enumerate().find(|(index, marker)| {
        detections.iter().enumerate().any(|(other, detection)| {
            other != *index && corner_distance(marker, detection) <= agreement(marker)
        })
    });
    confirmed
        .map(|(_, marker)| marker)
        .or_else(|| detections.first())
        .copied()
}

/// Find tag36h11 markers with the built-in detector.
///
/// The detector thresholds a shrunk copy of the photo over a window of about
/// 12 pixels, so it only finds tags whose cells come out roughly 1.5 to 6
/// pixels wide; a tag filling a 24-megapixel phone photo needs more shrinking
/// than one in a screenshot. Detection therefore runs at several shrink
/// factors. Corners lose accuracy with every further shrink, but the finest
/// detection occasionally misplaces a corner by several pixels, so every tag
/// keeps its finest detection that another shrink factor confirms.
///
/// Corners come from the decoded tag homography, so they follow the tag's
/// orientation rather than the order its edges were traced in.
pub fn detect(photo: &DynamicImage) -> Result<Vec<DetectedMarker>> {
    let gray = photo.to_luma8();
    let size = ImageSize {
        width: usize::try_from(gray.width())?,
        height: usize::try_from(gray.height())?,
    };
    let longest = size.width.max(size.height);
    let image = Image::<u8, 1, _>::new(size, gray.into_raw(), CpuAllocator)?;
    let factors = SHRINK_FACTORS
        .into_iter()
        .enumerate()
        .take_while(|&(index, factor)| index == 0 || longest / factor >= MIN_SHRUNK_SIZE)
        .map(|(_, factor)| factor);
    // Shrink factors are independent, so each runs on a thread of its own.
    let found = std::thread::scope(|scope| {
        let image = &image;
        let running = factors
            .map(|factor| {
                std::thread::Builder::new()
                    .name(format!("detect-{factor}"))
                    .spawn_scoped(scope, move || detect_shrunk(image, size, factor))
                    .context("starting a detector thread")
            })
            .collect::<Result<Vec<_>>>()?;
        running
            .into_iter()
            .map(|thread| {
                thread
                    .join()
                    .map_err(|_panic| anyhow::anyhow!("a detector thread panicked"))?
            })
            .collect::<Result<Vec<_>>>()
    })?;
    let mut detections: BTreeMap<u32, Vec<DetectedMarker>> = BTreeMap::new();
    for marker in found.into_iter().flatten() {
        detections.entry(marker.tag).or_default().push(marker);
    }
    Ok(detections
        .values()
        .filter_map(|detections| choose(detections))
        .collect())
}

/// The markers the built-in detector finds in `image` shrunk by `factor`.
fn detect_shrunk(
    image: &Image<u8, 1, CpuAllocator>,
    size: ImageSize,
    factor: usize,
) -> Result<Vec<DetectedMarker>> {
    let mut config = DecodeTagsConfig::new(vec![TagFamilyKind::Tag36H11]);
    config.downscale_factor = factor;
    let mut decoder = AprilTagDecoder::new(config, size)?;
    Ok(decoder
        .decode(image)?
        .into_iter()
        .map(|detection| {
            let corner = |x: f32, y: f32| {
                let point = detection.quad.homography_project(x, y);
                Point::new(f64::from(point.x), f64::from(point.y))
            };
            DetectedMarker {
                tag: u32::from(detection.id),
                corners: Quad::new([
                    corner(-1.0, -1.0),
                    corner(1.0, -1.0),
                    corner(1.0, 1.0),
                    corner(-1.0, 1.0),
                ]),
                hamming: Some(u32::from(detection.hamming)),
                margin: Some(f64::from(detection.decision_margin)),
            }
        })
        .collect())
}

/// Detect markers in the photo at `path`.
pub fn detect_file(path: &Path) -> Result<Detections> {
    let photo = open_photo(path)?;
    let (width, height) = (photo.width(), photo.height());
    Ok(Detections {
        version: DETECTIONS_VERSION,
        family: FAMILY.to_owned(),
        detector: BUILTIN_DETECTOR.to_owned(),
        photo: Photo {
            path: Some(path.display().to_string()),
            width,
            height,
        },
        markers: detect(&photo)?,
    })
}

#[cfg(test)]
mod tests {
    use wall_geometry::Size;

    use super::*;
    use crate::manifest::{Manifest, Placement};
    use crate::pattern;

    /// Render a display's pattern at `width` x `height`, soften it like a
    /// camera would, and check every tag decodes at its manifest corners.
    fn assert_pattern_decodes(width: u32, height: u32, blur: f32, tolerance: f64) {
        let screens = [("lobby".to_owned(), Size { width, height })];
        let manifest = Manifest::generate(&screens, 40, Placement::default()).expect("manifest");
        let screen = &manifest.screens[0];
        let raster = pattern::raster(screen).expect("raster");
        let photo = DynamicImage::ImageLuma8(if blur > 0.0 {
            image::imageops::blur(&raster, blur)
        } else {
            raster
        });
        let markers = detect(&photo).expect("detect");
        let tags: Vec<_> = markers.iter().map(|marker| marker.tag).collect();
        assert_eq!(tags, [40, 41, 42, 43], "{width}x{height}");
        for (detected, expected) in markers.iter().zip(&screen.markers) {
            for (found, local) in detected
                .corners
                .corners()
                .iter()
                .zip(expected.corners.corners())
            {
                let (x, y) = (local.x * f64::from(width), local.y * f64::from(height));
                assert!(
                    (found.x - x).abs() < tolerance && (found.y - y).abs() < tolerance,
                    "{width}x{height} tag {} corner {found:?} != ({x}, {y})",
                    detected.tag
                );
            }
        }
    }

    #[test]
    fn rendered_patterns_decode_at_their_manifest_corners() {
        assert_pattern_decodes(960, 540, 0.0, 0.75);
    }

    #[test]
    fn tags_too_large_for_the_first_shrink_are_still_found() {
        // Tags 650 pixels tall with soft edges, as when a display fills much
        // of a phone photo. The first shrink finds one of them; the rest come
        // from coarser shrinks, within half a percent of their size.
        assert_pattern_decodes(3840, 2160, 3.0, 3.0);
    }

    #[test]
    fn a_detection_another_shrink_confirms_wins() {
        let square = |x: f64, y: f64| DetectedMarker {
            tag: 7,
            corners: Quad::new([
                Point::new(x, y),
                Point::new(x + 100.0, y),
                Point::new(x + 100.0, y + 100.0),
                Point::new(x, y + 100.0),
            ]),
            hamming: None,
            margin: None,
        };
        // Finest first: an outlier, then two that agree within 2% of 141 px.
        let chosen = choose(&[square(4.0, 0.0), square(0.0, 0.2), square(0.0, 2.0)]);
        assert_eq!(chosen, Some(square(0.0, 0.2)));
        // Nothing agrees, or only one detection: keep the finest.
        assert_eq!(
            choose(&[square(9.0, 0.0), square(0.0, 0.0)]),
            Some(square(9.0, 0.0))
        );
        assert_eq!(choose(&[square(1.0, 1.0)]), Some(square(1.0, 1.0)));
        assert_eq!(choose(&[]), None);
    }

    #[test]
    fn heic_photos_get_a_conversion_hint() {
        let directory = tempfile::tempdir().expect("temporary directory");
        // An iPhone photo saved under a JPEG name is still recognized.
        let renamed = directory.path().join("IMG_0001.jpg");
        std::fs::write(&renamed, b"\0\0\0\x18ftypheic\0\0\0\0mif1heic").expect("write");
        for path in [directory.path().join("IMG_0001.HEIC"), renamed] {
            let error = open_photo(&path).expect_err("HEIC").to_string();
            assert!(error.contains("heif-convert"), "{error}");
        }
        let missing = open_photo(&directory.path().join("missing.jpg")).expect_err("missing");
        assert!(!missing.to_string().contains("HEIC"), "{missing}");
    }

    #[test]
    fn detections_round_trip_and_validate() {
        let detections = Detections {
            version: DETECTIONS_VERSION,
            family: FAMILY.to_owned(),
            detector: "test".to_owned(),
            photo: Photo {
                path: None,
                width: 10,
                height: 10,
            },
            markers: vec![DetectedMarker {
                tag: 3,
                corners: Quad::UNIT,
                hamming: None,
                margin: Some(12.5),
            }],
        };
        let json = detections.to_json().expect("serialize");
        assert!(!json.contains("hamming") && !json.contains("path"));
        assert_eq!(Detections::from_json(&json).expect("parse"), detections);
        let other = json.replace("tag36h11", "tag16h5");
        assert!(Detections::from_json(&other).is_err_and(|err| err.to_string().contains("family")));
    }
}
