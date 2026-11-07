//! `AprilTag` `tag36h11` bitmaps, built from the tables the detector decodes.

use std::sync::LazyLock;

use kornia_apriltag::family::TagFamily;
use kornia_apriltag::family::tag36h11::CODE_DATA;

/// Marker family name used in manifests and detections.
pub const FAMILY: &str = "tag36h11";

/// Cells across a rendered tag, including its outer white ring.
pub const TAG_CELLS: usize = 10;

/// Cells across the black border square whose corners locate a tag.
pub const BORDER_CELLS: usize = 8;

/// [`TAG_CELLS`] as a coordinate span.
pub const TAG_SPAN: f64 = 10.0;

/// [`BORDER_CELLS`] as a coordinate span.
pub const BORDER_SPAN: f64 = 8.0;

/// Number of distinct tags in the family.
pub const TAG_COUNT: usize = CODE_DATA.len();

/// A rendered tag, row-major, `true` for white.
pub type Cells = [[bool; TAG_CELLS]; TAG_CELLS];

#[expect(clippy::expect_used, reason = "built-in tag36h11 tables are valid")]
static FAMILY_TABLES: LazyLock<TagFamily> =
    LazyLock::new(|| TagFamily::tag36_h11().expect("built-in tag36h11 tables are valid"));

/// The cells of `tag`, or `None` beyond [`TAG_COUNT`].
///
/// The layout follows the reference `apriltag_to_image`: a white outer ring,
/// a black border ring, and the 6×6 data bits inside, most significant first.
#[must_use]
pub fn cells(tag: u32) -> Option<Cells> {
    let family = &*FAMILY_TABLES;
    if family.total_width != TAG_CELLS
        || family.width_at_border != BORDER_CELLS
        || family.reversed_border
    {
        return None;
    }
    let code = *family.code_data.get(usize::try_from(tag).ok()?)?;
    let outer = |index: usize| index == 0 || index == TAG_CELLS - 1;
    let mut cells = [[false; TAG_CELLS]; TAG_CELLS];
    for (y, row) in cells.iter_mut().enumerate() {
        for (x, cell) in row.iter_mut().enumerate() {
            *cell = outer(x) || outer(y);
        }
    }
    // Data bits sit one cell inside the black border.
    let offset = (TAG_CELLS - BORDER_CELLS) / 2;
    for (bit, (x, y)) in family.bit_x.iter().zip(&family.bit_y).enumerate() {
        let shift = family.nbits.checked_sub(bit + 1)?;
        let x = usize::try_from(*x).ok()? + offset;
        let y = usize::try_from(*y).ok()? + offset;
        *cells.get_mut(y)?.get_mut(x)? = (code >> shift) & 1 == 1;
    }
    Some(cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_match_cell_counts() {
        assert_eq!(TAG_SPAN.to_string(), TAG_CELLS.to_string());
        assert_eq!(BORDER_SPAN.to_string(), BORDER_CELLS.to_string());
    }

    #[test]
    fn tags_have_rings_and_distinct_codes() {
        let first = cells(0).expect("tag 0");
        for index in 0..TAG_CELLS {
            for (x, y) in [(index, 0), (0, index), (index, 9), (9, index)] {
                assert!(first[y][x], "outer ring cell ({x}, {y}) is white");
            }
        }
        for index in 1..TAG_CELLS - 1 {
            for (x, y) in [(index, 1), (1, index), (index, 8), (8, index)] {
                assert!(!first[y][x], "border cell ({x}, {y}) is black");
            }
        }
        assert_ne!(cells(0), cells(1));
        assert!(cells(u32::try_from(TAG_COUNT).expect("count") - 1).is_some());
        assert!(cells(u32::try_from(TAG_COUNT).expect("count")).is_none());
    }
}
