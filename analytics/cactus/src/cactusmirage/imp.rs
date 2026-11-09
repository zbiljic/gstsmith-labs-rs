use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use std::sync::Arc;

use analytics_meta::schema;

use super::describer::{self, Describer};
use super::frame;
use super::latest::{Analyzed, Job, Worker};
use super::mapping;
use super::settings::{
    DEFAULT_MAX_FRAME_BYTES, Fields, JsonOutput, MAX_THREADS, Mode, Request, Sections, Settings,
};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "cactusmirage",
        gst::DebugColorFlags::empty(),
        Some("Cactus Mirage scene description"),
    )
});

/// The dry run at start describes a small grey image.
const DRY_RUN_SIZE: u32 = 32;
/// The largest `latency`, `hold-time` and `analysis-interval` value: an hour.
const MAX_TIME: u64 = 3_600_000_000_000;

/// The engine side in `mode=sync`: what one inference needs.
struct Analyzer {
    describer: Box<dyn Describer>,
    request: Request,
    /// The output buffer growth was already logged.
    grew_logged: bool,
}

/// The engine side in `mode=latest`: the worker and what it produced.
struct Latest {
    worker: Worker,
    /// Results from an older generation are discarded.
    generation: u64,
    grew_logged: bool,
}

/// Which frames are analyzed, and which result the others carry.
#[derive(Default)]
struct Pacing {
    /// The running time of the last frame given to the engine.
    last: Option<u64>,
    /// The last result, kept for `hold-time`, and when it was attached.
    held: Option<(Arc<Fresh>, Option<u64>)>,
    /// The worker was still busy when `analysis-interval` had passed.
    behind: bool,
}

impl Pacing {
    /// Whether the frame at running time `now` is due for analysis: always
    /// without an interval, a timestamp or an earlier analysis, and when
    /// running time went backwards.
    fn due(&self, now: Option<u64>, interval: u64) -> bool {
        match (self.last, now) {
            (Some(last), Some(now)) if interval > 0 => now < last || now - last >= interval,
            _ => true,
        }
    }

    /// Records the frame given to the engine; one without a timestamp does
    /// not move the reference.
    fn analyzed(&mut self, now: Option<u64>) {
        if now.is_some() {
            self.last = now;
        }
    }

    /// Keeps `fresh`, first attached at `now`, for `hold_time`.
    fn hold(&mut self, fresh: &Arc<Fresh>, now: Option<u64>, hold_time: u64) {
        self.held = (hold_time > 0).then(|| (Arc::clone(fresh), now));
    }

    /// The result for the frame at `now`: a fresh one, kept for
    /// `hold_time`, else the held one while it lasts.
    fn result(
        &mut self,
        fresh: Option<Arc<Fresh>>,
        now: Option<u64>,
        hold_time: u64,
    ) -> Option<Arc<Fresh>> {
        if let Some(fresh) = fresh {
            self.hold(&fresh, now, hold_time);
            return Some(fresh);
        }
        match &self.held {
            Some((held, Some(since)))
                if now.is_some_and(|now| now.saturating_sub(*since) <= hold_time) =>
            {
                Some(Arc::clone(held))
            }
            _ => {
                self.held = None;
                None
            }
        }
    }
}

/// A finished analysis ready to attach.
struct Fresh {
    json: String,
    analyzed: Analyzed,
}

/// The stream side: what the streaming thread needs, never held across an
/// inference.
#[derive(Clone)]
struct Stream {
    info: Option<gst_video::VideoInfo>,
    mode: Mode,
    hold_time: u64,
    analysis_interval: u64,
    json_output: JsonOutput,
    max_analysis_size: u32,
    max_frame_bytes: u32,
    /// SHA-256 of the loaded weights.
    weights: String,
}

impl Stream {
    /// Results reach only some frames, or frames other than their own.
    fn sparse(&self) -> bool {
        self.mode == Mode::Latest || self.analysis_interval > 0
    }
}

#[cfg(test)]
pub(super) type Fixture = std::sync::Arc<dyn Fn() -> Box<dyn Describer> + Send + Sync>;

#[derive(Default)]
pub struct CactusMirage {
    settings: Mutex<Settings>,
    stream: Mutex<Option<Stream>>,
    analyzer: Mutex<Option<Analyzer>>,
    latest: Mutex<Option<Latest>>,
    pacing: Mutex<Pacing>,
    #[cfg(test)]
    pub(super) fixture: Mutex<Option<Fixture>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl CactusMirage {
    #[cfg_attr(
        not(test),
        expect(
            clippy::unused_self,
            reason = "tests swap in a fixture held by the element"
        )
    )]
    fn describer(&self) -> Box<dyn Describer> {
        #[cfg(test)]
        if let Some(fixture) = lock(&self.fixture).as_ref() {
            return fixture();
        }
        Box::<describer::Engine>::default()
    }

    fn running_time(&self, buffer: &gst::BufferRef) -> Option<u64> {
        self.obj()
            .segment()
            .downcast_ref::<gst::ClockTime>()
            .and_then(|segment| segment.to_running_time(buffer.pts()))
            .map(gst::ClockTime::nseconds)
    }

    /// The analyzed copy of a frame, packed and possibly downscaled.
    fn analyzed_copy(
        buffer: &gst::BufferRef,
        stream: &Stream,
        info: &gst_video::VideoInfo,
    ) -> Result<(Vec<u8>, (u32, u32)), String> {
        let (width, height) = (info.width(), info.height());
        let packed = frame::packed_rgb(buffer, info, stream.max_frame_bytes)?;
        let size = frame::analysis_size(width, height, stream.max_analysis_size);
        Ok((frame::resize(packed, (width, height), size)?, size))
    }

    fn log_description(
        &self,
        description: &cactus::Description,
        elapsed: std::time::Duration,
        grew_logged: &mut bool,
    ) {
        gst::debug!(
            CAT,
            imp = self,
            "described in {elapsed:?}: {} tokens, {} bytes",
            description.tokens,
            description.json.len()
        );
        if let Some(size) = description.grew_to
            && !*grew_logged
        {
            *grew_logged = true;
            gst::info!(
                CAT,
                imp = self,
                "grew the engine output buffer to {size} bytes; that frame ran inference twice"
            );
        }
    }

    /// Writes a result onto `buffer`; with an `x-provenance` naming the
    /// analyzed frame when results reach only some frames.
    fn attach(
        &self,
        buffer: &mut gst::BufferRef,
        stream: &Stream,
        info: &gst_video::VideoInfo,
        json: &str,
        analyzed: Analyzed,
    ) -> Result<(), String> {
        let mut meta = analytics_meta::relation_meta_mut(buffer).map_err(|err| err.to_string())?;
        if stream.sparse() {
            // Results reach only some frames: a pool must not hand this
            // meta back, cleared, on a frame that was never analyzed.
            analytics_meta::unpool(&mut meta);
            schema::add_provenance(
                &mut meta,
                &schema::Provenance {
                    pts: analyzed.pts,
                    running_time: analyzed.running_time,
                    producer: "cactusmirage".to_owned(),
                },
            )
            .map_err(|err| err.to_string())?;
        }
        let written = mapping::write(
            &mut meta,
            json,
            &mapping::Frame {
                width: info.width(),
                height: info.height(),
                json_output: stream.json_output,
                weights: &stream.weights,
                pts: analyzed.pts,
            },
        )?;
        if let Some(bytes) = written.record_dropped {
            gst::warning!(
                CAT,
                imp = self,
                "dropped the cactus/mirage/describe record: {bytes} bytes exceed the {} byte cap",
                analytics_meta::json::MAX_JSON_BYTES
            );
        }
        for skipped in &written.skipped {
            gst::debug!(CAT, imp = self, "skipped {skipped}");
        }
        gst::log!(CAT, imp = self, "wrote {written:?}");
        Ok(())
    }

    fn process(&self, buffer: &mut gst::BufferRef) -> Result<(), String> {
        let stream = lock(&self.stream)
            .clone()
            .ok_or("the element is not started")?;
        let info = stream.info.clone().ok_or("caps are not negotiated")?;
        match stream.mode {
            Mode::Sync => self.process_sync(buffer, &stream, &info),
            Mode::Latest => self.process_latest(buffer, &stream, &info),
        }
    }

    fn process_sync(
        &self,
        buffer: &mut gst::BufferRef,
        stream: &Stream,
        info: &gst_video::VideoInfo,
    ) -> Result<(), String> {
        let now = self.running_time(buffer);
        let skipped = {
            let mut pacing = lock(&self.pacing);
            (!pacing.due(now, stream.analysis_interval))
                .then(|| pacing.result(None, now, stream.hold_time))
        };
        if let Some(held) = skipped {
            return match held {
                Some(held) => self.attach(buffer, stream, info, &held.json, held.analyzed),
                None => Ok(()),
            };
        }
        let (packed, size) = Self::analyzed_copy(buffer, stream, info)?;
        let description = {
            // Held for the inference, so stop() waits for one in progress.
            let mut analyzer = lock(&self.analyzer);
            let analyzer = analyzer.as_mut().ok_or("the element is not started")?;
            let started = Instant::now();
            let description =
                analyzer
                    .describer
                    .describe(&packed, size.0, size.1, &analyzer.request)?;
            self.log_description(&description, started.elapsed(), &mut analyzer.grew_logged);
            description
        };
        let analyzed = Analyzed {
            pts: buffer.pts().map(gst::ClockTime::nseconds),
            running_time: now,
            ..Analyzed::default()
        };
        let fresh = Arc::new(Fresh {
            json: description.json,
            analyzed,
        });
        {
            let mut pacing = lock(&self.pacing);
            pacing.analyzed(now);
            pacing.hold(&fresh, now, stream.hold_time);
        }
        self.attach(buffer, stream, info, &fresh.json, fresh.analyzed)
    }

    fn process_latest(
        &self,
        buffer: &mut gst::BufferRef,
        stream: &Stream,
        info: &gst_video::VideoInfo,
    ) -> Result<(), String> {
        let now = self.running_time(buffer);
        let mut latest = lock(&self.latest);
        let latest = latest.as_mut().ok_or("the element is not started")?;
        let mut fresh = None;
        for done in latest.worker.poll()? {
            if done.analyzed.generation != latest.generation {
                if let Err(err) = done.result {
                    gst::warning!(CAT, imp = self, "a discarded analysis failed: {err}");
                }
                continue;
            }
            let description = done.result?;
            self.log_description(&description, done.elapsed, &mut latest.grew_logged);
            fresh = Some(Arc::new(Fresh {
                json: description.json,
                analyzed: done.analyzed,
            }));
        }
        // A new result is attached once; hold-time re-attaches the last one.
        let mut pacing = lock(&self.pacing);
        if let Some(result) = pacing.result(fresh, now, stream.hold_time) {
            self.attach(buffer, stream, info, &result.json, result.analyzed)?;
        }
        let due = pacing.due(now, stream.analysis_interval);
        if latest.worker.busy() {
            if due && stream.analysis_interval > 0 && !pacing.behind {
                pacing.behind = true;
                gst::info!(
                    CAT,
                    imp = self,
                    "inference is slower than analysis-interval; analyzing back to back"
                );
            }
        } else if !due {
            if pacing.behind {
                pacing.behind = false;
                gst::info!(
                    CAT,
                    imp = self,
                    "inference keeps up with analysis-interval again"
                );
            }
        } else {
            let (rgb, size) = Self::analyzed_copy(buffer, stream, info)?;
            let job = Job {
                rgb,
                size,
                analyzed: Analyzed {
                    pts: buffer.pts().map(gst::ClockTime::nseconds),
                    running_time: now,
                    generation: latest.generation,
                },
            };
            if latest.worker.submit(job) {
                pacing.analyzed(now);
            } else {
                gst::debug!(CAT, imp = self, "the worker did not take the frame");
            }
        }
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for CactusMirage {
    const NAME: &'static str = "GstSmithCactusMirage";
    type Type = super::CactusMirage;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for CactusMirage {
    #[expect(clippy::too_many_lines, reason = "one entry per property")]
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let defaults = Settings::default();
            let string = |name: &'static str, blurb: &'static str| {
                glib::ParamSpecString::builder(name)
                    .nick(name)
                    .blurb(blurb)
                    .mutable_ready()
                    .build()
            };
            let count = |name: &'static str, blurb: &'static str| {
                glib::ParamSpecUInt::builder(name)
                    .nick(name)
                    .blurb(blurb)
                    .default_value(0)
                    .mutable_ready()
                    .build()
            };
            let threshold = |name: &'static str, blurb: &'static str| {
                glib::ParamSpecFloat::builder(name)
                    .nick(name)
                    .blurb(blurb)
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(0.0)
                    .mutable_ready()
                    .build()
            };
            vec![
                string("model-file", "Path of the Mirage .cact weights (required)"),
                glib::ParamSpecEnum::builder_with_default("mode", defaults.mode)
                    .nick("Mode")
                    .blurb("sync analyzes every frame on the streaming thread; latest passes frames at full rate and analyzes the newest in a worker")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("hold-time")
                    .nick("Hold time")
                    .blurb("Re-attach the last result to later frames for this many ns; 0 attaches each result once")
                    .maximum(MAX_TIME)
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("analysis-interval")
                    .nick("Analysis interval")
                    .blurb("Least running time in ns between analyzed frames; 0 analyzes every frame the mode allows")
                    .maximum(MAX_TIME)
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("latency")
                    .nick("Latency")
                    .blurb("Latency in ns added to LATENCY queries; inference time depends on options and hardware")
                    .maximum(MAX_TIME)
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecFlags::builder::<Sections>("sections")
                    .nick("Sections")
                    .blurb("Response sections to request")
                    .default_value(defaults.sections)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecFlags::builder::<Fields>("fields")
                    .nick("Fields")
                    .blurb("Per-object and per-line details to request")
                    .default_value(defaults.fields)
                    .mutable_ready()
                    .build(),
                threshold("score-threshold", "Object score threshold; 0 keeps the engine default"),
                threshold(
                    "text-score-threshold",
                    "Text line score threshold; 0 keeps the engine default",
                ),
                count("max-objects", "Most objects per frame; 0 keeps the engine default"),
                count("max-text-lines", "Most text lines per frame; 0 keeps the engine default"),
                count("max-tokens", "Most generated tokens; 0 keeps the engine default. Too few silently empties sections"),
                glib::ParamSpecBoolean::builder("nms")
                    .nick("NMS")
                    .blurb("Suppress overlapping detections")
                    .default_value(defaults.nms)
                    .mutable_ready()
                    .build(),
                string("language", "Language of names and descriptions: empty for English, or de, fr, es, it, nl, pl"),
                string("keywords", "Comma-separated words and phrases to favour"),
                string("form", "Form to fill, as a JSON object of field schemas"),
                string(
                    "extra-options",
                    "Other engine options as a JSON object, merged last; keys with their own property are refused",
                ),
                glib::ParamSpecEnum::builder_with_default("mask-format", defaults.mask_format)
                    .nick("Mask format")
                    .blurb("How object masks are outlined")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("max-analysis-size")
                    .nick("Max analysis size")
                    .blurb("Longest side of the analyzed copy in pixels; 0 analyzes the full frame")
                    .default_value(0)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("max-frame-bytes")
                    .nick("Max frame bytes")
                    .blurb("Largest frame (and packed copy) accepted, in bytes")
                    .minimum(1)
                    .default_value(DEFAULT_MAX_FRAME_BYTES)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("json-output", defaults.json_output)
                    .nick("JSON output")
                    .blurb("Which Cactus documents to attach; generic x-* documents are always written")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("engine")
                    .nick("Engine")
                    .blurb("Cactus engine to run on. Elements naming the same engine share it and take turns; each name is a separate engine, with its own models, speech stream and threads, that runs at the same time as the others. Unset is the default engine")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("threads")
                    .nick("Threads")
                    .blurb("Threads of this element's engine: the first explicit value before the engine loads weights wins; 0 keeps the engine default")
                    .maximum(MAX_THREADS)
                    .default_value(0)
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = lock(&self.settings);
        let defaults = Settings::default();
        let text = || value.get::<Option<String>>().ok().flatten();
        match pspec.name() {
            "model-file" => settings.model_file = text(),
            "mode" => settings.mode = value.get().unwrap_or(defaults.mode),
            "hold-time" => settings.hold_time = value.get().unwrap_or(0),
            "analysis-interval" => settings.analysis_interval = value.get().unwrap_or(0),
            "latency" => settings.latency = value.get().unwrap_or(0),
            "sections" => settings.sections = value.get().unwrap_or(settings.sections),
            "fields" => settings.fields = value.get().unwrap_or(settings.fields),
            "score-threshold" => settings.score_threshold = value.get().unwrap_or(0.0),
            "text-score-threshold" => settings.text_score_threshold = value.get().unwrap_or(0.0),
            "max-objects" => settings.max_objects = value.get().unwrap_or(0),
            "max-text-lines" => settings.max_text_lines = value.get().unwrap_or(0),
            "max-tokens" => settings.max_tokens = value.get().unwrap_or(0),
            "nms" => settings.nms = value.get().unwrap_or(defaults.nms),
            "language" => settings.language = text(),
            "keywords" => settings.keywords = text(),
            "form" => settings.form = text(),
            "extra-options" => settings.extra_options = text(),
            "mask-format" => settings.mask_format = value.get().unwrap_or(defaults.mask_format),
            "max-analysis-size" => settings.max_analysis_size = value.get().unwrap_or(0),
            "max-frame-bytes" => {
                settings.max_frame_bytes = value.get().unwrap_or(defaults.max_frame_bytes);
            }
            "json-output" => settings.json_output = value.get().unwrap_or(defaults.json_output),
            "engine" => settings.engine = text(),
            "threads" => settings.threads = value.get().unwrap_or(0),
            other => gst::error!(CAT, imp = self, "unknown property {other}"),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = lock(&self.settings);
        match pspec.name() {
            "model-file" => settings.model_file.to_value(),
            "mode" => settings.mode.to_value(),
            "hold-time" => settings.hold_time.to_value(),
            "analysis-interval" => settings.analysis_interval.to_value(),
            "latency" => settings.latency.to_value(),
            "sections" => settings.sections.to_value(),
            "fields" => settings.fields.to_value(),
            "score-threshold" => settings.score_threshold.to_value(),
            "text-score-threshold" => settings.text_score_threshold.to_value(),
            "max-objects" => settings.max_objects.to_value(),
            "max-text-lines" => settings.max_text_lines.to_value(),
            "max-tokens" => settings.max_tokens.to_value(),
            "nms" => settings.nms.to_value(),
            "language" => settings.language.to_value(),
            "keywords" => settings.keywords.to_value(),
            "form" => settings.form.to_value(),
            "extra-options" => settings.extra_options.to_value(),
            "mask-format" => settings.mask_format.to_value(),
            "max-analysis-size" => settings.max_analysis_size.to_value(),
            "max-frame-bytes" => settings.max_frame_bytes.to_value(),
            "json-output" => settings.json_output.to_value(),
            "engine" => settings.engine.to_value(),
            "threads" => settings.threads.to_value(),
            other => {
                gst::error!(CAT, imp = self, "unknown property {other}");
                glib::Value::from_type(pspec.value_type())
            }
        }
    }
}

impl GstObjectImpl for CactusMirage {}

impl ElementImpl for CactusMirage {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Cactus Mirage",
                "Filter/Analyzer/Video",
                "Describes frames with the Cactus Mirage vision model: objects, outlines, text, relations and captions as analytics metadata",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_video::VideoCapsBuilder::new()
                .format(gst_video::VideoFormat::Rgb)
                .pixel_aspect_ratio(gst::Fraction::new(1, 1))
                .field("interlace-mode", "progressive")
                .build();
            [
                ("src", gst::PadDirection::Src),
                ("sink", gst::PadDirection::Sink),
            ]
            .into_iter()
            .filter_map(|(name, direction)| {
                gst::PadTemplate::new(name, direction, gst::PadPresence::Always, &caps).ok()
            })
            .collect()
        });
        TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for CactusMirage {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let settings = lock(&self.settings).clone();
        let path = settings.model_file.clone().ok_or_else(|| {
            gst::error_msg!(gst::ResourceError::NotFound, ["model-file is required"])
        })?;
        let request = settings
            .request()
            .map_err(|err| gst::error_msg!(gst::LibraryError::Settings, ["{err}"]))?;
        let weights = std::fs::read(&path).map_err(|err| {
            gst::error_msg!(gst::ResourceError::OpenRead, ["reading {path}: {err}"])
        })?;
        let mut describer = self.describer();
        let (sha, threads) = describer
            .start(
                settings.engine.as_deref().unwrap_or_default(),
                &weights,
                settings.threads,
            )
            .map_err(|err| gst::error_msg!(gst::LibraryError::Init, ["{path}: {err}"]))?;
        drop(weights);
        match threads {
            cactus::Threads::Ignored { requested, current } => gst::warning!(
                CAT,
                imp = self,
                "threads={requested} is ignored: the engine already runs with {current}"
            ),
            cactus::Threads::TooLate { requested } => gst::warning!(
                CAT,
                imp = self,
                "threads={requested} is ignored: weights were loaded before it was set"
            ),
            cactus::Threads::EngineDefault | cactus::Threads::Applied(_) => {}
        }
        // A dry run turns bad options, a bad language or a malformed form
        // into a state-change error instead of silently empty frames.
        let grey = vec![128u8; usize::try_from(DRY_RUN_SIZE * DRY_RUN_SIZE * 3).unwrap_or(0)];
        let started = Instant::now();
        describer
            .describe(&grey, DRY_RUN_SIZE, DRY_RUN_SIZE, &request)
            .map_err(|err| gst::error_msg!(gst::LibraryError::Settings, ["{err}"]))?;
        gst::info!(
            CAT,
            imp = self,
            "loaded {path} (SHA-256 {sha}); options checked in {:?}",
            started.elapsed()
        );
        match settings.mode {
            Mode::Sync => {
                *lock(&self.analyzer) = Some(Analyzer {
                    describer,
                    request,
                    grew_logged: false,
                });
            }
            Mode::Latest => {
                let worker = Worker::spawn(describer, request).map_err(|err| {
                    gst::error_msg!(
                        gst::CoreError::Thread,
                        ["starting the analysis thread: {err}"]
                    )
                })?;
                *lock(&self.latest) = Some(Latest {
                    worker,
                    generation: 0,
                    grew_logged: false,
                });
            }
        }
        *lock(&self.pacing) = Pacing::default();
        *lock(&self.stream) = Some(Stream {
            info: None,
            mode: settings.mode,
            hold_time: settings.hold_time,
            analysis_interval: settings.analysis_interval,
            json_output: settings.json_output,
            max_analysis_size: settings.max_analysis_size,
            max_frame_bytes: settings.max_frame_bytes,
            weights: sha,
        });
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *lock(&self.stream) = None;
        *lock(&self.analyzer) = None;
        // Waits for an inference in progress, which cannot be cancelled.
        let latest = lock(&self.latest).take();
        if let Some(mut latest) = latest
            && let Err(err) = latest.worker.shutdown()
        {
            gst::warning!(CAT, imp = self, "{err}");
        }
        Ok(())
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if matches!(
            event.view(),
            gst::EventView::FlushStop(_)
                | gst::EventView::Segment(_)
                | gst::EventView::StreamStart(_)
        ) {
            // Whatever is in flight or held describes the old stream
            // position, and running time may jump.
            if let Some(latest) = lock(&self.latest).as_mut() {
                latest.generation = latest.generation.wrapping_add(1);
            }
            *lock(&self.pacing) = Pacing::default();
        }
        self.parent_sink_event(event)
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        let info = gst_video::VideoInfo::from_caps(incaps)
            .map_err(|err| gst::loggable_error!(CAT, "invalid caps {incaps}: {err}"))?;
        if info.format() != gst_video::VideoFormat::Rgb || info.par() != gst::Fraction::new(1, 1) {
            return Err(gst::loggable_error!(
                CAT,
                "only square-pixel RGB is supported"
            ));
        }
        {
            let mut stream = lock(&self.stream);
            let stream = stream
                .as_mut()
                .ok_or_else(|| gst::loggable_error!(CAT, "the element is not started"))?;
            frame::packed_size(info.width(), info.height(), stream.max_frame_bytes)
                .map_err(|err| gst::loggable_error!(CAT, "{err}"))?;
            let changed = stream.info.as_ref() != Some(&info);
            stream.info = Some(info);
            // A result for the old geometry would distort on the new one.
            if changed {
                if let Some(latest) = lock(&self.latest).as_mut() {
                    latest.generation = latest.generation.wrapping_add(1);
                }
                *lock(&self.pacing) = Pacing::default();
            }
        }
        self.parent_set_caps(incaps, outcaps)
    }

    fn query(&self, direction: gst::PadDirection, query: &mut gst::QueryRef) -> bool {
        if direction == gst::PadDirection::Src
            && let gst::QueryViewMut::Latency(latency) = query.view_mut()
        {
            // mode=latest adds no latency: frames pass at once.
            let ours = {
                let settings = lock(&self.settings);
                match settings.mode {
                    Mode::Sync => gst::ClockTime::from_nseconds(settings.latency.min(MAX_TIME)),
                    Mode::Latest => gst::ClockTime::ZERO,
                }
            };
            let Some(peer) = self.obj().sink_pad().peer() else {
                return false;
            };
            let mut upstream = gst::query::Latency::new();
            if !peer.query(&mut upstream) {
                return false;
            }
            let (live, min, max) = upstream.result();
            latency.set(
                live,
                min.saturating_add(ours),
                max.map(|max| max.saturating_add(ours)),
            );
            return true;
        }
        BaseTransformImplExt::parent_query(self, direction, query)
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.process(buffer).map_err(|err| {
            gst::element_imp_error!(self, gst::StreamError::Failed, ["Mirage: {err}"]);
            gst::FlowError::Error
        })?;
        Ok(gst::FlowSuccess::Ok)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::{Context as _, Result, ensure};
    use gst_analytics::prelude::*;
    use gst_analytics::{AnalyticsODMtd, AnalyticsRelationMeta};

    use super::*;

    const RESPONSE: &str = r#"{"objects":[{"id":1,"name":"cup","bbox":[250,250,750,750],"score":0.9,"mask":[250,250,750,250,750,750]}],"reasoning":"a cup"}"#;

    /// What the fake saw: (width, height, pixels, options) per call.
    type Seen = Arc<Mutex<Vec<(u32, u32, Vec<u8>, String)>>>;

    /// Frame calls (after the dry run) wait for one message each.
    type Gate = Arc<Mutex<std::sync::mpsc::Receiver<()>>>;

    #[derive(Clone, Default)]
    struct Behaviour {
        fail_after: Option<usize>,
        panic_after: Option<usize>,
        gate: Option<Gate>,
        delay: Option<std::time::Duration>,
    }

    struct Fake {
        seen: Seen,
        behaviour: Behaviour,
    }

    impl Describer for Fake {
        fn start(
            &mut self,
            _engine: &str,
            weights: &[u8],
            _threads: u32,
        ) -> Result<(String, cactus::Threads), String> {
            if weights.is_empty() {
                return Err("empty weights".to_owned());
            }
            Ok(("fake-sha".to_owned(), cactus::Threads::EngineDefault))
        }

        #[expect(
            clippy::panic_in_result_fn,
            reason = "the fake panics on purpose to kill the worker"
        )]
        fn describe(
            &mut self,
            rgb: &[u8],
            width: u32,
            height: u32,
            request: &Request,
        ) -> Result<cactus::Description, String> {
            let calls = {
                let mut seen = lock(&self.seen);
                seen.push((width, height, rgb.to_vec(), request.options.clone()));
                seen.len()
            };
            if calls > 1 {
                if let Some(gate) = &self.behaviour.gate {
                    // A timeout, so a failing test cannot hang in stop().
                    let _released = lock(gate).recv_timeout(std::time::Duration::from_secs(10));
                }
                if let Some(delay) = self.behaviour.delay {
                    std::thread::sleep(delay);
                }
            }
            assert!(
                self.behaviour
                    .panic_after
                    .is_none_or(|after| calls <= after),
                "the fake engine panics"
            );
            if self.behaviour.fail_after.is_some_and(|after| calls > after) {
                return Err("invalid options: sections has an unknown name nonsense".to_owned());
            }
            Ok(cactus::Description {
                json: RESPONSE.to_owned(),
                tokens: 10,
                grew_to: None,
            })
        }
    }

    fn weights() -> Result<tempfile_path::TempPath> {
        tempfile_path::TempPath::with_contents(b"weights")
    }

    /// A tiny temporary file helper, so tests need no extra crate.
    mod tempfile_path {
        pub struct TempPath(std::path::PathBuf);

        impl TempPath {
            pub fn with_contents(bytes: &[u8]) -> anyhow::Result<Self> {
                static NEXT: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                let path = std::env::temp_dir().join(format!(
                    "cactusmirage-{}-{}.cact",
                    std::process::id(),
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                std::fs::write(&path, bytes)?;
                Ok(Self(path))
            }

            pub fn as_str(&self) -> &str {
                self.0.to_str().unwrap_or_default()
            }
        }

        impl Drop for TempPath {
            fn drop(&mut self) {
                let _ignored = std::fs::remove_file(&self.0);
            }
        }
    }

    fn mirage(fail_after: Option<usize>) -> Result<(super::super::CactusMirage, Seen)> {
        mirage_with(Behaviour {
            fail_after,
            ..Behaviour::default()
        })
    }

    fn mirage_with(behaviour: Behaviour) -> Result<(super::super::CactusMirage, Seen)> {
        gst::init()?;
        let element: super::super::CactusMirage = glib::Object::new();
        let seen = Seen::default();
        let shared = Arc::clone(&seen);
        *lock(&element.imp().fixture) = Some(Arc::new(move || {
            Box::new(Fake {
                seen: Arc::clone(&shared),
                behaviour: behaviour.clone(),
            }) as Box<dyn Describer>
        }));
        Ok((element, seen))
    }

    /// A `mode=latest` element whose frame analyses wait for `release`.
    fn gated(
        properties: &[(&str, glib::Value)],
    ) -> Result<(
        super::super::CactusMirage,
        std::sync::mpsc::Sender<()>,
        tempfile_path::TempPath,
    )> {
        let (release, gate) = std::sync::mpsc::channel();
        let (element, _) = mirage_with(Behaviour {
            gate: Some(Arc::new(Mutex::new(gate))),
            ..Behaviour::default()
        })?;
        let weights = weights()?;
        element.set_property("model-file", weights.as_str());
        element.set_property_from_str("mode", "latest");
        for (name, value) in properties {
            element.set_property_from_value(name, value);
        }
        Ok((element, release, weights))
    }

    /// The analyzed PTS of a buffer's result, if it carries one.
    fn provenance(buffer: &gst::BufferRef) -> Option<u64> {
        let meta = buffer.meta::<AnalyticsRelationMeta>()?;
        analytics_meta::schema::provenance(&meta)?.pts
    }

    /// Pushes frames 40 ms apart from `pts_ms` until one carries a result.
    fn until_result(
        harness: &mut gst_check::Harness,
        mut pts_ms: u64,
    ) -> Result<(u64, gst::Buffer)> {
        for _ in 0..200 {
            let output = harness
                .push_and_pull(frame(pts_ms))
                .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
            if provenance(&output).is_some() {
                return Ok((pts_ms, output));
            }
            pts_ms += 40;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        anyhow::bail!("no result arrived")
    }

    /// A 3x2 RGB frame: 9-byte rows padded to the 12-byte `GStreamer` stride.
    fn frame(pts_ms: u64) -> gst::Buffer {
        let mut data = Vec::new();
        for row in 0..2u8 {
            data.extend((0..9).map(|i| row * 9 + i));
            data.extend([0xee; 3]);
        }
        let mut buffer = gst::Buffer::from_mut_slice(data);
        buffer
            .make_mut()
            .set_pts(gst::ClockTime::from_mseconds(pts_ms));
        buffer
    }

    fn harness(element: &super::super::CactusMirage) -> gst_check::Harness {
        let mut harness = gst_check::Harness::with_element(element, Some("sink"), Some("src"));
        harness.set_src_caps_str(
            "video/x-raw,format=RGB,width=3,height=2,pixel-aspect-ratio=1/1,interlace-mode=progressive,framerate=30/1",
        );
        harness
    }

    #[test]
    fn describes_frames_into_metadata() -> Result<()> {
        let weights = weights()?;
        let (element, seen) = mirage(None)?;
        element.set_property("model-file", weights.as_str());
        let mut harness = harness(&element);
        let output = harness
            .push_and_pull(frame(40))
            .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
        let calls = lock(&seen).clone();
        // The dry run, then the frame as a packed 3x2 copy.
        ensure!(calls.len() == 2, "{calls:?}");
        let (width, height, pixels, options) = calls.get(1).context("frame call")?.clone();
        ensure!((width, height) == (3, 2), "{calls:?}");
        ensure!(
            pixels == (0..18).collect::<Vec<u8>>(),
            "row padding leaked into the packed copy: {pixels:?}"
        );
        ensure!(options.contains(r#""fields":["bbox","score","mask","description"]"#));
        ensure!(
            options.contains(r#""sections":["scene","objects","text","relations","reasoning"]"#)
        );
        let meta = output
            .meta::<AnalyticsRelationMeta>()
            .context("relation meta")?;
        let cup = meta.iter::<AnalyticsODMtd>().next().context("cup")?;
        let location = cup.location()?;
        ensure!((location.x, location.y, location.w, location.h) == (1, 1, 1, 1));
        ensure!(analytics_meta::schema::polygons(&meta, cup.id()).len() == 1);
        let record = analytics_meta::json::documents_with_schema(&meta, mapping::DESCRIBE_SCHEMA)
            .next()
            .context("frame record")?;
        let record: serde_json::Value = serde_json::from_str(record.json)?;
        ensure!(record["pts"] == 40_000_000u64 && record["weights"] == "fake-sha");
        let bytes: usize = analytics_meta::json::documents(&meta)
            .map(|doc| doc.schema.len() + doc.json.len())
            .sum();
        eprintln!("documents per frame: {bytes} bytes");
        Ok(())
    }

    #[test]
    fn analyzes_a_downscaled_copy_but_boxes_fit_the_frame() -> Result<()> {
        let weights = weights()?;
        let (element, seen) = mirage(None)?;
        element.set_property("model-file", weights.as_str());
        element.set_property("max-analysis-size", 2u32);
        let mut harness = harness(&element);
        let output = harness
            .push_and_pull(frame(0))
            .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
        let calls = lock(&seen).clone();
        ensure!(
            calls.get(1).map(|c| (c.0, c.1)) == Some((2, 1)),
            "{calls:?}"
        );
        let meta = output.meta::<AnalyticsRelationMeta>().context("meta")?;
        let location = meta
            .iter::<AnalyticsODMtd>()
            .next()
            .context("cup")?
            .location()?;
        ensure!(
            (location.x, location.w) == (1, 1),
            "boxes use the frame size"
        );
        Ok(())
    }

    #[test]
    fn start_fails_clearly() -> Result<()> {
        let (element, _) = mirage(None)?;
        ensure!(
            element.set_state(gst::State::Paused).is_err(),
            "started without model-file"
        );
        element.set_state(gst::State::Null)?;

        let (element, _) = mirage(None)?;
        element.set_property("model-file", "/nonexistent/mirage.cact");
        ensure!(element.set_state(gst::State::Paused).is_err());
        element.set_state(gst::State::Null)?;

        let weights = weights()?;
        let (element, _) = mirage(Some(0))?;
        element.set_property("model-file", weights.as_str());
        ensure!(
            element.set_state(gst::State::Paused).is_err(),
            "the dry run fails"
        );
        element.set_state(gst::State::Null)?;

        let (element, _) = mirage(None)?;
        element.set_property("model-file", weights.as_str());
        element.set_property("extra-options", r#"{"conf":0.5}"#);
        ensure!(
            element.set_state(gst::State::Paused).is_err(),
            "owned key in extra-options"
        );
        element.set_state(gst::State::Null)?;
        Ok(())
    }

    #[test]
    fn restarts_with_the_same_fixture() -> Result<()> {
        let weights = weights()?;
        let (element, seen) = mirage(None)?;
        element.set_property("model-file", weights.as_str());
        for _ in 0..2 {
            element.set_state(gst::State::Paused)?;
            element.set_state(gst::State::Null)?;
        }
        ensure!(lock(&seen).len() == 2, "one dry run per start");
        Ok(())
    }

    #[test]
    fn latency_is_bounded() -> Result<()> {
        let (element, _) = mirage(None)?;
        let pspec = element
            .find_property("latency")
            .context("latency")?
            .downcast::<glib::ParamSpecUInt64>()
            .map_err(|_err| anyhow::anyhow!("not a u64 property"))?;
        ensure!(pspec.maximum() == MAX_TIME);
        Ok(())
    }

    #[test]
    fn a_failing_describe_is_a_stream_error() -> Result<()> {
        let weights = weights()?;
        let (element, _) = mirage(Some(1))?;
        element.set_property("model-file", weights.as_str());
        let mut harness = harness(&element);
        ensure!(harness.push(frame(0)) == Err(gst::FlowError::Error));
        Ok(())
    }

    #[test]
    fn latency_adds_the_configured_inference_time() -> Result<()> {
        let weights = weights()?;
        let (element, _) = mirage(None)?;
        element.set_property("model-file", weights.as_str());
        element.set_property("latency", 300_000_000u64);
        let mut harness = harness(&element);
        harness.set_upstream_latency(gst::ClockTime::from_mseconds(20));
        harness.play();
        let latency = harness.query_latency().context("latency")?;
        ensure!(latency == gst::ClockTime::from_mseconds(320), "{latency}");
        Ok(())
    }

    #[test]
    fn latest_passes_frames_and_attaches_each_result_once() -> Result<()> {
        let (element, release, _weights) = gated(&[])?;
        let mut harness = harness(&element);
        // The worker takes frame 0 and waits; frames keep flowing meanwhile.
        for pts in [0, 40, 80] {
            let output = harness
                .push_and_pull(frame(pts))
                .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
            ensure!(output.meta::<AnalyticsRelationMeta>().is_none());
        }
        release.send(())?;
        let (at, output) = until_result(&mut harness, 120)?;
        ensure!(provenance(&output) == Some(0), "describes frame 0");
        let meta = output.meta::<AnalyticsRelationMeta>().context("meta")?;
        ensure!(meta.iter::<AnalyticsODMtd>().count() == 1);
        let record = analytics_meta::json::documents_with_schema(&meta, mapping::DESCRIBE_SCHEMA)
            .next()
            .context("record")?;
        ensure!(serde_json::from_str::<serde_json::Value>(record.json)?["pts"] == 0);
        // Attached once: the next frame carries nothing (the worker waits again).
        let next = harness
            .push_and_pull(frame(at + 40))
            .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
        ensure!(next.meta::<AnalyticsRelationMeta>().is_none());
        drop(release);
        Ok(())
    }

    #[test]
    fn hold_time_reattaches_the_last_result() -> Result<()> {
        let (element, release, _weights) = gated(&[("hold-time", 100_000_000u64.to_value())])?;
        let mut harness = harness(&element);
        release.send(())?;
        let (at, _) = until_result(&mut harness, 0)?;
        let held = harness
            .push_and_pull(frame(at + 40))
            .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
        ensure!(provenance(&held) == Some(0), "held within 100 ms");
        let expired = harness
            .push_and_pull(frame(at + 200))
            .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
        ensure!(provenance(&expired).is_none(), "dropped after 100 ms");
        drop(release);
        Ok(())
    }

    #[test]
    fn a_flush_discards_the_analysis_in_flight() -> Result<()> {
        let (element, release, _weights) = gated(&[])?;
        let mut harness = harness(&element);
        harness
            .push_and_pull(frame(0))
            .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
        ensure!(harness.push_event(gst::event::FlushStart::new()));
        ensure!(harness.push_event(gst::event::FlushStop::new(true)));
        ensure!(
            harness.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                gst::ClockTime,
            >::new()))
        );
        release.send(())?;
        // The result for frame 0 arrives but belongs to the old generation;
        // the next analysis waits for another release that never comes.
        for pts in (40..800).step_by(40) {
            let output = harness
                .push_and_pull(frame(pts))
                .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
            ensure!(provenance(&output).is_none(), "a stale result was attached");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(release);
        Ok(())
    }

    #[test]
    fn stop_waits_for_at_most_one_analysis() -> Result<()> {
        let (element, _) = mirage_with(Behaviour {
            delay: Some(std::time::Duration::from_millis(300)),
            ..Behaviour::default()
        })?;
        let weights = weights()?;
        element.set_property("model-file", weights.as_str());
        element.set_property_from_str("mode", "latest");
        let mut harness = harness(&element);
        harness
            .push_and_pull(frame(0))
            .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
        let started = std::time::Instant::now();
        element.set_state(gst::State::Null)?;
        let waited = started.elapsed();
        ensure!(
            waited < std::time::Duration::from_secs(2),
            "stop took {waited:?}"
        );
        eprintln!("stop waited {waited:?} for the analysis in flight");
        Ok(())
    }

    #[test]
    fn a_failed_analysis_in_latest_mode_is_a_stream_error() -> Result<()> {
        let weights = weights()?;
        let (element, _) = mirage(Some(1))?;
        element.set_property("model-file", weights.as_str());
        element.set_property_from_str("mode", "latest");
        let mut harness = harness(&element);
        for pts in (0..8000).step_by(40) {
            if harness.push(frame(pts)) == Err(gst::FlowError::Error) {
                return Ok(());
            }
            let _ignored = harness.try_pull();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        anyhow::bail!("the failure never reached the stream")
    }

    #[test]
    fn latest_adds_no_latency() -> Result<()> {
        let (element, release, _weights) = gated(&[("latency", 300_000_000u64.to_value())])?;
        let mut harness = harness(&element);
        harness.set_upstream_latency(gst::ClockTime::from_mseconds(20));
        harness.play();
        let latency = harness.query_latency().context("latency")?;
        ensure!(latency == gst::ClockTime::from_mseconds(20), "{latency}");
        drop(release);
        Ok(())
    }

    #[test]
    fn latest_keeps_an_upstream_empty_relation_meta() -> Result<()> {
        // An upstream analyzer's "saw nothing" is not this element's to erase.
        let (element, release, _weights) = gated(&[])?;
        let mut harness = harness(&element);
        let mut analyzed = frame(0);
        gst_analytics::AnalyticsRelationMeta::add(analyzed.make_mut());
        let output = harness
            .push_and_pull(analyzed)
            .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
        ensure!(output.meta::<AnalyticsRelationMeta>().is_some());
        drop(release);
        Ok(())
    }

    #[test]
    fn pacing_spaces_analyses_start_to_start() {
        let mut pacing = Pacing::default();
        assert!(pacing.due(Some(0), 100), "the first frame");
        pacing.analyzed(Some(0));
        assert!(!pacing.due(Some(99), 100));
        assert!(pacing.due(Some(100), 100));
        assert!(pacing.due(Some(50), 0), "no interval");
        assert!(pacing.due(None, 100), "no timestamp");
        pacing.analyzed(None);
        assert!(
            !pacing.due(Some(99), 100),
            "an untimestamped frame keeps the reference"
        );
        pacing.analyzed(Some(500));
        assert!(pacing.due(Some(400), 100), "running time went backwards");
    }

    /// Pushes frames at `pts_ms` and returns each output's analyzed PTS.
    fn analyzed_pts(
        harness: &mut gst_check::Harness,
        pts_ms: impl IntoIterator<Item = u64>,
    ) -> Result<Vec<Option<u64>>> {
        pts_ms
            .into_iter()
            .map(|pts| {
                let output = harness
                    .push_and_pull(frame(pts))
                    .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
                Ok(provenance(&output).map(|pts| pts / 1_000_000))
            })
            .collect()
    }

    #[test]
    fn sync_analyzes_frames_an_interval_apart() -> Result<()> {
        let weights = weights()?;
        let (element, seen) = mirage(None)?;
        element.set_property("model-file", weights.as_str());
        element.set_property("analysis-interval", 100_000_000u64);
        let mut harness = harness(&element);
        let analyzed = analyzed_pts(&mut harness, (0..=240).step_by(40))?;
        ensure!(
            analyzed == [Some(0), None, None, Some(120), None, None, Some(240)],
            "{analyzed:?}"
        );
        ensure!(lock(&seen).len() == 4, "a dry run and three frames");
        Ok(())
    }

    #[test]
    fn sync_hold_time_covers_skipped_frames() -> Result<()> {
        let weights = weights()?;
        let (element, _) = mirage(None)?;
        element.set_property("model-file", weights.as_str());
        element.set_property("analysis-interval", 200_000_000u64);
        element.set_property("hold-time", 50_000_000u64);
        let mut harness = harness(&element);
        let analyzed = analyzed_pts(&mut harness, [0, 40, 80, 200])?;
        ensure!(
            analyzed == [Some(0), Some(0), None, Some(200)],
            "{analyzed:?}"
        );
        Ok(())
    }

    #[test]
    fn sync_without_an_interval_writes_no_provenance() -> Result<()> {
        let weights = weights()?;
        let (element, _) = mirage(None)?;
        element.set_property("model-file", weights.as_str());
        let mut harness = harness(&element);
        let output = harness
            .push_and_pull(frame(0))
            .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
        ensure!(output.meta::<AnalyticsRelationMeta>().is_some());
        ensure!(provenance(&output).is_none());
        Ok(())
    }

    #[test]
    fn a_new_segment_restarts_the_interval() -> Result<()> {
        let weights = weights()?;
        let (element, _) = mirage(None)?;
        element.set_property("model-file", weights.as_str());
        element.set_property("analysis-interval", 1_000_000_000u64);
        let mut harness = harness(&element);
        ensure!(analyzed_pts(&mut harness, [0, 40])? == [Some(0), None]);
        ensure!(
            harness.push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                gst::ClockTime,
            >::new()))
        );
        ensure!(analyzed_pts(&mut harness, [80])? == [Some(80)]);
        Ok(())
    }

    #[test]
    fn latest_spaces_analyses_by_the_interval() -> Result<()> {
        let weights = weights()?;
        let (element, seen) = mirage(None)?;
        element.set_property("model-file", weights.as_str());
        element.set_property_from_str("mode", "latest");
        element.set_property("analysis-interval", 200_000_000u64);
        let mut harness = harness(&element);
        let mut analyzed = Vec::new();
        for pts in (0..=1000).step_by(40) {
            analyzed.extend(analyzed_pts(&mut harness, [pts])?.into_iter().flatten());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        ensure!(analyzed.len() >= 3, "{analyzed:?}");
        ensure!(
            analyzed.windows(2).all(|pair| pair[1] >= pair[0] + 200),
            "{analyzed:?}"
        );
        ensure!(lock(&seen).len() <= 7, "a dry run and at most six frames");
        Ok(())
    }

    #[test]
    fn a_dead_worker_is_a_stream_error() -> Result<()> {
        let weights = weights()?;
        let (element, _) = mirage_with(Behaviour {
            panic_after: Some(1),
            ..Behaviour::default()
        })?;
        element.set_property("model-file", weights.as_str());
        element.set_property_from_str("mode", "latest");
        let mut harness = harness(&element);
        for pts in (0..8000).step_by(40) {
            if harness.push(frame(pts)) == Err(gst::FlowError::Error) {
                return Ok(());
            }
            let _ignored = harness.try_pull();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        anyhow::bail!("the dead worker was never reported")
    }
}
