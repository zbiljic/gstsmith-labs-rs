use std::sync::{LazyLock, Mutex};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use crate::analyzer::{Analyzer, Config, Observation};
use crate::codec::{self, AccessUnitScanner, Codec};

use super::event;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "nalsenseactivity",
        gst::DebugColorFlags::empty(),
        Some("Portable H.264/H.265 encoded-video activity analysis"),
    )
});

#[derive(Debug, Clone)]
struct Settings {
    stream_id: String,
    activity_threshold: f64,
    baseline_alpha: f64,
    activity_min_frames: u32,
    activity_clear_frames: u32,
    warmup_frames: u32,
    post_idr_guard_frames: u32,
}

impl Default for Settings {
    fn default() -> Self {
        let config = Config::default();
        Self {
            stream_id: "stream".into(),
            activity_threshold: config.threshold,
            baseline_alpha: config.alpha,
            activity_min_frames: config.start_frames,
            activity_clear_frames: config.clear_frames,
            warmup_frames: config.warmup_frames,
            post_idr_guard_frames: config.post_idr_guard_frames,
        }
    }
}

impl Settings {
    const fn config(&self) -> Config {
        Config {
            threshold: self.activity_threshold,
            alpha: self.baseline_alpha,
            start_frames: self.activity_min_frames,
            clear_frames: self.activity_clear_frames,
            warmup_frames: self.warmup_frames,
            post_idr_guard_frames: self.post_idr_guard_frames,
        }
    }
}

#[derive(Debug)]
struct Runtime {
    analyzer: Analyzer,
    access_unit_scanner: AccessUnitScanner,
    stream_id: String,
    next_frame_number: u64,
}

impl Runtime {
    fn new(
        settings: &Settings,
        codec: Codec,
        qp_diagnostics: bool,
    ) -> Result<Self, crate::analyzer::ConfigError> {
        Ok(Self {
            analyzer: Analyzer::new(settings.config())?,
            access_unit_scanner: AccessUnitScanner::new(codec, qp_diagnostics),
            stream_id: settings.stream_id.clone(),
            next_frame_number: 0,
        })
    }

    fn reset(&mut self) {
        self.analyzer.reset();
        self.next_frame_number = 0;
    }
}

#[derive(Default)]
pub struct NalSenseActivity {
    settings: Mutex<Settings>,
    runtime: Mutex<Option<Runtime>>,
}

#[glib::object_subclass]
impl ObjectSubclass for NalSenseActivity {
    const NAME: &'static str = "GstSmithNalSenseActivity";
    type Type = super::NalSenseActivity;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for NalSenseActivity {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let defaults = Settings::default();
            vec![
                glib::ParamSpecString::builder("stream-id")
                    .nick("Stream identifier")
                    .blurb("Identifier copied into NALSense event messages")
                    .default_value(Some("stream"))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecDouble::builder("activity-threshold")
                    .nick("Activity threshold")
                    .blurb("Minimum pre-update standard-deviation score")
                    .minimum(f64::EPSILON)
                    .maximum(100.0)
                    .default_value(defaults.activity_threshold)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecDouble::builder("baseline-alpha")
                    .nick("Baseline EWMA alpha")
                    .blurb("Encoded-frame baseline update weight")
                    .minimum(f64::EPSILON)
                    .maximum(1.0)
                    .default_value(defaults.baseline_alpha)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("activity-min-frames")
                    .nick("Activity start frames")
                    .blurb("Consecutive anomalous delta frames required to start activity")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(defaults.activity_min_frames)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("activity-clear-frames")
                    .nick("Activity clear frames")
                    .blurb("Consecutive normal delta frames required to stop activity")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(defaults.activity_clear_frames)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("warmup-frames")
                    .nick("Warmup frames")
                    .blurb("Delta frames learned before activity transitions are eligible")
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(defaults.warmup_frames)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("post-idr-guard-frames")
                    .nick("Post-IDR guard frames")
                    .blurb(
                        "Delta frames after each IDR that update the baseline but cannot change activity state",
                    )
                    .minimum(0)
                    .maximum(u32::MAX)
                    .default_value(defaults.post_idr_guard_frames)
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let Ok(mut settings) = self.settings.lock() else {
            gst::error!(CAT, imp = self, "NALSense settings lock is poisoned");
            return;
        };
        match pspec.name() {
            "stream-id" => {
                if let Ok(Some(stream_id)) = value.get::<Option<String>>() {
                    settings.stream_id = stream_id;
                }
            }
            "activity-threshold" => {
                if let Ok(threshold) = value.get() {
                    settings.activity_threshold = threshold;
                }
            }
            "baseline-alpha" => {
                if let Ok(alpha) = value.get() {
                    settings.baseline_alpha = alpha;
                }
            }
            "activity-min-frames" => {
                if let Ok(frames) = value.get() {
                    settings.activity_min_frames = frames;
                }
            }
            "activity-clear-frames" => {
                if let Ok(frames) = value.get() {
                    settings.activity_clear_frames = frames;
                }
            }
            "warmup-frames" => {
                if let Ok(frames) = value.get() {
                    settings.warmup_frames = frames;
                }
            }
            "post-idr-guard-frames" => {
                if let Ok(frames) = value.get() {
                    settings.post_idr_guard_frames = frames;
                }
            }
            _ => gst::warning!(CAT, imp = self, "unexpected property {}", pspec.name()),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let defaults = Settings::default();
        let settings = self.settings.lock().ok();
        match pspec.name() {
            "stream-id" => settings
                .as_ref()
                .map_or(defaults.stream_id.as_str(), |value| {
                    value.stream_id.as_str()
                })
                .to_value(),
            "activity-threshold" => settings
                .as_ref()
                .map_or(defaults.activity_threshold, |value| {
                    value.activity_threshold
                })
                .to_value(),
            "baseline-alpha" => settings
                .as_ref()
                .map_or(defaults.baseline_alpha, |value| value.baseline_alpha)
                .to_value(),
            "activity-min-frames" => settings
                .as_ref()
                .map_or(defaults.activity_min_frames, |value| {
                    value.activity_min_frames
                })
                .to_value(),
            "activity-clear-frames" => settings
                .as_ref()
                .map_or(defaults.activity_clear_frames, |value| {
                    value.activity_clear_frames
                })
                .to_value(),
            "warmup-frames" => settings
                .as_ref()
                .map_or(defaults.warmup_frames, |value| value.warmup_frames)
                .to_value(),
            "post-idr-guard-frames" => settings
                .as_ref()
                .map_or(defaults.post_idr_guard_frames, |value| {
                    value.post_idr_guard_frames
                })
                .to_value(),
            _ => pspec.default_value().clone(),
        }
    }
}

impl GstObjectImpl for NalSenseActivity {}

impl ElementImpl for NalSenseActivity {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "NALSense H.264/H.265 Activity Analyzer",
                "Filter/Analysis/Video",
                "Reports encoded-frame activity events while passing H.264/H.265 access units unchanged",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&METADATA)
    }

    #[expect(clippy::expect_used, reason = "fixed static pad templates are valid")]
    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = codec::parsed_au_caps();
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .expect("static NALSense sink pad"),
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .expect("static NALSense source pad"),
            ]
        });
        TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for NalSenseActivity {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = true;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.runtime.lock().map_err(|_poisoned| {
            gst::error_msg!(
                gst::LibraryError::Failed,
                ["NALSense state lock is poisoned"]
            )
        })? = None;
        Ok(())
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let structure = incaps
            .structure(0)
            .ok_or_else(|| gst::loggable_error!(CAT, "missing input caps structure"))?;
        let codec = Codec::from_media_type(structure.name()).ok_or_else(|| {
            gst::loggable_error!(CAT, "unsupported media type {}", structure.name())
        })?;
        let settings = self
            .settings
            .lock()
            .map_err(|_poisoned| gst::loggable_error!(CAT, "NALSense settings lock is poisoned"))?
            .clone();
        let runtime = Runtime::new(&settings, codec, CAT.threshold() >= gst::DebugLevel::Trace)
            .map_err(|error| gst::loggable_error!(CAT, "invalid NALSense config: {error}"))?;
        *self
            .runtime
            .lock()
            .map_err(|_poisoned| gst::loggable_error!(CAT, "NALSense state lock is poisoned"))? =
            Some(runtime);
        self.parent_set_caps(incaps, outcaps)
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if matches!(
            event.view(),
            gst::EventView::StreamStart(_)
                | gst::EventView::Segment(_)
                | gst::EventView::FlushStop(_)
        ) {
            match self.runtime.lock() {
                Ok(mut runtime) => {
                    if let Some(runtime) = runtime.as_mut() {
                        runtime.reset();
                        if matches!(event.view(), gst::EventView::StreamStart(_)) {
                            runtime.access_unit_scanner.reset();
                        }
                    }
                }
                Err(_poisoned) => {
                    gst::error!(CAT, imp = self, "NALSense state lock is poisoned");
                    return false;
                }
            }
        }
        self.parent_sink_event(event)
    }

    fn transform_ip_passthrough(
        &self,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let delta_unit = buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
        let discontinuity = buffer.flags().contains(gst::BufferFlags::DISCONT);
        let timestamp_us = buffer
            .pts()
            .or_else(|| buffer.dts())
            .map(|timestamp| timestamp.nseconds() / 1_000);
        let mapped = buffer.map_readable().map_err(|error| {
            gst::element_imp_error!(
                self,
                gst::ResourceError::Read,
                ["Failed to map a compressed-video access unit: {error}"]
            );
            gst::FlowError::Error
        })?;
        let transition = {
            let mut runtime = self.runtime.lock().map_err(|_poisoned| {
                gst::element_imp_error!(
                    self,
                    gst::LibraryError::Failed,
                    ["NALSense state lock is poisoned"]
                );
                gst::FlowError::Error
            })?;
            let runtime = runtime.as_mut().ok_or(gst::FlowError::NotNegotiated)?;
            if discontinuity {
                runtime.reset();
            }
            let codec = runtime.access_unit_scanner.codec();
            let access_unit = runtime
                .access_unit_scanner
                .scan(mapped.as_slice(), delta_unit)
                .map_err(|error| {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Format,
                        ["Invalid {} access unit: {error}", codec.name()]
                    );
                    gst::FlowError::Error
                })?;
            drop(mapped);
            let Some(access_unit) = access_unit else {
                return Ok(gst::FlowSuccess::Ok);
            };
            let frame_number = runtime.next_frame_number;
            runtime.next_frame_number = runtime
                .next_frame_number
                .checked_add(1)
                .ok_or(gst::FlowError::Error)?;
            let transition = runtime.analyzer.observe(Observation {
                frame_number,
                encoded_vcl_bytes: access_unit.encoded_vcl_bytes,
                picture_type: access_unit.picture_type,
                is_reference_picture: access_unit.is_reference_picture,
                is_keyframe: access_unit.is_keyframe,
                is_idr: access_unit.is_idr,
                timestamp_us,
            });
            if let Some(luma_qp) = access_unit.luma_qp {
                gst::trace!(
                    CAT,
                    imp = self,
                    "nalsense-picture codec={} frame={frame_number} type={} reference={} idr={} bytes={} qp={luma_qp}",
                    codec.name(),
                    access_unit.picture_type.name(),
                    access_unit.is_reference_picture,
                    access_unit.is_idr,
                    access_unit.encoded_vcl_bytes
                );
            } else {
                gst::trace!(
                    CAT,
                    imp = self,
                    "nalsense-picture codec={} frame={frame_number} type={} reference={} idr={} bytes={} qp=na",
                    codec.name(),
                    access_unit.picture_type.name(),
                    access_unit.is_reference_picture,
                    access_unit.is_idr,
                    access_unit.encoded_vcl_bytes
                );
            }
            transition.map(|transition| (runtime.stream_id.clone(), transition))
        };

        if let Some((stream_id, transition)) = transition {
            event::post(&self.obj(), &stream_id, transition);
        }
        Ok(gst::FlowSuccess::Ok)
    }
}
