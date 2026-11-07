//! Hysteresis and per-key event state, independent of `GStreamer` buffers.
//!
//! Labels and tracks are forgotten once gone. Classification keys are kept
//! for the stream's life; producers use a fixed set of fields per run.

use std::collections::{BTreeMap, BTreeSet};

use super::observe::Observation;

/// How long a new observation must persist before it becomes stable: at
/// least `min_frames` consecutive observations and `min_time` nanoseconds of
/// running time since the first of them. Without timestamps only the frame
/// count applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hysteresis {
    pub min_frames: u32,
    pub min_time: u64,
}

impl Default for Hysteresis {
    fn default() -> Self {
        Self {
            min_frames: 1,
            min_time: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct Pending<T> {
    value: T,
    frames: u32,
    since: Option<u64>,
}

/// A value that changes only after a different observation persists.
#[derive(Clone, Debug)]
pub struct Debounced<T> {
    stable: T,
    pending: Option<Pending<T>>,
}

impl<T: Clone + PartialEq> Debounced<T> {
    pub fn new(value: T) -> Self {
        Self {
            stable: value,
            pending: None,
        }
    }

    pub fn stable(&self) -> &T {
        &self.stable
    }

    pub fn is_settled(&self) -> bool {
        self.pending.is_none()
    }

    /// Restarts the time of a pending change, keeping its frame count, for
    /// when running time may have jumped.
    pub fn restart_time(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.since = None;
        }
    }

    /// Feeds one observation at running time `now`; returns the previous
    /// stable value when the stable value changes. A `repeat` is an analysis
    /// seen before: time advances, the frame count does not.
    pub fn observe(
        &mut self,
        value: T,
        now: Option<u64>,
        rule: Hysteresis,
        repeat: bool,
    ) -> Option<T> {
        if value == self.stable {
            self.pending = None;
            return None;
        }
        let mut pending = match self.pending.take() {
            Some(mut pending) if pending.value == value => {
                if !repeat {
                    pending.frames = pending.frames.saturating_add(1);
                }
                pending
            }
            _ => Pending {
                value,
                frames: 1,
                since: now,
            },
        };
        if pending.since.is_none() {
            pending.since = now;
        }
        let long_enough = match (pending.since, now) {
            (Some(since), Some(now)) => now.saturating_sub(since) >= rule.min_time,
            _ => true,
        };
        if pending.frames >= rule.min_frames.max(1) && long_enough {
            Some(std::mem::replace(&mut self.stable, pending.value))
        } else {
            self.pending = Some(pending);
            None
        }
    }
}

/// What happened, before it becomes a structure.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Present {
        label: String,
        count: u32,
        score: f32,
    },
    Absent {
        label: String,
    },
    CountChanged {
        label: String,
        count: u32,
        score: f32,
    },
    Entered {
        track: u64,
        label: Option<String>,
        score: Option<f32>,
    },
    Left {
        track: u64,
        label: Option<String>,
    },
    ClassChanged {
        label: String,
        score: f32,
    },
}

/// When and how one observation is fed.
#[derive(Clone, Copy, Debug)]
struct At {
    now: Option<u64>,
    rules: Rules,
    repeat: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rules {
    pub enter: Hysteresis,
    pub leave: Hysteresis,
}

impl Rules {
    fn for_presence(self, present: bool) -> Hysteresis {
        if present { self.enter } else { self.leave }
    }
}

#[derive(Debug)]
struct LabelState {
    presence: Debounced<bool>,
    count: Debounced<u32>,
}

#[derive(Debug)]
struct TrackState {
    presence: Debounced<bool>,
    label: Option<String>,
}

/// Event state of one stream.
#[derive(Debug, Default)]
pub struct Tracker {
    labels: BTreeMap<String, LabelState>,
    tracks: BTreeMap<u64, TrackState>,
    classes: BTreeMap<String, Debounced<Option<String>>>,
}

impl Tracker {
    /// Keeps what is present but restarts time-based hysteresis, for a new
    /// segment whose running time may jump.
    pub fn restart_time(&mut self) {
        for state in self.labels.values_mut() {
            state.presence.restart_time();
            state.count.restart_time();
        }
        for state in self.tracks.values_mut() {
            state.presence.restart_time();
        }
        for state in self.classes.values_mut() {
            state.restart_time();
        }
    }

    /// Feeds one analyzed frame and returns its changes in a stable order:
    /// labels, then tracks, then classes, each sorted by key. A `repeat` is
    /// an analysis already seen, attached again to a later frame.
    pub fn observe(
        &mut self,
        observation: &Observation,
        now: Option<u64>,
        rules: Rules,
        repeat: bool,
    ) -> Vec<Change> {
        let mut changes = Vec::new();
        let at = At { now, rules, repeat };
        self.observe_labels(observation, at, &mut changes);
        self.observe_tracks(observation, at, &mut changes);
        self.observe_classes(observation, at, &mut changes);
        changes
    }

    fn observe_labels(&mut self, observation: &Observation, at: At, changes: &mut Vec<Change>) {
        let keys: BTreeSet<String> = self
            .labels
            .keys()
            .chain(observation.labels.keys())
            .cloned()
            .collect();
        for label in keys {
            let seen = observation.labels.get(&label);
            let count = seen.map_or(0, |seen| seen.count);
            let score = seen.map_or(0.0, |seen| seen.score);
            let state = self
                .labels
                .entry(label.clone())
                .or_insert_with(|| LabelState {
                    presence: Debounced::new(false),
                    count: Debounced::new(0),
                });
            let present = count > 0;
            if state
                .presence
                .observe(present, at.now, at.rules.for_presence(present), at.repeat)
                .is_some()
            {
                if present {
                    state.count = Debounced::new(count);
                    changes.push(Change::Present {
                        label: label.clone(),
                        count,
                        score,
                    });
                } else {
                    changes.push(Change::Absent {
                        label: label.clone(),
                    });
                }
            } else if *state.presence.stable()
                && present
                && state
                    .count
                    .observe(count, at.now, at.rules.enter, at.repeat)
                    .is_some()
            {
                changes.push(Change::CountChanged {
                    label: label.clone(),
                    count,
                    score,
                });
            }
            if !present && !*state.presence.stable() && state.presence.is_settled() {
                self.labels.remove(&label);
            }
        }
    }

    fn observe_tracks(&mut self, observation: &Observation, at: At, changes: &mut Vec<Change>) {
        let keys: BTreeSet<u64> = self
            .tracks
            .keys()
            .chain(observation.tracks.keys())
            .copied()
            .collect();
        for track in keys {
            let seen = observation.tracks.get(&track);
            let state = self.tracks.entry(track).or_insert_with(|| TrackState {
                presence: Debounced::new(false),
                label: None,
            });
            if let Some(label) = seen.and_then(|seen| seen.label.clone()) {
                state.label = Some(label);
            }
            let present = seen.is_some();
            if state
                .presence
                .observe(present, at.now, at.rules.for_presence(present), at.repeat)
                .is_some()
            {
                changes.push(if present {
                    Change::Entered {
                        track,
                        label: state.label.clone(),
                        score: seen.and_then(|seen| seen.score),
                    }
                } else {
                    Change::Left {
                        track,
                        label: state.label.clone(),
                    }
                });
            }
            if !present && !*state.presence.stable() && state.presence.is_settled() {
                self.tracks.remove(&track);
            }
        }
    }

    fn observe_classes(&mut self, observation: &Observation, at: At, changes: &mut Vec<Change>) {
        for (key, class) in &observation.classes {
            let state = self
                .classes
                .entry(key.clone())
                .or_insert_with(|| Debounced::new(None));
            if state
                .observe(Some(class.label.clone()), at.now, at.rules.enter, at.repeat)
                .is_some()
            {
                changes.push(Change::ClassChanged {
                    label: class.label.clone(),
                    score: class.score,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::observe::{ClassSeen, LabelSeen, TrackSeen};
    use super::*;

    const MS: u64 = 1_000_000;

    fn frame(labels: &[(&str, u32)]) -> Observation {
        Observation {
            labels: labels
                .iter()
                .map(|&(label, count)| (label.to_owned(), LabelSeen { count, score: 0.5 }))
                .collect(),
            ..Observation::default()
        }
    }

    #[test]
    fn debounced_values_need_frames_and_time() {
        let rule = Hysteresis {
            min_frames: 2,
            min_time: 100 * MS,
        };
        let mut value = Debounced::new(false);
        assert_eq!(value.observe(true, Some(0), rule, false), None);
        // Two frames, but only 40 ms.
        assert_eq!(value.observe(true, Some(40 * MS), rule, false), None);
        // A relapse restarts the streak.
        assert_eq!(value.observe(false, Some(80 * MS), rule, false), None);
        assert_eq!(value.observe(true, Some(120 * MS), rule, false), None);
        assert_eq!(value.observe(true, Some(200 * MS), rule, false), None);
        assert_eq!(
            value.observe(true, Some(220 * MS), rule, false),
            Some(false)
        );
        assert!(*value.stable());
        // Without timestamps only frames count.
        let mut value = Debounced::new(0u32);
        assert_eq!(value.observe(3, None, rule, false), None);
        assert_eq!(value.observe(3, None, rule, false), Some(0));
    }

    #[test]
    fn labels_report_presence_and_counts() {
        let mut tracker = Tracker::default();
        let rules = Rules::default();
        assert_eq!(
            tracker.observe(&frame(&[("man", 1)]), None, rules, false),
            [Change::Present {
                label: "man".to_owned(),
                count: 1,
                score: 0.5
            }]
        );
        assert_eq!(
            tracker.observe(&frame(&[("man", 1)]), None, rules, false),
            []
        );
        assert_eq!(
            tracker.observe(&frame(&[("man", 2), ("cup", 1)]), None, rules, false),
            [
                Change::Present {
                    label: "cup".to_owned(),
                    count: 1,
                    score: 0.5
                },
                Change::CountChanged {
                    label: "man".to_owned(),
                    count: 2,
                    score: 0.5
                },
            ]
        );
        assert_eq!(
            tracker.observe(&frame(&[]), None, rules, false),
            [
                Change::Absent {
                    label: "cup".to_owned()
                },
                Change::Absent {
                    label: "man".to_owned()
                },
            ]
        );
        assert!(tracker.labels.is_empty(), "absent labels are forgotten");
    }

    #[test]
    fn leave_hysteresis_rides_out_flicker() {
        let mut tracker = Tracker::default();
        let rules = Rules {
            enter: Hysteresis::default(),
            leave: Hysteresis {
                min_frames: 3,
                min_time: 0,
            },
        };
        assert_eq!(
            tracker
                .observe(&frame(&[("man", 1)]), None, rules, false)
                .len(),
            1
        );
        for _ in 0..2 {
            assert_eq!(tracker.observe(&frame(&[]), None, rules, false), []);
        }
        assert_eq!(
            tracker.observe(&frame(&[("man", 1)]), None, rules, false),
            []
        );
        for _ in 0..2 {
            assert_eq!(tracker.observe(&frame(&[]), None, rules, false), []);
        }
        assert_eq!(
            tracker.observe(&frame(&[]), None, rules, false),
            [Change::Absent {
                label: "man".to_owned()
            }]
        );
    }

    #[test]
    fn tracks_enter_and_leave_with_their_label() {
        let mut tracker = Tracker::default();
        let rules = Rules::default();
        let mut seen = Observation::default();
        seen.tracks.insert(
            7,
            TrackSeen {
                label: Some("man".to_owned()),
                score: Some(0.75),
            },
        );
        assert_eq!(
            tracker.observe(&seen, None, rules, false),
            [Change::Entered {
                track: 7,
                label: Some("man".to_owned()),
                score: Some(0.75)
            }]
        );
        assert_eq!(
            tracker.observe(&Observation::default(), None, rules, false),
            [Change::Left {
                track: 7,
                label: Some("man".to_owned())
            }]
        );
        assert!(tracker.tracks.is_empty());
    }

    #[test]
    fn classes_change_per_key() {
        let mut tracker = Tracker::default();
        let rules = Rules::default();
        let mut seen = Observation::default();
        let class = |label: &str| ClassSeen {
            label: label.to_owned(),
            score: 0.6,
        };
        seen.classes.insert("a=no|a=yes".to_owned(), class("a=yes"));
        assert_eq!(
            tracker.observe(&seen, None, rules, false),
            [Change::ClassChanged {
                label: "a=yes".to_owned(),
                score: 0.6
            }]
        );
        assert_eq!(tracker.observe(&seen, None, rules, false), []);
        // A frame without the field keeps its value.
        assert_eq!(
            tracker.observe(&Observation::default(), None, rules, false),
            []
        );
        seen.classes.insert("a=no|a=yes".to_owned(), class("a=no"));
        assert_eq!(
            tracker.observe(&seen, None, rules, false),
            [Change::ClassChanged {
                label: "a=no".to_owned(),
                score: 0.6
            }]
        );
    }

    #[test]
    fn repeated_analyses_count_time_but_not_frames() {
        let rule = Hysteresis {
            min_frames: 2,
            min_time: 0,
        };
        let mut value = Debounced::new(false);
        assert_eq!(value.observe(true, Some(0), rule, false), None);
        assert_eq!(value.observe(true, Some(40 * MS), rule, true), None);
        assert_eq!(value.observe(true, Some(80 * MS), rule, true), None);
        assert_eq!(
            value.observe(true, Some(120 * MS), rule, false),
            Some(false)
        );
    }

    #[test]
    fn restarting_time_keeps_presence_and_frames() {
        let rules = Rules {
            enter: Hysteresis::default(),
            leave: Hysteresis {
                min_frames: 1,
                min_time: 100 * MS,
            },
        };
        let mut tracker = Tracker::default();
        assert_eq!(
            tracker
                .observe(&frame(&[("man", 1)]), Some(0), rules, false)
                .len(),
            1
        );
        assert_eq!(
            tracker.observe(&frame(&[]), Some(40 * MS), rules, false),
            []
        );
        // A new segment: running time restarts near zero.
        tracker.restart_time();
        assert_eq!(
            tracker.observe(&frame(&[]), Some(10 * MS), rules, false),
            []
        );
        assert_eq!(
            tracker.observe(&frame(&[]), Some(110 * MS), rules, false),
            [Change::Absent {
                label: "man".to_owned()
            }]
        );
        // And presence survived: nothing re-announces the man.
        assert_eq!(
            tracker
                .observe(&frame(&[("man", 1)]), Some(120 * MS), rules, false)
                .len(),
            1
        );
    }
}
