use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use analytics_meta::schema;
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use super::ring::Ring;
use crate::embedder::{Embedder, SegmentEmbedder};
use crate::profile;
use crate::speakers::{self, Fields, Names, Outcome, Registry, Rules};

#[cfg(test)]
mod tests;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "diarizer",
        gst::DebugColorFlags::empty(),
        Some("Speakers for text cues"),
    )
});

/// The longest a cue waits for its audio to arrive. After a cue waited this
/// long, later cues do not wait until audio arrives again.
#[cfg(not(test))]
const AUDIO_WAIT: Duration = Duration::from_secs(5);
#[cfg(test)]
const AUDIO_WAIT: Duration = Duration::from_millis(1500);
/// Added to the upstream text latency for waiting on audio and embedding a
/// cue: about 16 ms per second of audio with 4 threads, more with fewer.
const PROCESSING_MS: u64 = 1000;
/// Audio within this many milliseconds of the expected position continues
/// the buffer.
const JITTER_MS: u64 = 20;
/// Cue audio quieter than this RMS (−70 dBFS) is silence: a model embeds
/// silence like a voice, so it is not given one.
const SILENCE_RMS: f32 = 3.2e-4;
/// The upstream speaker-change event, as gst-plugins-rs' `speechmatics`
/// transcriber sends it.
const SPEAKER_CHANGE: &str = "rstranscribe/speaker-change";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstDiarizerModelProfile")]
pub enum ModelProfile {
    #[default]
    #[enum_value(
        name = "Recognize a qualified model file by its SHA-256",
        nick = "auto"
    )]
    Auto,
    #[enum_value(
        name = "WeSpeaker ResNet34-LM (VoxCeleb), 256 values",
        nick = "wespeaker-resnet34"
    )]
    WespeakerResnet34,
    #[enum_value(name = "Read from model-config", nick = "custom")]
    Custom,
}

impl ModelProfile {
    /// The built-in profile's name.
    fn name(self) -> Option<&'static str> {
        match self {
            Self::Auto | Self::Custom => None,
            Self::WespeakerResnet34 => Some("wespeaker-resnet34"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstDiarizerLabelMode")]
pub enum LabelMode {
    #[enum_value(name = "Prefix every cue with a known speaker", nick = "always")]
    Always,
    #[default]
    #[enum_value(
        name = "Prefix a cue whose speaker differs from the previous known one",
        nick = "on-change"
    )]
    OnChange,
}

#[derive(Clone, Debug, PartialEq)]
struct Settings {
    model_file: Option<String>,
    model_profile: ModelProfile,
    model_config: Option<String>,
    threads: u32,
    match_threshold: f64,
    short_match_threshold: f64,
    min_new_duration: u32,
    min_update_duration: u32,
    max_speakers: u32,
    names: Names,
    label_format: String,
    label_mode: LabelMode,
    unknown_label: String,
    audio_buffer_duration: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            model_file: None,
            model_profile: ModelProfile::Auto,
            model_config: None,
            threads: 0,
            match_threshold: 0.0,
            short_match_threshold: 0.0,
            min_new_duration: 1500,
            min_update_duration: 1000,
            max_speakers: 0,
            names: Names::new(),
            label_format: "{label}: ".to_owned(),
            label_mode: LabelMode::OnChange,
            unknown_label: String::new(),
            audio_buffer_duration: 60_000,
        }
    }
}

impl Settings {
    fn names_structure(&self) -> gst::Structure {
        let mut keys: Vec<_> = self.names.iter().collect();
        keys.sort();
        let mut structure = gst::Structure::new_empty("names");
        for (key, name) in keys {
            structure.set(key.as_str(), name);
        }
        structure
    }

    fn set_names(&mut self, structure: Option<&gst::Structure>) {
        self.names = structure
            .map(|structure| {
                structure
                    .iter()
                    .filter_map(|(key, value)| Some((key.to_string(), value.get::<String>().ok()?)))
                    .filter(|(key, name)| !key.is_empty() && !name.is_empty())
                    .collect()
            })
            .unwrap_or_default();
    }
}

/// Audio by running time, shared by both sink pads.
#[derive(Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent conditions of the two pads"
)]
struct Audio {
    ring: Ring,
    /// The model's sample rate, set when the element starts.
    rate: u32,
    max_samples: usize,
    segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    caps: bool,
    eos: bool,
    /// The text pad is flushing or the element is stopping, so a cue waiting
    /// for audio gives up.
    flushing: bool,
    /// No later cue needs audio before this index.
    needed_from: u64,
    /// A cue waited for audio in vain, and no audio arrived since.
    starved: bool,
}

impl Audio {
    fn index(&self, running_time: gst::ClockTime) -> u64 {
        let index = u128::from(running_time.nseconds()) * u128::from(self.rate) / 1_000_000_000;
        u64::try_from(index).unwrap_or(u64::MAX)
    }

    fn reset(&mut self) {
        self.ring = Ring::default();
        self.eos = false;
        self.needed_from = 0;
        self.starved = false;
    }
}

#[derive(Default)]
struct State {
    embedder: Option<Box<dyn SegmentEmbedder>>,
    model: String,
    registry: Option<Registry>,
    segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    caps: bool,
    /// The speaker of the last cue with a known speaker.
    previous: Option<u32>,
    /// Whether the last prefix written was `unknown-label`, so the next
    /// known speaker is labelled again in on-change mode.
    unknown_shown: bool,
}

/// What `identify` found for one cue.
struct Identified {
    document: schema::Speaker,
    /// The label to announce in a speaker-change event.
    change: Option<String>,
    prefix: String,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn text_caps() -> gst::Caps {
    gst::Caps::builder("text/x-raw")
        .field("format", "utf8")
        .build()
}

fn audio_caps(rate: Option<u32>) -> gst::Caps {
    let builder = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("layout", "interleaved");
    let builder = match rate.and_then(|rate| i32::try_from(rate).ok()) {
        Some(rate) => builder.field("rate", rate),
        None => builder.field("rate", gst::IntRange::new(1, i32::MAX)),
    };
    builder.field("channels", 1_i32).build()
}

fn rms(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f64 = pcm.iter().map(|v| f64::from(*v) * f64::from(*v)).sum();
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        reason = "a level"
    )]
    let rms = (sum / pcm.len() as f64).sqrt() as f32;
    rms
}

#[cfg(test)]
pub(super) type Fixture = std::sync::Arc<dyn Fn() -> Box<dyn SegmentEmbedder> + Send + Sync>;

pub struct Diarizer {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    audio: Mutex<Audio>,
    /// Signalled when audio arrives or waiting cues must give up.
    audio_changed: Condvar,
    /// A cue found its audio already dropped, which is reported once.
    warned_dropped: AtomicBool,
    audio_sinkpad: gst::Pad,
    text_sinkpad: gst::Pad,
    srcpad: gst::Pad,
    #[cfg(test)]
    pub(super) fixture: Mutex<Option<Fixture>>,
}

#[glib::object_subclass]
impl ObjectSubclass for Diarizer {
    const NAME: &'static str = "GstDiarizer";
    type Type = super::Diarizer;
    type ParentType = gst::Element;

    #[expect(
        clippy::expect_used,
        reason = "class pad templates are installed before construction"
    )]
    fn with_class(class: &Self::Class) -> Self {
        let template = |name| class.pad_template(name).expect("pad template");
        let audio_sinkpad = gst::Pad::builder_from_template(&template("audio_sink"))
            .chain_function(|_, parent, buffer| {
                Self::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.audio_chain(&buffer),
                )
            })
            .event_function(|_, parent, event| {
                Self::catch_panic_pad_function(parent, || false, |imp| imp.audio_event(&event))
            })
            .query_function(|pad, parent, query| {
                Self::catch_panic_pad_function(parent, || false, |imp| imp.audio_query(pad, query))
            })
            .iterate_internal_links_function(|_, _| gst::Iterator::from_vec(Vec::new()))
            .build();
        let text_sinkpad = gst::Pad::builder_from_template(&template("text_sink"))
            .chain_function(|_, parent, buffer| {
                Self::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |imp| imp.text_chain(buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Self::catch_panic_pad_function(parent, || false, |imp| imp.text_event(pad, event))
            })
            .iterate_internal_links_function(|_, parent| {
                Self::catch_panic_pad_function(
                    parent,
                    || gst::Iterator::from_vec(Vec::new()),
                    |imp| gst::Iterator::from_vec(vec![imp.srcpad.clone()]),
                )
            })
            .build();
        // Upstream events and queries follow the text, whose branch
        // normally starts at the same source as the audio.
        let srcpad = gst::Pad::builder_from_template(&template("src"))
            .query_function(|pad, parent, query| {
                Self::catch_panic_pad_function(parent, || false, |imp| imp.src_query(pad, query))
            })
            .iterate_internal_links_function(|_, parent| {
                Self::catch_panic_pad_function(
                    parent,
                    || gst::Iterator::from_vec(Vec::new()),
                    |imp| gst::Iterator::from_vec(vec![imp.text_sinkpad.clone()]),
                )
            })
            .build();
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            audio: Mutex::new(Audio::default()),
            audio_changed: Condvar::new(),
            warned_dropped: AtomicBool::new(false),
            audio_sinkpad,
            text_sinkpad,
            srcpad,
            #[cfg(test)]
            fixture: Mutex::new(None),
        }
    }
}

impl ObjectImpl for Diarizer {
    #[expect(clippy::too_many_lines, reason = "one entry per property")]
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let milliseconds = |name, nick, blurb, minimum, maximum, default| {
                glib::ParamSpecUInt::builder(name)
                    .nick(nick)
                    .blurb(blurb)
                    .minimum(minimum)
                    .maximum(maximum)
                    .default_value(default)
                    .mutable_ready()
                    .build()
            };
            let defaults = Settings::default();
            vec![
                glib::ParamSpecString::builder("model-file")
                    .nick("Model file")
                    .blurb("Path of the speaker model ONNX file (required)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("model-profile", defaults.model_profile)
                    .nick("Model profile")
                    .blurb("How to run model-file; auto recognizes a qualified model file by its SHA-256 and refuses any other; custom reads model-config")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("model-config")
                    .nick("Model config")
                    .blurb("Path of a TOML file with the profile for model-profile=custom")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("threads")
                    .nick("Threads")
                    .blurb("ONNX Runtime threads for the model; 0 lets ONNX Runtime choose, which takes every core it finds")
                    .maximum(256)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecDouble::builder("match-threshold")
                    .nick("Match threshold")
                    .blurb("Cosine similarity at which a cue belongs to an existing speaker; 0 uses the model profile's (0.4 for wespeaker-resnet34)")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(defaults.match_threshold)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecDouble::builder("short-match-threshold")
                    .nick("Short match threshold")
                    .blurb("Cosine similarity at which a cue too short to start a speaker goes to the closest one, without refining it; 0 uses the model profile's (0.3 for wespeaker-resnet34); at match-threshold or above, short cues match like any other")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(defaults.short_match_threshold)
                    .mutable_ready()
                    .build(),
                milliseconds(
                    "min-new-duration",
                    "Minimum new-speaker duration",
                    "A cue that matches no speaker starts a new one if its audio is at least this many milliseconds long; a shorter one goes to the closest speaker at short-match-threshold, or gets an unknown speaker",
                    0,
                    60_000,
                    defaults.min_new_duration,
                ),
                milliseconds(
                    "min-update-duration",
                    "Minimum update duration",
                    "A cue refines its speaker's voice if its audio is at least this many milliseconds long",
                    0,
                    60_000,
                    defaults.min_update_duration,
                ),
                glib::ParamSpecUInt::builder("max-speakers")
                    .nick("Maximum speakers")
                    .blurb("Once this many speakers exist, a cue that matches none goes to the closest one; 0 is unlimited")
                    .maximum(1000)
                    .default_value(defaults.max_speakers)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoxed::builder::<gst::Structure>("speaker-names")
                    .nick("Speaker names")
                    .blurb("Speaker names keyed by S{n} or speaker id, such as \"names, S1=Lousma, S2=Haise\"; applies to cues pushed after it is set")
                    .mutable_playing()
                    .build(),
                glib::ParamSpecString::builder("label-format")
                    .nick("Label format")
                    .blurb("Prefix added to the text of a cue with a known speaker, with {label} (the name, or S{n}), {name}, {n}, {id} and {confidence}; empty leaves the text unchanged")
                    .default_value(Some(defaults.label_format.as_str()))
                    .mutable_playing()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("label-mode", defaults.label_mode)
                    .nick("Label mode")
                    .blurb("Which cues get the label-format prefix")
                    .mutable_playing()
                    .build(),
                glib::ParamSpecString::builder("unknown-label")
                    .nick("Unknown label")
                    .blurb("Prefix added to the text of a cue whose speaker is unknown")
                    .default_value(Some(defaults.unknown_label.as_str()))
                    .mutable_playing()
                    .build(),
                milliseconds(
                    "audio-buffer-duration",
                    "Audio buffer duration",
                    "Audio kept for cues that have not arrived yet, in milliseconds; it must cover how far the text runs behind the audio",
                    1000,
                    3_600_000,
                    defaults.audio_buffer_duration,
                ),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            let element = |args: &[glib::Value]| {
                args.first()
                    .and_then(|value| value.get::<super::Diarizer>().ok())
            };
            let text = |args: &[glib::Value], index: usize| {
                args.get(index)
                    .and_then(|value| value.get::<Option<String>>().ok())
                    .flatten()
                    .unwrap_or_default()
            };
            vec![
                glib::subclass::Signal::builder("set-speaker-name")
                    .param_types([String::static_type(), String::static_type()])
                    .return_type::<bool>()
                    .action()
                    .class_handler(move |args| {
                        let done = element(args).is_some_and(|element| {
                            element
                                .imp()
                                .set_speaker_name(&text(args, 1), &text(args, 2))
                        });
                        Some(done.to_value())
                    })
                    .build(),
                glib::subclass::Signal::builder("merge-speakers")
                    .param_types([String::static_type(), String::static_type()])
                    .return_type::<bool>()
                    .action()
                    .class_handler(move |args| {
                        let done = element(args).is_some_and(|element| {
                            element.imp().merge_speakers(&text(args, 1), &text(args, 2))
                        });
                        Some(done.to_value())
                    })
                    .build(),
            ]
        });
        SIGNALS.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = lock(&self.settings);
        let text = || value.get::<Option<String>>().ok().flatten();
        let number = |current: u32| value.get().unwrap_or(current);
        match pspec.name() {
            "model-file" => settings.model_file = text(),
            "model-profile" => {
                settings.model_profile = value.get().unwrap_or(settings.model_profile);
            }
            "model-config" => settings.model_config = text().filter(|path| !path.is_empty()),
            "threads" => settings.threads = number(settings.threads),
            "match-threshold" => {
                settings.match_threshold = value.get().unwrap_or(settings.match_threshold);
            }
            "short-match-threshold" => {
                settings.short_match_threshold =
                    value.get().unwrap_or(settings.short_match_threshold);
            }
            "min-new-duration" => settings.min_new_duration = number(settings.min_new_duration),
            "min-update-duration" => {
                settings.min_update_duration = number(settings.min_update_duration);
            }
            "max-speakers" => settings.max_speakers = number(settings.max_speakers),
            "speaker-names" => {
                let structure = value.get::<Option<gst::Structure>>().ok().flatten();
                settings.set_names(structure.as_ref());
            }
            "label-format" => settings.label_format = text().unwrap_or_default(),
            "label-mode" => settings.label_mode = value.get().unwrap_or(settings.label_mode),
            "unknown-label" => settings.unknown_label = text().unwrap_or_default(),
            "audio-buffer-duration" => {
                settings.audio_buffer_duration = number(settings.audio_buffer_duration);
            }
            other => gst::error!(CAT, imp = self, "unknown property {other}"),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = lock(&self.settings);
        match pspec.name() {
            "model-file" => settings.model_file.to_value(),
            "model-profile" => settings.model_profile.to_value(),
            "model-config" => settings.model_config.to_value(),
            "threads" => settings.threads.to_value(),
            "match-threshold" => settings.match_threshold.to_value(),
            "short-match-threshold" => settings.short_match_threshold.to_value(),
            "min-new-duration" => settings.min_new_duration.to_value(),
            "min-update-duration" => settings.min_update_duration.to_value(),
            "max-speakers" => settings.max_speakers.to_value(),
            "speaker-names" => settings.names_structure().to_value(),
            "label-format" => settings.label_format.to_value(),
            "label-mode" => settings.label_mode.to_value(),
            "unknown-label" => settings.unknown_label.to_value(),
            "audio-buffer-duration" => settings.audio_buffer_duration.to_value(),
            other => {
                gst::error!(CAT, imp = self, "unknown property {other}");
                glib::Value::from_type(pspec.value_type())
            }
        }
    }

    #[expect(clippy::expect_used, reason = "fixed pad names are unique")]
    fn constructed(&self) {
        self.parent_constructed();
        let element = self.obj();
        element
            .add_pad(&self.audio_sinkpad)
            .expect("add audio_sink");
        element.add_pad(&self.text_sinkpad).expect("add text_sink");
        element.add_pad(&self.srcpad).expect("add src");
    }
}

impl GstObjectImpl for Diarizer {}

impl ElementImpl for Diarizer {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Speaker diarizer",
                "Text/Audio/Filter",
                "Labels timed text cues with who spoke them, using a speaker embedding model",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            [
                ("audio_sink", gst::PadDirection::Sink, audio_caps(None)),
                ("text_sink", gst::PadDirection::Sink, text_caps()),
                ("src", gst::PadDirection::Src, text_caps()),
            ]
            .into_iter()
            .filter_map(|(name, direction, caps)| {
                gst::PadTemplate::new(name, direction, gst::PadPresence::Always, &caps).ok()
            })
            .collect()
        });
        TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::ReadyToPaused
            && let Err(err) = self.start()
        {
            self.post_error_message(err);
            return Err(gst::StateChangeError);
        }
        if transition == gst::StateChange::PausedToReady {
            // A cue waiting for audio holds the text pad's stream lock,
            // which deactivating the pad waits for.
            lock(&self.audio).flushing = true;
            self.audio_changed.notify_all();
        }
        let result = self.parent_change_state(transition);
        if transition == gst::StateChange::PausedToReady
            || (transition == gst::StateChange::ReadyToPaused && result.is_err())
        {
            *lock(&self.state) = State::default();
            *lock(&self.audio) = Audio::default();
            self.warned_dropped.store(false, Ordering::Relaxed);
        }
        result
    }
}

impl Diarizer {
    fn load(&self, settings: &Settings) -> Result<Box<dyn SegmentEmbedder>, gst::ErrorMessage> {
        #[cfg(test)]
        if let Some(fixture) = lock(&self.fixture).as_ref() {
            return Ok(fixture());
        }
        let path = settings.model_file.clone().ok_or_else(|| {
            gst::error_msg!(gst::ResourceError::NotFound, ["model-file is required"])
        })?;
        let threads = usize::try_from(settings.threads).unwrap_or(0);
        let loaded = match (settings.model_profile, settings.model_config.as_deref()) {
            (ModelProfile::Custom, Some(config)) => {
                let text = std::fs::read_to_string(config).map_err(|err| {
                    gst::error_msg!(gst::ResourceError::OpenRead, ["reading {config}: {err}"])
                })?;
                let profile = profile::from_toml(&text).map_err(|err| {
                    gst::error_msg!(gst::ResourceError::Settings, ["{config}: {err}"])
                })?;
                Embedder::new(Path::new(&path), profile, threads)
            }
            (ModelProfile::Custom, None) => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["model-profile=custom needs model-config"]
                ));
            }
            (_, Some(_)) => {
                return Err(gst::error_msg!(
                    gst::ResourceError::Settings,
                    ["model-config is only read with model-profile=custom"]
                ));
            }
            (profile, None) => Embedder::load(Path::new(&path), profile.name(), threads),
        };
        let embedder = loaded.map_err(|err| match err {
            crate::embedder::Error::Io { .. } => {
                gst::error_msg!(gst::ResourceError::OpenRead, ["{err}"])
            }
            _ => gst::error_msg!(gst::LibraryError::Init, ["{path}: {err}"]),
        })?;
        // A custom profile without checksums names no model file to check.
        if !embedder.profile().checksums.is_empty() && !embedder.is_qualified() {
            gst::warning!(
                CAT,
                imp = self,
                "profile {} was not qualified with this model file (SHA-256 {})",
                embedder.profile().name,
                embedder.checksum()
            );
        }
        gst::info!(
            CAT,
            imp = self,
            "loaded {path} with profile {}: {} values, SHA-256 {}",
            embedder.profile().name,
            embedder.dim(),
            embedder.checksum()
        );
        Ok(Box::new(embedder))
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let settings = lock(&self.settings).clone();
        let embedder = self.load(&settings)?;
        let profile = embedder.profile().clone();
        #[expect(clippy::cast_possible_truncation, reason = "a threshold in 0..1")]
        let threshold = |set: f64, profile: f32| if set > 0.0 { set as f32 } else { profile };
        let rules = Rules {
            matching: threshold(settings.match_threshold, profile.thresholds.matching),
            short_matching: threshold(
                settings.short_match_threshold,
                profile.thresholds.short_matching,
            ),
            min_new_ms: u64::from(settings.min_new_duration),
            min_update_ms: u64::from(settings.min_update_duration),
            max_speakers: settings.max_speakers,
        };
        let rate = profile.sample_rate();
        *lock(&self.state) = State {
            embedder: Some(embedder),
            model: profile.name,
            registry: Some(Registry::new(rules)),
            ..State::default()
        };
        *lock(&self.audio) = Audio {
            rate,
            max_samples: usize::try_from(
                u64::from(settings.audio_buffer_duration) * u64::from(rate) / 1000,
            )
            .unwrap_or(usize::MAX),
            ..Audio::default()
        };
        Ok(())
    }

    fn error(&self, detail: &str) -> gst::FlowError {
        gst::element_imp_error!(self, gst::StreamError::Failed, ["{detail}"]);
        gst::FlowError::Error
    }

    // Audio pad.

    fn audio_chain(&self, buffer: &gst::BufferRef) -> Result<gst::FlowSuccess, gst::FlowError> {
        let samples: Vec<f32> = {
            let map = buffer
                .map_readable()
                .map_err(|err| self.error(&err.to_string()))?;
            let (chunks, _) = map.as_slice().as_chunks::<4>();
            chunks.iter().map(|b| f32::from_le_bytes(*b)).collect()
        };
        {
            let mut audio = lock(&self.audio);
            if !audio.caps {
                return Err(gst::FlowError::NotNegotiated);
            }
            let running_time = buffer
                .pts()
                .and_then(|pts| audio.segment.as_ref()?.to_running_time(pts));
            let at = match running_time {
                Some(running_time) => audio.index(running_time),
                None if audio.ring.is_started() => audio.ring.end(),
                None => {
                    gst::debug!(CAT, imp = self, "dropping untimed audio {buffer:?}");
                    return Ok(gst::FlowSuccess::Ok);
                }
            };
            let jitter = u64::from(audio.rate) * JITTER_MS / 1000;
            let (max, needed) = (audio.max_samples, audio.needed_from);
            let discont = buffer.flags().contains(gst::BufferFlags::DISCONT);
            if audio.ring.push(at, &samples, jitter, max, discont) {
                // A new timeline: cues of the old one say nothing about it.
                gst::debug!(CAT, imp = self, "audio timestamps jumped; buffering again");
                audio.needed_from = 0;
            } else {
                audio.ring.discard_before(needed);
            }
            audio.starved = false;
        }
        self.audio_changed.notify_all();
        Ok(gst::FlowSuccess::Ok)
    }

    fn audio_event(&self, event: &gst::Event) -> bool {
        let mut audio = lock(&self.audio);
        let handled = match event.view() {
            gst::EventView::StreamStart(_) | gst::EventView::FlushStop(_) => {
                audio.reset();
                true
            }
            gst::EventView::Caps(caps) => {
                let rate = (audio.rate > 0).then_some(audio.rate);
                audio.caps = caps.caps().is_fixed() && caps.caps().is_subset(&audio_caps(rate));
                if !audio.caps {
                    gst::warning!(CAT, imp = self, "refusing audio caps {}", caps.caps());
                }
                audio.caps
            }
            gst::EventView::Segment(segment) => {
                let Some(segment) = segment
                    .segment()
                    .downcast_ref::<gst::ClockTime>()
                    .filter(|segment| segment.rate().to_bits() == 1_f64.to_bits())
                else {
                    drop(audio);
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Format,
                        ["audio_sink needs a forward, unit-rate TIME segment"]
                    );
                    return false;
                };
                audio.segment = Some(segment.clone());
                true
            }
            gst::EventView::Gap(gap) => {
                let (pts, duration) = gap.get();
                let end = pts.saturating_add(duration.unwrap_or(gst::ClockTime::ZERO));
                if let Some(running_time) = audio
                    .segment
                    .as_ref()
                    .and_then(|segment| segment.to_running_time(end))
                {
                    let (index, max) = (audio.index(running_time), audio.max_samples);
                    audio.ring.fill_to(index, max);
                    audio.starved = false;
                }
                true
            }
            gst::EventView::Eos(_) => {
                audio.eos = true;
                true
            }
            // Audio events stay here: the output follows the text.
            _ => true,
        };
        drop(audio);
        self.audio_changed.notify_all();
        handled
    }

    fn audio_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        let rate = {
            let audio = lock(&self.audio);
            (audio.rate > 0).then_some(audio.rate)
        };
        match query.view_mut() {
            gst::QueryViewMut::Caps(query) => {
                let caps = audio_caps(rate);
                let result = query.filter().map_or_else(
                    || caps.clone(),
                    |filter| filter.intersect_with_mode(&caps, gst::CapsIntersectMode::First),
                );
                query.set_result(&result);
                true
            }
            gst::QueryViewMut::AcceptCaps(query) => {
                let accepted = query.caps().is_subset(&audio_caps(rate));
                query.set_result(accepted);
                true
            }
            _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
        }
    }

    // Text pad.

    fn text_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::FlushStart(_) => {
                lock(&self.audio).flushing = true;
                self.audio_changed.notify_all();
            }
            gst::EventView::FlushStop(_) => {
                {
                    let mut audio = lock(&self.audio);
                    audio.flushing = false;
                    audio.needed_from = 0;
                }
                let mut state = lock(&self.state);
                state.previous = None;
                state.unknown_shown = false;
            }
            gst::EventView::StreamStart(_) => {
                let mut state = lock(&self.state);
                state.caps = false;
                state.previous = None;
                state.unknown_shown = false;
            }
            gst::EventView::Caps(caps) => {
                let valid = caps.caps().is_fixed() && caps.caps().is_subset(&text_caps());
                lock(&self.state).caps = valid;
                return valid && self.srcpad.push_event(gst::event::Caps::new(&text_caps()));
            }
            gst::EventView::Segment(segment) => {
                let Some(segment) = segment
                    .segment()
                    .downcast_ref::<gst::ClockTime>()
                    .filter(|segment| segment.rate().to_bits() == 1_f64.to_bits())
                else {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Format,
                        ["text_sink needs a forward, unit-rate TIME segment"]
                    );
                    return false;
                };
                lock(&self.state).segment = Some(segment.clone());
            }
            gst::EventView::Gap(gap) => {
                // No cue will start inside a gap, so its audio can go.
                let (pts, duration) = gap.get();
                let end = pts.saturating_add(duration.unwrap_or(gst::ClockTime::ZERO));
                let running_time = lock(&self.state)
                    .segment
                    .as_ref()
                    .and_then(|segment| segment.to_running_time(end));
                if let Some(running_time) = running_time {
                    let mut audio = lock(&self.audio);
                    let index = audio.index(running_time);
                    audio.needed_from = audio.needed_from.max(index);
                }
            }
            _ => (),
        }
        gst::Pad::event_default(pad, Some(&*self.obj()), event)
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        if let gst::QueryViewMut::Latency(latency) = query.view_mut() {
            let mut upstream = gst::query::Latency::new();
            if !self.text_sinkpad.peer_query(&mut upstream) {
                return false;
            }
            let (live, min, max) = upstream.result();
            let ours = gst::ClockTime::from_mseconds(PROCESSING_MS);
            latency.set(
                live,
                min.saturating_add(ours),
                max.map(|max| max.saturating_add(ours)),
            );
            true
        } else {
            gst::Pad::query_default(pad, Some(&*self.obj()), query)
        }
    }

    /// The cue's audio range on the sample timeline.
    fn cue_range(&self, buffer: &gst::BufferRef) -> Option<Range<u64>> {
        let pts = buffer.pts()?;
        let end = pts.saturating_add(buffer.duration()?);
        let state = lock(&self.state);
        let segment = state.segment.as_ref()?;
        let (start, end) = (segment.to_running_time(pts)?, segment.to_running_time(end)?);
        drop(state);
        let audio = lock(&self.audio);
        Some(audio.index(start)..audio.index(end))
    }

    /// Waits until the audio of `range` has arrived, the audio ended, or
    /// `AUDIO_WAIT` passed, and returns what is there.
    fn cue_audio(&self, range: Range<u64>) -> Result<Vec<f32>, gst::FlowError> {
        let deadline = Instant::now() + AUDIO_WAIT;
        let mut audio = lock(&self.audio);
        loop {
            if audio.flushing {
                return Err(gst::FlowError::Flushing);
            }
            if audio.eos
                || audio.starved
                || (audio.ring.is_started() && audio.ring.end() >= range.end)
            {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                gst::debug!(CAT, imp = self, "gave up waiting for the audio of a cue");
                audio.starved = true;
                break;
            }
            audio = self
                .audio_changed
                .wait_timeout(audio, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let (pcm, dropped) = audio.ring.slice(range.clone());
        // Later cues start at or after this one.
        audio.needed_from = audio.needed_from.max(range.start);
        let needed = audio.needed_from;
        audio.ring.discard_before(needed);
        drop(audio);
        if dropped && !self.warned_dropped.swap(true, Ordering::Relaxed) {
            gst::warning!(
                CAT,
                imp = self,
                "a cue's audio was dropped before the cue arrived; raise audio-buffer-duration"
            );
        }
        Ok(pcm)
    }

    /// Finds the speaker of a cue whose audio is `pcm`.
    fn identify(
        &self,
        pcm: Option<&[f32]>,
        settings: &Settings,
    ) -> Result<Identified, gst::FlowError> {
        let mut state = lock(&self.state);
        let State {
            embedder,
            registry,
            model,
            previous,
            unknown_shown,
            ..
        } = &mut *state;
        let (Some(embedder), Some(registry)) = (embedder.as_mut(), registry.as_mut()) else {
            return Err(gst::FlowError::Flushing);
        };
        let rate = u64::from(embedder.profile().sample_rate().max(1));
        let outcome = match pcm {
            None => Outcome::Unknown { confidence: None },
            Some([]) => {
                gst::debug!(CAT, imp = self, "no audio for the cue");
                Outcome::Unknown { confidence: None }
            }
            Some(pcm) if rms(pcm) < SILENCE_RMS => {
                gst::debug!(CAT, imp = self, "silent cue");
                Outcome::Unknown { confidence: None }
            }
            Some(pcm) => match embedder.embed(pcm) {
                Ok(embedding) => registry.assign(&embedding, pcm.len() as u64 * 1000 / rate),
                Err(err) if err.is_segment_error() => {
                    gst::debug!(CAT, imp = self, "no speaker for the cue: {err}");
                    Outcome::Unknown { confidence: None }
                }
                Err(err) => return Err(self.error(&format!("speaker model failed: {err}"))),
            },
        };
        let (n, confidence, new) = match outcome {
            Outcome::Speaker { n, confidence, new } => (Some(n), confidence, new),
            Outcome::Unknown { confidence } => (None, confidence, false),
        };
        let speaker = n.and_then(|n| registry.speaker(n));
        let fields = Fields {
            n,
            id: speaker.map(|speaker| speaker.id.as_str()),
            name: n.and_then(|n| speakers::name_of(registry, &settings.names, n)),
            confidence,
        };
        let label = fields.label();
        let changed = n.is_some() && previous.map(|p| registry.current(p)) != n;
        if n.is_some() {
            *previous = n;
        }
        let prefix = if n.is_none() {
            settings.unknown_label.clone()
        } else if changed || *unknown_shown || settings.label_mode == LabelMode::Always {
            speakers::format_label(&settings.label_format, &fields)
        } else {
            String::new()
        };
        if n.is_some() {
            *unknown_shown = false;
        } else if !prefix.is_empty() {
            *unknown_shown = true;
        }
        let document = schema::Speaker {
            id: fields.id.map(str::to_owned),
            n,
            name: fields.name.map(str::to_owned),
            label: label.clone(),
            confidence,
            new,
            known: false,
            model: model.clone(),
        };
        Ok(Identified {
            document,
            change: label.filter(|_| changed),
            prefix,
        })
    }

    fn text_chain(&self, mut buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
        if !lock(&self.state).caps {
            return Err(gst::FlowError::NotNegotiated);
        }
        let settings = lock(&self.settings).clone();
        let pcm = self
            .cue_range(&buffer)
            .map(|range| self.cue_audio(range))
            .transpose()?;
        let identified = self.identify(pcm.as_deref(), &settings)?;
        let text = {
            let map = buffer
                .map_readable()
                .map_err(|err| self.error(&err.to_string()))?;
            String::from_utf8_lossy(map.as_slice()).into_owned()
        };
        {
            let buffer = buffer.make_mut();
            if !identified.prefix.is_empty() {
                let labelled = format!("{}{text}", identified.prefix);
                buffer.replace_all_memory(gst::Memory::from_slice(labelled.into_bytes()));
            }
            let mut meta = analytics_meta::relation_meta_mut(buffer)
                .map_err(|err| self.error(&err.to_string()))?;
            schema::add_speaker(&mut meta, &identified.document)
                .map_err(|err| self.error(&err.to_string()))?;
        }
        self.post_messages(&identified.document, &buffer, &text);
        if let Some(label) = identified.change {
            let structure = gst::Structure::builder(SPEAKER_CHANGE)
                .field("speaker", label)
                .build();
            let _pushed = self
                .srcpad
                .push_event(gst::event::CustomDownstream::builder(structure).build());
        }
        self.srcpad.push(buffer)
    }

    fn post_messages(&self, speaker: &schema::Speaker, buffer: &gst::BufferRef, text: &str) {
        let element = self.obj();
        let post = |structure: gst::Structure| {
            let _posted = element.post_message(
                gst::message::Element::builder(structure)
                    .src(&*element)
                    .build(),
            );
        };
        if speaker.new {
            post(
                gst::Structure::builder("diarizer/speaker-new")
                    .field_if_some("n", speaker.n)
                    .field_if_some("id", speaker.id.as_deref())
                    .field_if_some("pts", buffer.pts())
                    .field("text", text)
                    .build(),
            );
        }
        post(
            gst::Structure::builder("diarizer/speaker")
                .field_if_some("id", speaker.id.as_deref())
                .field_if_some("n", speaker.n)
                .field_if_some("name", speaker.name.as_deref())
                .field_if_some("label", speaker.label.as_deref())
                .field_if_some("confidence", speaker.confidence.map(f64::from))
                .field("new", speaker.new)
                .field("known", speaker.known)
                .field("model", speaker.model.as_str())
                .field_if_some("pts", buffer.pts())
                .field_if_some("duration", buffer.duration())
                .field("text", text)
                .build(),
        );
    }

    // Action signals.

    fn set_speaker_name(&self, key: &str, name: &str) -> bool {
        // An empty name clears every key the speaker goes by.
        let keys = {
            let state = lock(&self.state);
            let registry = state.registry.as_ref();
            if !speakers::is_key(registry, key) {
                gst::warning!(CAT, imp = self, "no speaker {key} to name");
                return false;
            }
            registry
                .and_then(|registry| Some(registry.keys_of(registry.find(key)?.n)))
                .unwrap_or_else(|| vec![key.to_owned()])
        };
        let mut settings = lock(&self.settings);
        if name.is_empty() {
            for key in &keys {
                settings.names.remove(key);
            }
        } else {
            settings.names.insert(key.to_owned(), name.to_owned());
        }
        drop(settings);
        self.obj().notify("speaker-names");
        true
    }

    fn merge_speakers(&self, keep: &str, drop: &str) -> bool {
        let merged = lock(&self.state)
            .registry
            .as_mut()
            .and_then(|registry| registry.merge(keep, drop));
        if let Some((kept, dropped)) = merged {
            gst::info!(CAT, imp = self, "merged S{dropped} into S{kept}");
            true
        } else {
            gst::warning!(CAT, imp = self, "cannot merge {drop} into {keep}");
            false
        }
    }
}
