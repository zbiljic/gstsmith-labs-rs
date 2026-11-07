//! Presentation drift between independently playing displays.
//!
//! While measuring, the pipeline records when it finished every output frame,
//! by frame number. Each browser tells which frame it presents, from the RTP
//! timestamp its video frame callback reports or, where it reports none, from
//! a frame code the pipeline then draws into every output
//! ([`crate::framecode`]), and reports when it expects the frame on screen,
//! converted to the server clock by its own clock synchronization. All times
//! are milliseconds since the server started.
//!
//! The browsers' times are estimates of when they hand a frame to the
//! display: they do not include the panel's own latency, and they are only as
//! good as the clock synchronization, about half the best round trip.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::framecode;

/// Samples older than this, relative to the newest, are dropped.
pub const WINDOW_MS: f64 = 20_000.0;

/// What one browser reports about the frames it presented.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameReport {
    /// Best clock synchronization round trip so far, in milliseconds.
    pub rtt: Option<f64>,
    /// The browser's mean jitter buffer delay since the last report.
    #[serde(default)]
    pub jitter_buffer: Option<f64>,
    /// Frames presented without a readable code since the last report.
    #[serde(default)]
    pub unreadable: u64,
    /// `[code, shown, received]`: the frame code, when the browser expects
    /// the frame on screen, and when its last packet arrived, if known.
    #[serde(default)]
    pub frames: Vec<(u64, f64, Option<f64>)>,
    /// `[rtp, shown, received]` for frames told apart by their RTP timestamp.
    #[serde(default)]
    pub rtp: Vec<(u32, f64, Option<f64>)>,
    /// Whether the browser reports RTP timestamps; one that does not needs
    /// frame codes drawn.
    #[serde(default)]
    pub rtp_timestamps: Option<bool>,
}

/// RTP's video clock rate.
const RTP_CLOCK_RATE: f64 = 90_000.0;

/// RTP timestamps wrap at 2³².
const RTP_WRAP: f64 = 4_294_967_296.0;

/// The frame, at `fps`, whose RTP timestamp is `rtp`, of the frames near
/// `latest`. The pipeline's RTP timestamps are its running time at 90 kHz,
/// wrapping about every 13 hours.
#[must_use]
pub fn rtp_frame(rtp: u32, latest: u64, fps: u32) -> u64 {
    let fps = f64::from(fps.max(1));
    #[expect(
        clippy::cast_precision_loss,
        reason = "frame numbers stay far below 2^52"
    )]
    let expected = latest as f64 * RTP_CLOCK_RATE / fps;
    let turns = ((expected - f64::from(rtp)) / RTP_WRAP).round();
    let timestamp = f64::from(rtp) + turns * RTP_WRAP;
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the frame number is clamped to be non-negative"
    )]
    let frame = (timestamp * fps / RTP_CLOCK_RATE).round().max(0.0) as u64;
    frame
}

#[derive(Clone, Copy, Debug)]
struct Shown {
    at: f64,
    received: Option<f64>,
}

#[derive(Default)]
struct DisplayLog {
    /// When the pipeline stamped each frame.
    stamped: BTreeMap<u64, f64>,
    /// How long each stamped frame spent in the pipeline before stamping.
    delays: BTreeMap<u64, f64>,
    shown: BTreeMap<u64, Shown>,
    rtt: Option<f64>,
    jitter_buffer: Option<f64>,
    unreadable: u64,
    /// Codes reported before any frame was stamped, which cannot be placed.
    unplaced: u64,
    rtp_timestamps: Option<bool>,
    /// When the browser's latest report arrived, on the server's clock.
    reported: Option<f64>,
}

/// Stamped and presented frames per display.
#[derive(Default)]
pub struct Measurement {
    displays: HashMap<String, DisplayLog>,
}

/// Order statistics of a set of values.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Stats {
    pub samples: usize,
    pub min: f64,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
}

impl Stats {
    fn of(mut values: Vec<f64>) -> Option<Self> {
        values.retain(|value| value.is_finite());
        values.sort_by(f64::total_cmp);
        let rank = |fraction: f64| {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "the rank lies in 0..values.len()"
            )]
            let index = (fraction * (values.len() - 1) as f64).round() as usize;
            values.get(index).copied()
        };
        Some(Self {
            samples: values.len(),
            min: *values.first()?,
            median: rank(0.5)?,
            p95: rank(0.95)?,
            max: *values.last()?,
        })
    }
}

/// One display's measurement.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DisplaySummary {
    pub id: String,
    /// Frames presented within the window.
    pub frames: usize,
    /// Frames presented without a readable code, since measuring started.
    pub unreadable: u64,
    /// Best clock synchronization round trip; times are uncertain by about
    /// half of it.
    pub clock_rtt_ms: Option<f64>,
    /// From a frame's capture time to stamping it, after the warp: time spent
    /// in the pipeline before encoding.
    pub pipeline_ms: Option<Stats>,
    /// The browser's latest mean jitter buffer delay.
    pub jitter_buffer_ms: Option<f64>,
    /// From stamping a frame to its expected presentation.
    pub latency_ms: Option<Stats>,
    /// From stamping a frame to receiving it, when the browser reports it.
    pub receive_ms: Option<Stats>,
    /// Presentation time minus the reference display's, for frames both
    /// presented; positive is later.
    pub offset_ms: Option<Stats>,
    /// How far the offset moved across the window: its median over the last
    /// quarter of the window minus its median over the first. Presentation
    /// times fall on display refreshes, so a display that slipped by one frame
    /// shows as one refresh interval, about 16.7 ms at 60 Hz.
    pub offset_change_ms: Option<f64>,
}

/// All displays' measurements.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Summary {
    pub window_seconds: f64,
    /// The display others are compared with: the first with frames.
    pub reference: Option<String>,
    /// Latest minus earliest presentation of each frame every measured display
    /// presented.
    pub spread_ms: Option<Stats>,
    pub displays: Vec<DisplaySummary>,
}

/// Fewest offsets for which [`offset_change`] is reported.
const MIN_CHANGE_SAMPLES: usize = 8;

/// Median offset over the last quarter of `offsets`' time span minus the
/// median over the first, for `(time, offset)` pairs.
fn offset_change(offsets: &[(f64, f64)]) -> Option<f64> {
    if offsets.len() < MIN_CHANGE_SAMPLES {
        return None;
    }
    let first = offsets.iter().map(|(time, _)| *time).reduce(f64::min)?;
    let last = offsets.iter().map(|(time, _)| *time).reduce(f64::max)?;
    let quarter = (last - first) / 4.0;
    let median = |keep: &dyn Fn(f64) -> bool| {
        let values = offsets
            .iter()
            .filter(|(time, _)| keep(*time))
            .map(|(_, offset)| *offset)
            .collect();
        Stats::of(values).map(|stats| stats.median)
    };
    Some(median(&|time| time >= last - quarter)? - median(&|time| time <= first + quarter)?)
}

impl Measurement {
    /// Forget everything, as when measuring starts again.
    pub fn clear(&mut self) {
        self.displays.clear();
    }

    /// The pipeline stamped `frame` for `display` at time `at`, `delay`
    /// milliseconds after the frame's capture time.
    pub fn stamped(&mut self, display: &str, frame: u64, at: f64, delay: Option<f64>) {
        let log = self.displays.entry(display.to_owned()).or_default();
        log.stamped.insert(frame, at);
        log.stamped.retain(|_, time| *time >= at - WINDOW_MS);
        if let Some(delay) = delay {
            log.delays.insert(frame, delay);
        }
        if let Some(&oldest) = log.stamped.keys().next() {
            log.delays.retain(|frame, _| *frame >= oldest);
        }
    }

    /// A browser reported the frames it presented on `display`, of a stream
    /// at `fps` frames per second.
    pub fn shown(&mut self, display: &str, report: &FrameReport, fps: u32) {
        let log = self.displays.entry(display.to_owned()).or_default();
        log.rtt = report.rtt.or(log.rtt);
        log.jitter_buffer = report.jitter_buffer.or(log.jitter_buffer);
        log.unreadable += report.unreadable;
        log.rtp_timestamps = report.rtp_timestamps.or(log.rtp_timestamps);
        // Codes and RTP timestamps wrap; a presented frame was stamped
        // shortly before the newest stamp, so the nearest frame with that code
        // or timestamp is the one.
        let Some(&latest) = log.stamped.keys().next_back() else {
            log.unplaced += u64::try_from(report.frames.len() + report.rtp.len()).unwrap_or(0);
            return;
        };
        let coded = report
            .frames
            .iter()
            .map(|&(code, at, received)| (framecode::unwrap(code, latest), at, received));
        let timed = report
            .rtp
            .iter()
            .map(|&(rtp, at, received)| (rtp_frame(rtp, latest, fps), at, received));
        for (frame, at, received) in coded.chain(timed) {
            if at.is_finite() {
                log.shown.insert(frame, Shown { at, received });
            }
        }
        if let Some(newest) = log.shown.values().map(|shown| shown.at).reduce(f64::max) {
            log.shown.retain(|_, shown| shown.at >= newest - WINDOW_MS);
        }
    }

    /// A report from `display`'s browser arrived at `at`, whatever it held.
    pub fn reported(&mut self, display: &str, at: f64) {
        self.displays
            .entry(display.to_owned())
            .or_default()
            .reported = Some(at);
    }

    /// When `display`'s browser last reported.
    #[must_use]
    pub fn last_report(&self, display: &str) -> Option<f64> {
        self.displays.get(display)?.reported
    }

    /// When `display` last presented a frame its browser reported.
    #[must_use]
    pub fn last_shown(&self, display: &str) -> Option<f64> {
        self.displays
            .get(display)?
            .shown
            .values()
            .map(|shown| shown.at)
            .reduce(f64::max)
    }

    /// When the pipeline last stamped a frame for `display`.
    #[must_use]
    pub fn last_stamped(&self, display: &str) -> Option<f64> {
        self.displays
            .get(display)?
            .stamped
            .values()
            .copied()
            .reduce(f64::max)
    }

    /// Whether any of `displays` needs frame codes drawn to tell frames
    /// apart: its browser reported that it gives no RTP timestamps, or it has
    /// not reported yet.
    #[must_use]
    pub fn needs_codes(&self, displays: &[String]) -> bool {
        displays.iter().any(|id| {
            self.displays
                .get(id)
                .is_none_or(|log| log.rtp_timestamps != Some(true))
        })
    }

    /// Summarize `displays`, in order; the first with frames is the reference.
    #[must_use]
    pub fn summary(&self, displays: &[String]) -> Summary {
        self.summary_since(displays, f64::NEG_INFINITY)
    }

    /// Summarize `displays` over the frames presented from time `since`.
    #[must_use]
    pub fn summary_since(&self, displays: &[String], since: f64) -> Summary {
        let empty = DisplayLog::default();
        let logs: Vec<(&String, &DisplayLog, BTreeMap<u64, Shown>)> = displays
            .iter()
            .map(|id| {
                let log = self.displays.get(id).unwrap_or(&empty);
                let shown = log
                    .shown
                    .iter()
                    .filter(|(_, shown)| shown.at >= since)
                    .map(|(frame, shown)| (*frame, *shown))
                    .collect();
                (id, log, shown)
            })
            .collect();
        let reference = logs.iter().find(|(_, _, shown)| !shown.is_empty());
        let measured: Vec<&BTreeMap<u64, Shown>> = logs
            .iter()
            .filter(|(_, _, shown)| !shown.is_empty())
            .map(|(_, _, shown)| shown)
            .collect();
        let spread = measured.first().map(|first| {
            first
                .keys()
                .filter_map(|frame| {
                    let times: Vec<f64> = measured
                        .iter()
                        .map(|shown| shown.get(frame).map(|shown| shown.at))
                        .collect::<Option<_>>()?;
                    let earliest = times.iter().copied().reduce(f64::min)?;
                    let latest = times.iter().copied().reduce(f64::max)?;
                    (times.len() > 1).then_some(latest - earliest)
                })
                .collect::<Vec<_>>()
        });
        let summaries = logs
            .iter()
            .map(|(id, log, presented)| {
                let latency = presented
                    .iter()
                    .filter_map(|(frame, shown)| Some(shown.at - log.stamped.get(frame)?))
                    .collect();
                let receive = presented
                    .iter()
                    .filter_map(|(frame, shown)| Some(shown.received? - log.stamped.get(frame)?))
                    .collect();
                let offsets: Vec<(f64, f64)> = reference
                    .map(|(_, _, reference)| {
                        presented
                            .iter()
                            .filter_map(|(frame, shown)| {
                                let base = reference.get(frame)?.at;
                                Some((base, shown.at - base))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                DisplaySummary {
                    id: (*id).clone(),
                    frames: presented.len(),
                    unreadable: log.unreadable + log.unplaced,
                    clock_rtt_ms: log.rtt,
                    pipeline_ms: Stats::of(log.delays.values().copied().collect()),
                    jitter_buffer_ms: log.jitter_buffer,
                    latency_ms: Stats::of(latency),
                    receive_ms: Stats::of(receive),
                    offset_ms: Stats::of(offsets.iter().map(|(_, offset)| *offset).collect()),
                    offset_change_ms: offset_change(&offsets),
                }
            })
            .collect();
        Summary {
            window_seconds: WINDOW_MS / 1000.0,
            reference: reference.map(|(id, _, _)| (*id).clone()),
            spread_ms: spread.and_then(Stats::of),
            displays: summaries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_MS: f64 = 1000.0 / 30.0;

    /// Stamp frames `frames` for both displays, and present them on `a` after
    /// 80 ms and on `b` after `80 + offset(t)` ms.
    fn measure(frames: std::ops::Range<u64>, offset: impl Fn(f64) -> f64) -> Measurement {
        let mut measurement = Measurement::default();
        for frame in frames {
            #[expect(clippy::cast_precision_loss, reason = "small frame numbers")]
            let stamped = frame as f64 * FRAME_MS;
            measurement.stamped("a", frame, stamped, Some(4.0));
            measurement.stamped("b", frame, stamped + 1.0, None);
            let code = frame % framecode::MODULUS;
            let shown_a = stamped + 80.0;
            let report = |at: f64| FrameReport {
                rtt: Some(4.0),
                jitter_buffer: Some(25.0),
                frames: vec![(code, at, Some(at - 30.0))],
                ..FrameReport::default()
            };
            measurement.shown("a", &report(shown_a), 30);
            measurement.shown("b", &report(shown_a + offset(shown_a)), 30);
        }
        measurement
    }

    fn close(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() < 1e-6
    }

    #[test]
    fn constant_offsets_have_no_drift() {
        let measurement = measure(0..300, |_| 12.0);
        let summary = measurement.summary(&["a".to_owned(), "b".to_owned(), "c".to_owned()]);
        assert_eq!(summary.reference.as_deref(), Some("a"));
        let [a, b, c] = summary.displays.as_slice() else {
            panic!("three displays")
        };
        assert_eq!((a.frames, b.frames, c.frames), (300, 300, 0));
        let latency = a.latency_ms.expect("latency");
        assert!(close(latency.median, 80.0) && close(latency.max, 80.0));
        assert!(close(a.receive_ms.expect("receive").median, 50.0));
        assert!(close(a.pipeline_ms.expect("pipeline").max, 4.0));
        assert!(b.pipeline_ms.is_none());
        assert_eq!(a.jitter_buffer_ms, Some(25.0));
        assert!(close(b.latency_ms.expect("latency").median, 91.0));
        let offset = b.offset_ms.expect("offset");
        assert!(close(offset.median, 12.0) && close(offset.p95, 12.0));
        assert!(close(b.offset_change_ms.expect("change"), 0.0));
        assert!(close(a.offset_ms.expect("offset").max, 0.0));
        assert!(c.latency_ms.is_none() && c.offset_ms.is_none());
        let spread = summary.spread_ms.expect("spread");
        assert_eq!(spread.samples, 300);
        assert!(close(spread.median, 12.0));
    }

    #[test]
    fn slow_drift_and_slipped_frames_show_as_offset_changes() {
        // 30 frames per second for 20 seconds, `b` drifting 3 ms per minute:
        // the last quarter's median is 15 seconds, 0.75 ms, past the first's.
        let measurement = measure(0..600, |at| 5.0 + 3.0 * at / 60_000.0);
        let summary = measurement.summary(&["a".to_owned(), "b".to_owned()]);
        let change = summary.displays[1].offset_change_ms.expect("change");
        assert!((change - 0.75).abs() < 0.01, "{change}");
        // `b` slips by one 60 Hz refresh halfway through.
        let measurement = measure(0..600, |at| if at < 10_000.0 { 0.0 } else { 16.7 });
        let summary = measurement.summary(&["a".to_owned(), "b".to_owned()]);
        let change = summary.displays[1].offset_change_ms.expect("change");
        assert!(close(change, 16.7), "{change}");
        // Too few frames to say.
        let measurement = measure(0..5, |_| 0.0);
        let summary = measurement.summary(&["a".to_owned(), "b".to_owned()]);
        assert!(summary.displays[1].offset_change_ms.is_none());
    }

    #[test]
    fn codes_are_placed_across_wraps_and_old_samples_expire() {
        let start = framecode::MODULUS - 30;
        let measurement = measure(start..start + 60, |_| 0.0);
        let summary = measurement.summary(&["a".to_owned()]);
        assert_eq!(summary.displays[0].frames, 60);
        assert_eq!(summary.displays[0].unreadable, 0);

        let mut measurement = measure(0..30 * 30, |_| 0.0);
        let summary = measurement.summary(&["a".to_owned()]);
        // Only the last 20 seconds of 30 are kept.
        assert_eq!(summary.displays[0].frames, 601);
        measurement.clear();
        let report = FrameReport {
            unreadable: 2,
            frames: vec![(1, 10.0, None)],
            ..FrameReport::default()
        };
        measurement.shown("a", &report, 30);
        let summary = measurement.summary(&["a".to_owned()]);
        assert_eq!(summary.displays[0].frames, 0);
        assert_eq!(summary.displays[0].unreadable, 3);
        assert!(summary.reference.is_none() && summary.spread_ms.is_none());
    }

    #[test]
    fn rtp_timestamps_name_frames_across_wraps() {
        // At 30 frames per second a frame is 3000 ticks; the pipeline's RTP
        // timestamps are its running time, sometimes a tick early.
        assert_eq!(rtp_frame(356_999, 119, 30), 119);
        assert_eq!(rtp_frame(357_000, 125, 30), 119);
        // The 32-bit timestamp wrapped after about 13 hours.
        let latest = 1_431_700;
        let rtp = u32::try_from((latest * 3000) % (1 << 32)).expect("32 bits");
        assert_eq!(rtp_frame(rtp, latest + 3, 30), latest);
        assert_eq!(rtp_frame(rtp.wrapping_sub(6000), latest, 30), latest - 2);
        // Reports by RTP timestamp place like codes, and only browsers
        // without them need codes drawn.
        let mut measurement = Measurement::default();
        for frame in 100..130 {
            measurement.stamped("a", frame, 0.0, None);
        }
        let report = FrameReport {
            rtp: (100..130)
                .map(|frame| (u32::try_from(frame * 3000).expect("small"), 50.0, None))
                .collect(),
            rtp_timestamps: Some(true),
            ..FrameReport::default()
        };
        measurement.shown("a", &report, 30);
        measurement.shown(
            "b",
            &FrameReport {
                rtp_timestamps: Some(false),
                ..FrameReport::default()
            },
            30,
        );
        let ids = ["a".to_owned(), "b".to_owned()];
        assert_eq!(measurement.summary(&ids).displays[0].frames, 30);
        assert!(measurement.needs_codes(&ids));
        assert!(!measurement.needs_codes(&ids[..1]));
        assert!(measurement.needs_codes(&["c".to_owned()]));
    }

    #[test]
    fn summaries_can_start_later() {
        let measurement = measure(0..300, |at| if at < 5000.0 { 40.0 } else { 10.0 });
        let ids = ["a".to_owned(), "b".to_owned()];
        let all = measurement.summary(&ids);
        assert_eq!(all.displays[1].frames, 300);
        let later = measurement.summary_since(&ids, 5000.0);
        let offset = later.displays[1].offset_ms.expect("offset");
        assert!(close(offset.max, 10.0), "{offset:?}");
        assert!(later.displays[1].frames < 300);
    }
}
