//! Aligning displays by delaying the early ones.
//!
//! Every display's browser presents a frame when its jitter buffer releases
//! it, so displays fed separately drift apart by their network, decoding and
//! buffering. While aligning, each display's offset from the reference is
//! measured as in [`crate::drift`], and every display presenting frames
//! earlier than the latest one gets that much more jitter buffer target, in
//! steps, until all agree within [`TOLERANCE_MS`]. The smallest extra delay
//! stays zero, so aligning adds no latency beyond the latest display's.
//!
//! Presentation falls on display refreshes, so displays whose refreshes are
//! out of phase cannot agree closer than half a refresh; the tolerance is a
//! little more than that at 60 Hz, which keeps steps from hunting between two
//! equally good refreshes. Alignment is only as good as each browser's clock
//! synchronization.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::drift::Summary;

/// After a change, how long browsers take to settle before frames count.
pub const SETTLE_MS: f64 = 2000.0;

/// Frames each display must present with the reference before a step.
pub const MIN_FRAMES: usize = 45;

/// Displays presenting within this of the latest one count as aligned.
pub const TOLERANCE_MS: f64 = 10.0;

/// The fraction of a display's lag corrected per step, less than one so that
/// noisy measurements settle rather than overshoot.
const GAIN: f64 = 0.7;

/// The most extra delay one display gets.
pub const MAX_DELAY_MS: f64 = 1000.0;

/// Extra jitter buffer delay per display, and how it was reached.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Aligner {
    /// Extra delay per display, in milliseconds.
    delays: BTreeMap<String, f64>,
    /// When the delays last changed, on the server clock.
    changed_at: f64,
    /// Corrections made so far.
    pub steps: u32,
    /// Latest minus earliest display at the last measurement.
    pub spread_ms: Option<f64>,
}

impl Aligner {
    /// Start aligning at time `now`, with no extra delay.
    #[must_use]
    pub fn new(now: f64) -> Self {
        Self {
            changed_at: now,
            ..Self::default()
        }
    }

    /// Frames presented from this time on measure the current delays.
    #[must_use]
    pub fn since(&self) -> f64 {
        self.changed_at + SETTLE_MS
    }

    /// The extra delay of `display`, in milliseconds.
    #[must_use]
    pub fn delay(&self, display: &str) -> f64 {
        self.delays.get(display).copied().unwrap_or(0.0)
    }

    #[must_use]
    pub fn delays(&self) -> &BTreeMap<String, f64> {
        &self.delays
    }

    /// Whether the last measurement found every display within the
    /// tolerance.
    #[must_use]
    pub fn aligned(&self) -> bool {
        self.spread_ms.is_some_and(|spread| spread <= TOLERANCE_MS)
    }

    /// Correct the delays from `summary`, made of frames presented since
    /// [`Aligner::since`], at time `now`; returns whether they changed.
    pub fn step(&mut self, summary: &Summary, now: f64) -> bool {
        if now < self.since() {
            return false;
        }
        let offsets: Vec<(&str, f64)> = summary
            .displays
            .iter()
            .filter_map(|display| {
                let offset = display.offset_ms?;
                (offset.samples >= MIN_FRAMES).then_some((display.id.as_str(), offset.median))
            })
            .collect();
        if offsets.len() < 2 {
            return false;
        }
        let latest = offsets
            .iter()
            .map(|(_, offset)| *offset)
            .fold(f64::NEG_INFINITY, f64::max);
        let earliest = offsets
            .iter()
            .map(|(_, offset)| *offset)
            .fold(f64::INFINITY, f64::min);
        self.spread_ms = Some(latest - earliest);
        let mut changed = false;
        for (id, offset) in &offsets {
            let lag = latest - offset;
            if lag > TOLERANCE_MS {
                *self.delays.entry((*id).to_owned()).or_default() += GAIN * lag;
                changed = true;
            }
        }
        if !changed {
            return false;
        }
        // Delays that all grew together only add latency.
        let floor = offsets
            .iter()
            .map(|(id, _)| self.delay(id))
            .fold(f64::INFINITY, f64::min);
        for (id, _) in &offsets {
            if let Some(delay) = self.delays.get_mut(*id) {
                *delay = (*delay - floor).clamp(0.0, MAX_DELAY_MS);
            }
        }
        self.changed_at = now;
        self.steps += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drift::{DisplaySummary, Stats};

    /// A summary whose displays present `offsets` after the first, from
    /// `frames` frames each.
    fn summary(offsets: &[(&str, f64)], frames: usize) -> Summary {
        Summary {
            window_seconds: 20.0,
            reference: offsets.first().map(|(id, _)| (*id).to_owned()),
            spread_ms: None,
            displays: offsets
                .iter()
                .map(|(id, offset)| DisplaySummary {
                    id: (*id).to_owned(),
                    frames,
                    unreadable: 0,
                    clock_rtt_ms: None,
                    pipeline_ms: None,
                    jitter_buffer_ms: None,
                    latency_ms: None,
                    receive_ms: None,
                    offset_ms: Some(Stats {
                        samples: frames,
                        min: *offset,
                        median: *offset,
                        p95: *offset,
                        max: *offset,
                    }),
                    offset_change_ms: None,
                })
                .collect(),
        }
    }

    /// Displays presenting `natural` milliseconds after a common time,
    /// delayed further by the aligner, as measured against the first.
    fn measured(natural: &[(&'static str, f64)], aligner: &Aligner) -> Vec<(&'static str, f64)> {
        let shown = |(id, at): &(&str, f64)| at + aligner.delay(id);
        let base = natural.first().map_or(0.0, shown);
        natural
            .iter()
            .map(|display| (display.0, shown(display) - base))
            .collect()
    }

    #[test]
    fn early_displays_are_delayed_until_all_agree() {
        // `b` presents 40 ms before `a`, `c` 25 ms after it.
        let natural = [("a", 0.0), ("b", -40.0), ("c", 25.0)];
        let mut aligner = Aligner::new(0.0);
        let mut now = 0.0;
        for _ in 0..10 {
            now += 3000.0;
            aligner.step(&summary(&measured(&natural, &aligner), 60), now);
        }
        assert!(aligner.aligned(), "{aligner:?}");
        // The latest display keeps no extra delay; the others catch up to it.
        assert!(aligner.delay("c").abs() < 1e-9);
        assert!((aligner.delay("a") - 25.0).abs() <= TOLERANCE_MS);
        assert!((aligner.delay("b") - 65.0).abs() <= TOLERANCE_MS);
        let steps = aligner.steps;
        aligner.step(&summary(&measured(&natural, &aligner), 60), now + 3000.0);
        assert_eq!(aligner.steps, steps, "aligned displays need no step");
    }

    #[test]
    fn delays_shrink_when_the_late_display_catches_up() {
        let mut aligner = Aligner::new(0.0);
        aligner.step(&summary(&[("a", 0.0), ("b", 50.0)], 60), 3000.0);
        assert!((aligner.delay("a") - 35.0).abs() < 1e-9);
        // `b` got faster on its own: now `a`, delayed, is the late one, and
        // its delay shrinks rather than `b`'s growing.
        aligner.step(&summary(&[("a", 0.0), ("b", -30.0)], 60), 6000.0);
        assert!((aligner.delay("a") - 14.0).abs() < 1e-9, "{aligner:?}");
        assert!(aligner.delay("b").abs() < 1e-9, "{aligner:?}");
    }

    #[test]
    fn steps_wait_for_settled_frames_from_two_displays() {
        let mut aligner = Aligner::new(0.0);
        let unaligned = summary(&[("a", 0.0), ("b", 50.0)], 60);
        assert!(!aligner.step(&unaligned, SETTLE_MS - 1.0), "still settling");
        assert!(!aligner.step(&summary(&[("a", 0.0), ("b", 50.0)], 10), 5000.0));
        assert!(!aligner.step(&summary(&[("a", 0.0)], 60), 5000.0));
        assert!(aligner.step(&unaligned, 5000.0));
        assert!(!aligner.step(&unaligned, 5000.0 + SETTLE_MS - 1.0));
        assert!(aligner.since() > 5000.0);
    }
}
