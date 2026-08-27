use std::error::Error as StdError;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PictureType {
    I,
    P,
    B,
}

impl PictureType {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::I => "I",
            Self::P => "P",
            Self::B => "B",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Config {
    pub(crate) threshold: f64,
    pub(crate) alpha: f64,
    pub(crate) start_frames: u32,
    pub(crate) clear_frames: u32,
    pub(crate) warmup_frames: u32,
    pub(crate) post_idr_guard_frames: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold: 3.0,
            alpha: 0.02,
            start_frames: 3,
            clear_frames: 4,
            warmup_frames: 30,
            post_idr_guard_frames: 0,
        }
    }
}

impl Config {
    pub(crate) fn validate(self) -> Result<Self, ConfigError> {
        if !self.threshold.is_finite() || self.threshold <= 0.0 {
            return Err(ConfigError::Threshold);
        }
        if !self.alpha.is_finite() || !(0.0..=1.0).contains(&self.alpha) || self.alpha == 0.0 {
            return Err(ConfigError::Alpha);
        }
        if self.start_frames == 0 {
            return Err(ConfigError::StartFrames);
        }
        if self.clear_frames == 0 {
            return Err(ConfigError::ClearFrames);
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigError {
    Threshold,
    Alpha,
    StartFrames,
    ClearFrames,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Threshold => {
                formatter.write_str("activity threshold must be finite and positive")
            }
            Self::Alpha => formatter.write_str("baseline alpha must be finite and in (0, 1]"),
            Self::StartFrames => formatter.write_str("activity minimum frames must be positive"),
            Self::ClearFrames => formatter.write_str("activity clear frames must be positive"),
        }
    }
}

impl StdError for ConfigError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Observation {
    pub(crate) frame_number: u64,
    pub(crate) encoded_vcl_bytes: u32,
    pub(crate) picture_type: PictureType,
    pub(crate) is_reference_picture: bool,
    pub(crate) is_keyframe: bool,
    pub(crate) is_idr: bool,
    pub(crate) timestamp_us: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityEventKind {
    Start,
    Stop,
}

impl ActivityEventKind {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Start => "activity-start",
            Self::Stop => "activity-stop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ActivityEvent {
    pub(crate) kind: ActivityEventKind,
    pub(crate) observation: Observation,
    pub(crate) score: f64,
    pub(crate) intensity: f64,
    pub(crate) baseline_size: f64,
}

#[derive(Debug, Clone, Copy)]
struct Baseline {
    mean: f64,
    variance: f64,
    observations: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct Analyzer {
    config: Config,
    p_baseline: Option<Baseline>,
    b_baseline: Option<Baseline>,
    active: bool,
    start_streak: u32,
    clear_streak: u32,
    post_idr_guard_remaining: u32,
}

impl Analyzer {
    pub(crate) fn new(config: Config) -> Result<Self, ConfigError> {
        Ok(Self {
            config: config.validate()?,
            p_baseline: None,
            b_baseline: None,
            active: false,
            start_streak: 0,
            clear_streak: 0,
            post_idr_guard_remaining: 0,
        })
    }

    pub(crate) fn reset(&mut self) {
        self.p_baseline = None;
        self.b_baseline = None;
        self.active = false;
        self.start_streak = 0;
        self.clear_streak = 0;
        self.post_idr_guard_remaining = 0;
    }

    pub(crate) fn observe(&mut self, observation: Observation) -> Option<ActivityEvent> {
        if observation.is_keyframe {
            if observation.is_idr && self.config.post_idr_guard_frames > 0 {
                self.post_idr_guard_remaining = self.config.post_idr_guard_frames;
                self.start_streak = 0;
                self.clear_streak = 0;
            }
            return None;
        }

        let guarded = self.post_idr_guard_remaining > 0;
        self.post_idr_guard_remaining = self.post_idr_guard_remaining.saturating_sub(1);

        let size = f64::from(observation.encoded_vcl_bytes);
        let baseline_slot = match observation.picture_type {
            PictureType::B => &mut self.b_baseline,
            PictureType::I | PictureType::P => &mut self.p_baseline,
        };
        let Some(mut baseline) = *baseline_slot else {
            *baseline_slot = Some(Baseline {
                mean: size,
                variance: 0.0,
                observations: 1,
            });
            self.start_streak = 0;
            self.clear_streak = 0;
            return None;
        };

        let delta = size - baseline.mean;
        let deviation_floor = (baseline.mean * 1.0e-6).max(1.0);
        let deviation = baseline.variance.sqrt().max(deviation_floor);
        let score = delta.abs() / deviation;
        let intensity = delta.abs() / baseline.mean.max(1.0);
        let eligible = baseline.observations >= self.config.warmup_frames;
        let anomalous = score >= self.config.threshold;
        let baseline_size = baseline.mean;

        baseline.mean += self.config.alpha * delta;
        baseline.variance =
            (1.0 - self.config.alpha) * (baseline.variance + self.config.alpha * delta * delta);
        baseline.observations = baseline.observations.saturating_add(1);
        *baseline_slot = Some(baseline);

        if guarded {
            self.start_streak = 0;
            self.clear_streak = 0;
            return None;
        }

        if !eligible {
            self.start_streak = 0;
            self.clear_streak = 0;
            return None;
        }

        let kind = if self.active {
            self.start_streak = 0;
            if anomalous {
                self.clear_streak = 0;
                None
            } else {
                self.clear_streak = self.clear_streak.saturating_add(1);
                if self.clear_streak >= self.config.clear_frames {
                    self.active = false;
                    self.clear_streak = 0;
                    Some(ActivityEventKind::Stop)
                } else {
                    None
                }
            }
        } else {
            self.clear_streak = 0;
            if anomalous {
                self.start_streak = self.start_streak.saturating_add(1);
                if self.start_streak >= self.config.start_frames {
                    self.active = true;
                    self.start_streak = 0;
                    Some(ActivityEventKind::Start)
                } else {
                    None
                }
            } else {
                self.start_streak = 0;
                None
            }
        };

        kind.map(|kind| ActivityEvent {
            kind,
            observation,
            score,
            intensity,
            baseline_size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(frame_number: u64, size: u32, is_keyframe: bool) -> Observation {
        Observation {
            frame_number,
            encoded_vcl_bytes: size,
            picture_type: if is_keyframe {
                PictureType::I
            } else {
                PictureType::P
            },
            is_reference_picture: true,
            is_keyframe,
            is_idr: is_keyframe,
            timestamp_us: Some(frame_number.saturating_mul(40_000)),
        }
    }

    #[test]
    fn starts_and_clears_after_configured_streaks() {
        let mut analyzer = Analyzer::new(Config {
            threshold: 1.5,
            alpha: 0.1,
            start_frames: 2,
            clear_frames: 2,
            warmup_frames: 2,
            post_idr_guard_frames: 0,
        })
        .expect("valid test config");

        assert!(analyzer.observe(observation(0, 100, false)).is_none());
        assert!(analyzer.observe(observation(1, 110, false)).is_none());
        assert!(analyzer.observe(observation(2, 102, false)).is_none());
        assert!(analyzer.observe(observation(3, 150, false)).is_none());
        let start = analyzer
            .observe(observation(4, 151, false))
            .expect("second anomaly starts activity");
        assert_eq!(start.kind, ActivityEventKind::Start);
        assert!(analyzer.observe(observation(5, 120, false)).is_none());
        let stop = analyzer
            .observe(observation(6, 121, false))
            .expect("second normal frame clears activity");
        assert_eq!(stop.kind, ActivityEventKind::Stop);
    }

    #[test]
    fn keyframes_do_not_change_streaks_or_baseline() {
        let config = Config {
            threshold: 1.0,
            alpha: 0.1,
            start_frames: 2,
            clear_frames: 2,
            warmup_frames: 1,
            post_idr_guard_frames: 0,
        };
        let mut with_keyframe = Analyzer::new(config).expect("valid test config");
        let mut without_keyframe = Analyzer::new(config).expect("valid test config");

        assert!(with_keyframe.observe(observation(0, 100, false)).is_none());
        assert!(
            without_keyframe
                .observe(observation(0, 100, false))
                .is_none()
        );
        assert!(
            with_keyframe
                .observe(observation(1, 10_000, true))
                .is_none()
        );
        assert_eq!(
            with_keyframe.observe(observation(2, 150, false)),
            without_keyframe.observe(observation(2, 150, false))
        );
    }

    #[test]
    fn post_idr_guard_learns_frames_without_advancing_transitions() {
        let mut analyzer = Analyzer::new(Config {
            threshold: 1.0,
            alpha: 0.1,
            start_frames: 2,
            clear_frames: 2,
            warmup_frames: 1,
            post_idr_guard_frames: 2,
        })
        .expect("valid guarded config");

        assert!(analyzer.observe(observation(0, 100, false)).is_none());
        assert!(analyzer.observe(observation(1, 200, false)).is_none());
        assert_eq!(analyzer.start_streak, 1);
        let observations_before_idr = analyzer
            .p_baseline
            .expect("initialized baseline")
            .observations;

        assert!(analyzer.observe(observation(2, 10_000, true)).is_none());
        assert_eq!(analyzer.post_idr_guard_remaining, 2);
        assert_eq!(analyzer.start_streak, 0);
        assert_eq!(
            analyzer
                .p_baseline
                .expect("preserved baseline")
                .observations,
            observations_before_idr
        );

        assert!(analyzer.observe(observation(3, 300, false)).is_none());
        assert_eq!(analyzer.post_idr_guard_remaining, 1);
        assert_eq!(analyzer.start_streak, 0);
        assert!(analyzer.observe(observation(4, 300, false)).is_none());
        assert_eq!(analyzer.post_idr_guard_remaining, 0);
        assert_eq!(analyzer.start_streak, 0);
        assert_eq!(
            analyzer
                .p_baseline
                .expect("guard frames update baseline")
                .observations,
            observations_before_idr + 2
        );
    }

    #[test]
    fn p_and_b_frames_learn_independent_baselines() {
        let mut analyzer = Analyzer::new(Config {
            threshold: 1.5,
            alpha: 0.1,
            start_frames: 2,
            clear_frames: 2,
            warmup_frames: 1,
            post_idr_guard_frames: 0,
        })
        .expect("valid test config");

        let mut b_frame = observation(1, 10, false);
        b_frame.picture_type = PictureType::B;
        b_frame.is_reference_picture = true;
        assert!(analyzer.observe(observation(0, 100, false)).is_none());
        assert!(analyzer.observe(b_frame).is_none());

        assert!(analyzer.observe(observation(2, 101, false)).is_none());
        b_frame.frame_number = 3;
        b_frame.encoded_vcl_bytes = 11;
        b_frame.is_reference_picture = false;
        assert!(analyzer.observe(b_frame).is_none());
        assert!(!analyzer.active);

        let p_baseline = analyzer.p_baseline.expect("P baseline");
        let b_baseline = analyzer.b_baseline.expect("B baseline");
        assert!((p_baseline.mean - 100.1).abs() < f64::EPSILON);
        assert!((b_baseline.mean - 10.1).abs() < f64::EPSILON);
        assert_eq!(p_baseline.observations, 2);
        assert_eq!(b_baseline.observations, 2);
    }

    #[test]
    fn ineligible_picture_class_breaks_start_streak() {
        let mut analyzer = Analyzer::new(Config {
            threshold: 1.0,
            alpha: 0.1,
            start_frames: 2,
            clear_frames: 2,
            warmup_frames: 2,
            post_idr_guard_frames: 0,
        })
        .expect("valid test config");

        assert!(analyzer.observe(observation(0, 100, false)).is_none());
        assert!(analyzer.observe(observation(1, 100, false)).is_none());
        assert!(analyzer.observe(observation(2, 200, false)).is_none());
        assert_eq!(analyzer.start_streak, 1);

        let mut first_b_frame = observation(3, 10, false);
        first_b_frame.picture_type = PictureType::B;
        assert!(analyzer.observe(first_b_frame).is_none());
        assert_eq!(analyzer.start_streak, 0);

        assert!(analyzer.observe(observation(4, 200, false)).is_none());
        assert_eq!(analyzer.start_streak, 1);
        assert_eq!(
            analyzer
                .observe(observation(5, 200, false))
                .expect("two consecutive eligible P anomalies start activity")
                .kind,
            ActivityEventKind::Start
        );
    }

    #[test]
    fn ineligible_picture_class_breaks_clear_streak() {
        let mut analyzer = Analyzer::new(Config {
            threshold: 1.0,
            alpha: 0.1,
            start_frames: 2,
            clear_frames: 2,
            warmup_frames: 2,
            post_idr_guard_frames: 0,
        })
        .expect("valid test config");

        assert!(analyzer.observe(observation(0, 100, false)).is_none());
        assert!(analyzer.observe(observation(1, 100, false)).is_none());
        assert!(analyzer.observe(observation(2, 200, false)).is_none());
        assert_eq!(
            analyzer
                .observe(observation(3, 200, false))
                .expect("two P anomalies start activity")
                .kind,
            ActivityEventKind::Start
        );
        assert!(analyzer.observe(observation(4, 119, false)).is_none());
        assert_eq!(analyzer.clear_streak, 1);

        let mut first_b_frame = observation(5, 10, false);
        first_b_frame.picture_type = PictureType::B;
        assert!(analyzer.observe(first_b_frame).is_none());
        assert_eq!(analyzer.clear_streak, 0);

        assert!(analyzer.observe(observation(6, 119, false)).is_none());
        assert_eq!(analyzer.clear_streak, 1);
        assert_eq!(
            analyzer
                .observe(observation(7, 119, false))
                .expect("two consecutive eligible P frames stop activity")
                .kind,
            ActivityEventKind::Stop
        );
    }

    #[test]
    fn non_idr_keyframe_does_not_arm_post_idr_guard() {
        let mut analyzer = Analyzer::new(Config {
            threshold: 1.0,
            alpha: 0.1,
            start_frames: 2,
            clear_frames: 2,
            warmup_frames: 1,
            post_idr_guard_frames: 2,
        })
        .expect("valid guarded config");

        assert!(analyzer.observe(observation(0, 100, false)).is_none());
        assert!(analyzer.observe(observation(1, 200, false)).is_none());
        assert_eq!(analyzer.start_streak, 1);

        let mut non_idr_keyframe = observation(2, 10_000, true);
        non_idr_keyframe.is_idr = false;
        assert!(analyzer.observe(non_idr_keyframe).is_none());
        assert_eq!(analyzer.post_idr_guard_remaining, 0);
        assert_eq!(analyzer.start_streak, 1);
    }

    #[test]
    fn post_idr_guard_does_not_clear_active_state() {
        let mut analyzer = Analyzer::new(Config {
            threshold: 1.0,
            alpha: 0.1,
            start_frames: 1,
            clear_frames: 1,
            warmup_frames: 1,
            post_idr_guard_frames: 2,
        })
        .expect("valid guarded config");

        assert!(analyzer.observe(observation(0, 100, false)).is_none());
        assert_eq!(
            analyzer
                .observe(observation(1, 200, false))
                .expect("anomaly starts activity")
                .kind,
            ActivityEventKind::Start
        );
        assert!(analyzer.active);

        assert!(analyzer.observe(observation(2, 10_000, true)).is_none());
        assert!(analyzer.observe(observation(3, 100, false)).is_none());
        assert!(analyzer.observe(observation(4, 100, false)).is_none());
        assert!(analyzer.active);

        assert_eq!(
            analyzer
                .observe(observation(5, 100, false))
                .expect("first unguarded normal frame clears activity")
                .kind,
            ActivityEventKind::Stop
        );
        assert!(!analyzer.active);
    }

    #[test]
    fn reset_returns_to_learning() {
        let mut analyzer = Analyzer::new(Config {
            warmup_frames: 1,
            ..Config::default()
        })
        .expect("valid test config");
        assert!(analyzer.observe(observation(0, 100, false)).is_none());
        analyzer.reset();
        assert!(analyzer.observe(observation(1, 10_000, false)).is_none());
    }

    #[test]
    fn rejects_invalid_config() {
        assert_eq!(
            Config {
                threshold: f64::NAN,
                ..Config::default()
            }
            .validate(),
            Err(ConfigError::Threshold)
        );
        assert_eq!(
            Config {
                alpha: 0.0,
                ..Config::default()
            }
            .validate(),
            Err(ConfigError::Alpha)
        );
        assert_eq!(
            Config {
                start_frames: 0,
                ..Config::default()
            }
            .validate(),
            Err(ConfigError::StartFrames)
        );
        assert_eq!(
            Config {
                clear_frames: 0,
                ..Config::default()
            }
            .validate(),
            Err(ConfigError::ClearFrames)
        );
    }
}
