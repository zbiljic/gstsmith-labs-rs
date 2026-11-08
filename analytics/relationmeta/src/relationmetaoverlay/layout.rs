//! Colours and label placement, without any drawing.

/// The six pastels of the Cactus web demo, as `0xRRGGBB`.
pub const PASTELS: [u32; 6] = [
    0x7d_d3e8, 0xb6_d98c, 0xf2_c879, 0xc4_a5f5, 0xf1_a8bd, 0xf5_aa91,
];

/// Distinct colours for track ids under the `tracking` palette.
pub const TRACKING: [u32; 12] = [
    0xe6_194b, 0x3c_b44b, 0xff_e119, 0x43_63d8, 0xf5_8231, 0x91_1eb4, 0x42_d4f4, 0xf0_32e6,
    0xbf_ef45, 0xfa_bed4, 0x46_9990, 0xdc_beff,
];

pub const FOREGROUND: u32 = 0xfa_fafa;
pub const BACKGROUND: u32 = 0x0a_0a0a;
pub const PREDICATE_BORDER: u32 = 0x33_3333;

/// A Java-style string hash over chars. The demo hashes UTF-16 units, so
/// names outside the Basic Multilingual Plane may pick another colour.
#[must_use]
pub fn name_hash(name: &str) -> i32 {
    name.chars().fold(0i32, |hash, c| {
        #[expect(
            clippy::cast_possible_wrap,
            reason = "a char is at most 0x10FFFF, well inside i32"
        )]
        hash.wrapping_mul(31).wrapping_add(u32::from(c) as i32)
    })
}

fn pick(palette: &[u32], key: u64) -> u32 {
    let len = u64::try_from(palette.len()).unwrap_or(1).max(1);
    usize::try_from(key % len)
        .ok()
        .and_then(|index| palette.get(index))
        .copied()
        .unwrap_or(FOREGROUND)
}

/// The class colour of a label.
#[must_use]
pub fn label_colour(name: &str) -> u32 {
    pick(&PASTELS, u64::from(name_hash(name).unsigned_abs()))
}

/// The colour of a track id.
#[must_use]
pub fn track_colour(track: u64) -> u32 {
    pick(&TRACKING, track)
}

/// Splits `0xRRGGBB` into 0..1 components.
#[must_use]
pub fn rgb(colour: u32) -> (f64, f64, f64) {
    let channel = |shift: u32| f64::from((colour >> shift) & 0xff) / 255.0;
    (channel(16), channel(8), channel(0))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }

    #[must_use]
    pub fn centre(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Moves the rectangle inside a `width` × `height` frame where it fits.
    #[must_use]
    pub fn clamped(self, width: f64, height: f64) -> Self {
        Self {
            x: self.x.min(width - self.w).max(0.0),
            y: self.y.min(height - self.h).max(0.0),
            ..self
        }
    }
}

/// How many label-height steps a chip may move to avoid another.
const MAX_STEPS: i32 = 6;

/// Places one label chip per object, in order, as the demo does: above the
/// box's top-left corner, right-aligned when the box starts in the right
/// half, clamped inside the frame, and on overlap stepped up and down in
/// alternating label-height steps (up 1, down 1, up 2, ...). A chip that
/// finds no free place stays at its clamped first position.
#[must_use]
pub fn place_chips(frame: (f64, f64), objects: &[(Rect, (f64, f64))]) -> Vec<Rect> {
    let (width, height) = frame;
    let mut placed: Vec<Rect> = Vec::with_capacity(objects.len());
    for &(object, (w, h)) in objects {
        let x = if object.x > width / 2.0 {
            object.x + object.w - w
        } else {
            object.x
        };
        let first = Rect {
            x,
            y: object.y - h,
            w,
            h,
        }
        .clamped(width, height);
        let free = (0..=2 * MAX_STEPS)
            .map(|step| {
                let distance = (step + 1) / 2;
                let direction = if step % 2 == 1 { -1 } else { 1 };
                Rect {
                    y: object.y - h + f64::from(direction * distance) * h,
                    ..first
                }
                .clamped(width, height)
            })
            .find(|candidate| placed.iter().all(|other| !candidate.overlaps(other)));
        placed.push(free.unwrap_or(first));
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn colours_are_stable_per_name_and_track() {
        assert_eq!(name_hash("man"), 107_866);
        assert_eq!(label_colour("man"), label_colour("man"));
        assert!(PASTELS.contains(&label_colour("framed picture")));
        let colours: std::collections::BTreeSet<_> = ["man", "cup", "dog", "car", "tree", "poster"]
            .map(label_colour)
            .into();
        assert!(colours.len() > 1, "names spread over the palette");
        assert_eq!(track_colour(0), TRACKING[0]);
        assert_eq!(track_colour(13), TRACKING[1]);
        assert_eq!(rgb(0xff_8000), (1.0, 128.0 / 255.0, 0.0));
    }

    #[test]
    fn chips_sit_above_the_box_and_align_right_in_the_right_half() {
        let chips = place_chips(
            (640.0, 480.0),
            &[
                (rect(100.0, 100.0, 50.0, 50.0), (40.0, 20.0)),
                (rect(400.0, 300.0, 100.0, 50.0), (40.0, 20.0)),
            ],
        );
        assert_eq!(
            chips,
            [
                rect(100.0, 80.0, 40.0, 20.0),
                rect(460.0, 280.0, 40.0, 20.0)
            ]
        );
    }

    #[test]
    fn chips_are_clamped_inside_the_frame() {
        let chips = place_chips(
            (640.0, 480.0),
            &[
                (rect(0.0, 0.0, 50.0, 50.0), (40.0, 20.0)),
                (rect(630.0, 470.0, 10.0, 10.0), (40.0, 20.0)),
            ],
        );
        assert_eq!(
            chips,
            [rect(0.0, 0.0, 40.0, 20.0), rect(600.0, 450.0, 40.0, 20.0)]
        );
    }

    #[test]
    fn overlapping_chips_step_up_then_down() {
        let label = (40.0, 20.0);
        let chips = place_chips(
            (640.0, 480.0),
            &[
                (rect(100.0, 100.0, 50.0, 50.0), label),
                (rect(110.0, 100.0, 50.0, 50.0), label),
                (rect(105.0, 100.0, 50.0, 50.0), label),
            ],
        );
        // The second steps up one label height; the third tries up (taken),
        // then down one step.
        assert_eq!(
            chips,
            [
                rect(100.0, 80.0, 40.0, 20.0),
                rect(110.0, 60.0, 40.0, 20.0),
                rect(105.0, 100.0, 40.0, 20.0),
            ]
        );
    }
}
