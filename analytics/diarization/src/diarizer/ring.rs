//! Recent audio, indexed by sample on the running-time timeline.

use std::collections::VecDeque;
use std::ops::Range;

/// Recent samples; `first` is the timeline index of `samples[0]`.
#[derive(Debug, Default)]
pub struct Ring {
    first: u64,
    samples: VecDeque<f32>,
    started: bool,
}

impl Ring {
    /// Index one past the last sample.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.first + self.samples.len() as u64
    }

    #[must_use]
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Starts again, empty, at `index`.
    fn restart(&mut self, index: u64) {
        self.started = true;
        self.first = index;
        self.samples.clear();
    }

    /// Adds samples that start at `at`, keeping at most `max`. A gap of more
    /// than `jitter` samples is filled with silence, and samples more than
    /// `jitter` before the end overlap what is buffered and are skipped;
    /// otherwise the samples continue the buffer, so timestamp rounding does
    /// not tear it. A jump of more than `max` either way, such as a
    /// restarted source, starts the buffer again at `at`, as everything
    /// before it would be dropped anyway. At a `discont`, a step back of more
    /// than `jitter` replaces what is buffered from `at` on, and starts the
    /// buffer again if `at` is before it.
    /// Returns whether a started buffer started again.
    pub fn push(
        &mut self,
        at: u64,
        samples: &[f32],
        jitter: u64,
        max: usize,
        discont: bool,
    ) -> bool {
        let end = self.end();
        let max_jump = max as u64;
        let back = discont && at.saturating_add(jitter) < end;
        let jumped = self.started
            && (at > end.saturating_add(max_jump)
                || at.saturating_add(max_jump) < end
                || back && at < self.first);
        if !self.started || jumped {
            self.restart(at);
        } else if back {
            self.samples
                .truncate(usize::try_from(at - self.first).unwrap_or(usize::MAX));
        }
        let end = self.end();
        let skip = if at > end + jitter {
            let missing = usize::try_from(at - end).unwrap_or(usize::MAX);
            self.samples.extend(std::iter::repeat_n(0.0, missing));
            0
        } else if at + jitter < end {
            usize::try_from(end - at).unwrap_or(usize::MAX)
        } else {
            0
        };
        self.samples
            .extend(samples.get(skip..).unwrap_or_default().iter().copied());
        self.limit(max);
        jumped
    }

    /// Fills silence up to `index`, as for a gap with no audio buffers,
    /// keeping at most `max`.
    pub fn fill_to(&mut self, index: u64, max: usize) {
        let end = self.end();
        if !self.started || index > end.saturating_add(max as u64) {
            self.restart(index);
        }
        let end = self.end();
        if index > end {
            let missing = usize::try_from(index - end).unwrap_or(usize::MAX);
            self.samples.extend(std::iter::repeat_n(0.0, missing));
        }
        self.limit(max);
    }

    /// Drops samples before `index`.
    pub fn discard_before(&mut self, index: u64) {
        let count = index.saturating_sub(self.first);
        let count = usize::try_from(count)
            .unwrap_or(usize::MAX)
            .min(self.samples.len());
        self.samples.drain(..count);
        self.first += count as u64;
    }

    /// Drops the oldest samples beyond `max`; returns how many.
    pub fn limit(&mut self, max: usize) -> usize {
        let over = self.samples.len().saturating_sub(max);
        self.discard_before(self.first + over as u64);
        over
    }

    /// The buffered part of `range`, and whether its start was already
    /// dropped.
    #[must_use]
    pub fn slice(&self, range: Range<u64>) -> (Vec<f32>, bool) {
        let start = range.start.max(self.first);
        let end = range.end.min(self.end());
        let dropped = range.start < self.first;
        if start >= end {
            return (Vec::new(), dropped);
        }
        let from = usize::try_from(start - self.first).unwrap_or(usize::MAX);
        let to = usize::try_from(end - self.first).unwrap_or(usize::MAX);
        (self.samples.range(from..to).copied().collect(), dropped)
    }
}

#[cfg(test)]
mod tests {
    use super::Ring;

    #[test]
    fn keeps_samples_on_their_timeline() {
        let mut ring = Ring::default();
        ring.push(100, &[1.0, 2.0, 3.0], 2, 100, false);
        assert_eq!((ring.first, ring.end()), (100, 103));
        // Within jitter: continues the buffer.
        ring.push(104, &[4.0], 2, 100, false);
        assert_eq!(ring.end(), 104);
        // A gap is filled with silence.
        ring.push(110, &[5.0, 6.0], 2, 100, false);
        assert_eq!(
            ring.slice(103..112).0,
            [4.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 5.0, 6.0]
        );
        // An overlap is skipped.
        ring.push(108, &[9.0, 9.0, 9.0, 9.0, 7.0], 2, 100, false);
        assert_eq!(ring.slice(110..113).0, [5.0, 6.0, 7.0]);
        ring.fill_to(115, 100);
        assert_eq!(ring.end(), 115);
        ring.discard_before(102);
        assert_eq!(ring.slice(100..103), (vec![3.0], true));
        assert_eq!(ring.limit(5), 8);
        assert_eq!((ring.first, ring.end()), (110, 115));
        assert_eq!(ring.slice(120..130), (Vec::new(), false));
        assert_eq!(ring.limit(10), 0);
    }

    #[test]
    fn jumps_beyond_the_maximum_start_again() {
        let mut ring = Ring::default();
        assert!(!ring.push(1000, &[1.0; 10], 2, 50, false));
        // Far ahead: no fill of the whole distance.
        assert!(ring.push(u64::MAX / 2, &[2.0; 3], 2, 50, false));
        assert_eq!((ring.first, ring.samples.len()), (u64::MAX / 2, 3));
        // Far back: the new timeline is kept, not skipped.
        assert!(ring.push(500, &[3.0; 4], 2, 50, false));
        assert_eq!((ring.first, ring.end()), (500, 504));
        assert_eq!(ring.slice(500..504).0, [3.0; 4]);
        // A gap within the maximum is filled; one beyond it starts again.
        ring.fill_to(540, 50);
        assert_eq!((ring.first, ring.end()), (500, 540));
        ring.fill_to(1_000_000, 50);
        assert_eq!((ring.first, ring.end()), (1_000_000, 1_000_000));
        // The maximum holds after a fill.
        ring.push(1_000_000, &[4.0; 40], 2, 50, false);
        ring.fill_to(1_000_049, 50);
        ring.fill_to(1_000_060, 50);
        assert_eq!((ring.first, ring.end()), (1_000_010, 1_000_060));
    }

    #[test]
    fn a_step_back_at_a_discontinuity_replaces_audio() {
        let mut ring = Ring::default();
        ring.push(100, &[1.0; 10], 2, 50, false);
        // Within jitter it still continues the buffer.
        assert!(!ring.push(109, &[2.0], 2, 50, true));
        assert_eq!(ring.end(), 111);
        // Further back, the new samples replace the old ones from there on.
        assert!(!ring.push(105, &[3.0; 2], 2, 50, true));
        assert_eq!((ring.first, ring.end()), (100, 107));
        assert_eq!(ring.slice(100..107).0, [1.0, 1.0, 1.0, 1.0, 1.0, 3.0, 3.0]);
        // Before the buffer, it starts again.
        assert!(ring.push(90, &[4.0; 2], 2, 50, true));
        assert_eq!((ring.first, ring.end()), (90, 92));
    }
}
