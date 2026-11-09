use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use super::stitch::{Cue, Limits, Stitcher, Window};
use super::transcriber::{self, Request, Transcriber};

#[cfg(test)]
mod tests;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "cactuswhistle",
        gst::DebugColorFlags::empty(),
        Some("Cactus Whistle transcription"),
    )
});

const RATE: usize = 16_000;
const SAMPLES_PER_MS: usize = RATE / 1000;
/// Offline input is capped at an hour (230 MB of samples).
const MAX_SAMPLES: usize = 3600 * RATE;
/// Later input within this distance of the expected position is contiguous.
const JITTER_SAMPLES: usize = RATE / 50;
/// Added to the automatic latency: a window takes 0.1–0.3 s to transcribe
/// on a laptop CPU, and a `describe` holding the engine lock can delay it by
/// about a second.
const AUTO_PROCESSING_MS: i64 = 2000;
/// The engine stream can confirm a word that started up to about a step and
/// 0.75 s before the audio it had received (measured at 0.5–2 s steps), so
/// silence is settled this much and a step behind it.
const STREAM_SETTLE_MS: i64 = 1000;
/// The largest window part a property can ask for. Whistle transcribes up
/// to 30 seconds in one pass.
const MAX_WINDOW_MS: u32 = 30_000;
/// Codes Whistle can be told to expect.
const LANGUAGES: [&str; 7] = ["en", "de", "fr", "es", "it", "nl", "pl"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstCactusWhistleMode")]
pub enum Mode {
    /// Stateless transcriptions of overlapping windows, stitched here.
    #[default]
    #[enum_value(name = "Transcribe rolling windows and stitch them", nick = "windows")]
    Windows,
    /// The engine's streaming transcriber, one per process.
    #[enum_value(
        name = "The engine's streaming transcriber, one per process",
        nick = "stream"
    )]
    Stream,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Settings {
    mode: Mode,
    model_file: Option<String>,
    language: Option<String>,
    keywords: Option<String>,
    threads: u32,
    chunk_duration: u32,
    context_duration: u32,
    holdback: u32,
    agreement: u32,
    latency: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::Windows,
            model_file: None,
            language: None,
            keywords: None,
            threads: 0,
            chunk_duration: 2000,
            context_duration: 10_000,
            holdback: 1000,
            agreement: 0,
            latency: 0,
        }
    }
}

impl Settings {
    fn live(&self) -> bool {
        self.chunk_duration > 0
    }

    /// The longest a cue's PTS can be behind its push: the cue stays open
    /// for up to `max_open_ms`, its last word is committed a chunk (and the
    /// agreement margin) later, or for the stream two steps and the settle
    /// margin later, then processing.
    fn latency(&self) -> gst::ClockTime {
        let ms = match self.latency {
            0 if self.live() => {
                let chunk = i64::from(self.chunk_duration);
                let commit = match self.mode {
                    Mode::Windows => chunk + i64::from(self.agreement),
                    Mode::Stream => 2 * chunk + STREAM_SETTLE_MS,
                };
                Limits::default().max_open_ms() + commit + AUTO_PROCESSING_MS
            }
            latency => i64::from(latency),
        };
        gst::ClockTime::from_mseconds(u64::try_from(ms).unwrap_or(0))
    }

    fn stitcher(&self) -> Stitcher {
        if self.mode == Mode::Stream {
            // The engine commits each word once; only cues are left to do.
            return Stitcher::trusted(Limits::default());
        }
        let pending = if self.live() {
            2 * i64::from(self.chunk_duration) + i64::from(self.holdback)
        } else {
            i64::MAX
        };
        Stitcher::new(
            i64::from(self.context_duration),
            i64::from(self.holdback),
            pending,
            Limits::default(),
        )
        .with_agreement_ms(i64::from(self.agreement))
    }

    /// Checks the settings and makes the request every window sends.
    fn request(&self) -> Result<Request, String> {
        let language = self
            .language
            .clone()
            .filter(|language| !language.is_empty());
        if let Some(language) = &language
            && !LANGUAGES.contains(&language.as_str())
        {
            return Err(format!(
                "language {language} is not one of {}",
                LANGUAGES.join(", ")
            ));
        }
        match self.mode {
            Mode::Windows if self.live() && self.holdback >= self.chunk_duration => {
                return Err("holdback must be shorter than chunk-duration".to_owned());
            }
            Mode::Stream if !self.live() => {
                return Err("mode=stream needs a chunk-duration".to_owned());
            }
            Mode::Windows | Mode::Stream => {}
        }
        let keywords = self.keywords.as_deref().map(|keywords| {
            keywords
                .split(',')
                .map(str::trim)
                .filter(|keyword| !keyword.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        });
        Ok(Request {
            language,
            keywords: keywords.filter(|keywords| !keywords.is_empty()),
        })
    }
}

/// Recent audio on the stream timeline; `first` is the index of `samples[0]`.
#[derive(Default)]
struct Audio {
    first: usize,
    samples: Vec<f32>,
}

impl Audio {
    fn end(&self) -> usize {
        self.first + self.samples.len()
    }

    fn discard_before(&mut self, index: usize) {
        let count = index.saturating_sub(self.first).min(self.samples.len());
        self.samples.drain(..count);
        self.first += count;
    }
}

#[derive(Default)]
struct State {
    /// The settings the element started with; the streaming thread uses
    /// these, whatever the properties say later.
    settings: Settings,
    caps: bool,
    request: Request,
    /// PTS of the stream timeline's first sample.
    start: Option<gst::ClockTime>,
    audio: Audio,
    /// In `mode=stream`, the first sample not yet given to the engine.
    sent: usize,
    /// In `mode=stream`, the engine holds audio of this stream.
    streaming: bool,
    /// In `mode=windows`, where the last window ended and the commit point
    /// it started from, in milliseconds.
    windowed_until: i64,
    windowed_committed: i64,
    stitcher: Option<Stitcher>,
    /// Output is complete up to here: the end of the last cue or gap.
    pushed_until: Option<gst::ClockTime>,
    /// The language of the last tag pushed.
    language: String,
}

impl State {
    fn reset_audio(&mut self, settings: &Settings) {
        self.start = None;
        self.audio = Audio::default();
        self.sent = 0;
        self.streaming = false;
        self.windowed_until = 0;
        self.windowed_committed = 0;
        self.stitcher = Some(settings.stitcher());
        self.pushed_until = None;
    }

    /// Zero-fills up to `pts`, so a missing stretch keeps later words at
    /// their media time.
    fn fill_to(&mut self, pts: gst::ClockTime, live: bool) -> Result<(), &'static str> {
        let start = *self.start.get_or_insert(pts);
        let target = pts
            .checked_sub(start)
            .map_or(0, |offset| sample_index(offset.nseconds()));
        if target > self.audio.end() + JITTER_SAMPLES {
            if !live && target > MAX_SAMPLES {
                return Err("input exceeds an hour; set chunk-duration");
            }
            let missing = target - self.audio.end();
            self.audio
                .samples
                .resize(self.audio.samples.len() + missing, 0.0);
        }
        Ok(())
    }
}

fn sample_index(nanoseconds: u64) -> usize {
    usize::try_from(nanoseconds / (1_000_000_000 / RATE as u64)).unwrap_or(usize::MAX)
}

fn sample_ms(index: usize) -> i64 {
    i64::try_from(index / SAMPLES_PER_MS).unwrap_or(i64::MAX)
}

fn ms_sample(ms: i64) -> usize {
    usize::try_from(ms.max(0)).unwrap_or(0) * SAMPLES_PER_MS
}

fn seconds(ms: i64) -> f64 {
    f64::from(i32::try_from(ms).unwrap_or(i32::MAX)) / 1000.0
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn text_caps() -> gst::Caps {
    gst::Caps::builder("text/x-raw")
        .field("format", "utf8")
        .build()
}

/// One window's audio, copied out of the buffer.
struct Job {
    start: gst::ClockTime,
    samples: Vec<f32>,
    /// Stream-timeline index of the first sample.
    first: usize,
    window: Window,
}

#[cfg(test)]
pub(super) type Fixture = std::sync::Arc<dyn Fn() -> Box<dyn Transcriber> + Send + Sync>;

pub struct CactusWhistle {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    /// Held for each transcription, so stopping waits for one in progress.
    transcriber: Mutex<Option<Box<dyn Transcriber>>>,
    flushing: AtomicBool,
    sinkpad: gst::Pad,
    srcpad: gst::Pad,
    #[cfg(test)]
    pub(super) fixture: Mutex<Option<Fixture>>,
}

#[glib::object_subclass]
impl ObjectSubclass for CactusWhistle {
    const NAME: &'static str = "GstCactusWhistle";
    type Type = super::CactusWhistle;
    type ParentType = gst::Element;

    #[expect(
        clippy::expect_used,
        reason = "class pad templates are installed before construction"
    )]
    fn with_class(class: &Self::Class) -> Self {
        let sinkpad =
            gst::Pad::builder_from_template(&class.pad_template("sink").expect("sink template"))
                .chain_function(|_, parent, buffer| {
                    Self::catch_panic_pad_function(
                        parent,
                        || Err(gst::FlowError::Error),
                        |imp| imp.chain(&buffer),
                    )
                })
                .event_function(|_, parent, event| {
                    Self::catch_panic_pad_function(parent, || false, |imp| imp.sink_event(event))
                })
                .build();
        let srcpad =
            gst::Pad::builder_from_template(&class.pad_template("src").expect("src template"))
                .query_function(|pad, parent, query| {
                    Self::catch_panic_pad_function(
                        parent,
                        || false,
                        |imp| imp.src_query(pad, query),
                    )
                })
                .build();
        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            transcriber: Mutex::new(None),
            flushing: AtomicBool::new(true),
            sinkpad,
            srcpad,
            #[cfg(test)]
            fixture: Mutex::new(None),
        }
    }
}

impl ObjectImpl for CactusWhistle {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let milliseconds = |name, nick, blurb, maximum, default| {
                glib::ParamSpecUInt::builder(name)
                    .nick(nick)
                    .blurb(blurb)
                    .maximum(maximum)
                    .default_value(default)
                    .mutable_ready()
                    .build()
            };
            let defaults = Settings::default();
            vec![
                glib::ParamSpecEnum::builder_with_default("mode", defaults.mode)
                    .nick("Mode")
                    .blurb("windows transcribes rolling windows and stitches them; stream uses the engine's streaming transcriber, of which a process has one")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("model-file")
                    .nick("Model file")
                    .blurb("Path of the Whistle .cact weights (required)")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("language")
                    .nick("Language")
                    .blurb("Spoken language: en, de, fr, es, it, nl or pl; empty detects it in each window")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("keywords")
                    .nick("Keywords")
                    .blurb("Comma-separated words to bias recognition towards, such as names")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("threads")
                    .nick("Threads")
                    .blurb("Engine threads, process-global: the first explicit value before weights load wins; 0 keeps the engine default")
                    .maximum(256)
                    .mutable_ready()
                    .build(),
                milliseconds(
                    "chunk-duration",
                    "Chunk duration",
                    "New audio per window or stream step in milliseconds; 0 transcribes the whole recording at EOS (mode=windows)",
                    MAX_WINDOW_MS,
                    defaults.chunk_duration,
                ),
                milliseconds(
                    "context-duration",
                    "Context duration",
                    "Committed audio repeated before each window's new audio, in milliseconds (mode=windows)",
                    MAX_WINDOW_MS,
                    defaults.context_duration,
                ),
                milliseconds(
                    "holdback",
                    "Holdback",
                    "Words ending this close to a window's end wait for the next window, in milliseconds (mode=windows)",
                    MAX_WINDOW_MS,
                    defaults.holdback,
                ),
                milliseconds(
                    "agreement",
                    "Agreement",
                    "Words ending within this many milliseconds of the commit point wait until two consecutive windows transcribe them alike; 0 disables (mode=windows)",
                    MAX_WINDOW_MS,
                    defaults.agreement,
                ),
                milliseconds(
                    "latency",
                    "Latency",
                    "Reported cue delay in milliseconds; 0 uses two chunks (three with agreement or mode=stream) plus 2000",
                    3_600_000,
                    defaults.latency,
                ),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = lock(&self.settings);
        let text = || value.get::<Option<String>>().ok().flatten();
        let number = |current: u32| value.get().unwrap_or(current);
        match pspec.name() {
            "mode" => settings.mode = value.get().unwrap_or(settings.mode),
            "model-file" => settings.model_file = text(),
            "language" => settings.language = text(),
            "keywords" => settings.keywords = text(),
            "threads" => settings.threads = number(settings.threads),
            "chunk-duration" => settings.chunk_duration = number(settings.chunk_duration),
            "context-duration" => settings.context_duration = number(settings.context_duration),
            "holdback" => settings.holdback = number(settings.holdback),
            "agreement" => settings.agreement = number(settings.agreement),
            "latency" => settings.latency = number(settings.latency),
            other => gst::error!(CAT, imp = self, "unknown property {other}"),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = lock(&self.settings);
        match pspec.name() {
            "mode" => settings.mode.to_value(),
            "model-file" => settings.model_file.to_value(),
            "language" => settings.language.to_value(),
            "keywords" => settings.keywords.to_value(),
            "threads" => settings.threads.to_value(),
            "chunk-duration" => settings.chunk_duration.to_value(),
            "context-duration" => settings.context_duration.to_value(),
            "holdback" => settings.holdback.to_value(),
            "agreement" => settings.agreement.to_value(),
            "latency" => settings.latency.to_value(),
            other => {
                gst::error!(CAT, imp = self, "unknown property {other}");
                glib::Value::from_type(pspec.value_type())
            }
        }
    }

    #[expect(clippy::expect_used, reason = "fixed pad names are unique")]
    fn constructed(&self) {
        self.parent_constructed();
        self.obj().add_pad(&self.sinkpad).expect("add sink");
        self.obj().add_pad(&self.srcpad).expect("add src");
    }
}

impl GstObjectImpl for CactusWhistle {}

impl ElementImpl for CactusWhistle {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Transcriber",
                "Text/Audio/Filter",
                "Speech to text filter, using Cactus Whistle",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let audio = gst::Caps::builder("audio/x-raw")
                .field("format", "F32LE")
                .field("layout", "interleaved")
                .field("rate", i32::try_from(RATE).unwrap_or(16_000))
                .field("channels", 1_i32)
                .build();
            [
                ("sink", gst::PadDirection::Sink, audio),
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
        let result = self.parent_change_state(transition);
        if transition == gst::StateChange::PausedToReady
            || (transition == gst::StateChange::ReadyToPaused && result.is_err())
        {
            self.flushing.store(true, Ordering::SeqCst);
            *lock(&self.transcriber) = None;
            *lock(&self.state) = State::default();
        }
        result
    }
}

impl CactusWhistle {
    #[cfg_attr(
        not(test),
        expect(
            clippy::unused_self,
            reason = "tests swap in a fixture held by the element"
        )
    )]
    fn new_transcriber(&self) -> Box<dyn Transcriber> {
        #[cfg(test)]
        if let Some(fixture) = lock(&self.fixture).as_ref() {
            return fixture();
        }
        Box::<transcriber::Engine>::default()
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let settings = lock(&self.settings).clone();
        let request = settings
            .request()
            .map_err(|err| gst::error_msg!(gst::LibraryError::Settings, ["{err}"]))?;
        let path = settings.model_file.clone().ok_or_else(|| {
            gst::error_msg!(gst::ResourceError::NotFound, ["model-file is required"])
        })?;
        let weights = std::fs::read(&path).map_err(|err| {
            gst::error_msg!(gst::ResourceError::OpenRead, ["reading {path}: {err}"])
        })?;
        let mut transcriber = self.new_transcriber();
        let (sha, threads) = transcriber
            .start(&weights, settings.threads)
            .map_err(|err| gst::error_msg!(gst::LibraryError::Init, ["{path}: {err}"]))?;
        match threads {
            cactus::Threads::Ignored { requested, current } => gst::warning!(
                CAT,
                imp = self,
                "threads={requested} is ignored: the process already runs the engine with {current}"
            ),
            cactus::Threads::TooLate { requested } => gst::warning!(
                CAT,
                imp = self,
                "threads={requested} is ignored: weights were loaded before it was set"
            ),
            cactus::Threads::EngineDefault | cactus::Threads::Applied(_) => {}
        }
        gst::info!(CAT, imp = self, "loaded Whistle weights {sha}");
        if settings.mode == Mode::Stream {
            transcriber
                .open_stream()
                .map_err(|err| gst::error_msg!(gst::ResourceError::Busy, ["{err}"]))?;
        }
        *lock(&self.transcriber) = Some(transcriber);
        let mut state = State {
            request,
            settings: settings.clone(),
            ..State::default()
        };
        state.reset_audio(&settings);
        *lock(&self.state) = state;
        self.flushing.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn error(&self, detail: &str) -> gst::FlowError {
        gst::element_imp_error!(self, gst::StreamError::Failed, ["{detail}"]);
        gst::FlowError::Error
    }

    fn chain(&self, buffer: &gst::BufferRef) -> Result<gst::FlowSuccess, gst::FlowError> {
        if self.flushing.load(Ordering::SeqCst) {
            return Err(gst::FlowError::Flushing);
        }
        let settings = lock(&self.state).settings.clone();
        {
            let mut state = lock(&self.state);
            if !state.caps {
                return Err(gst::FlowError::NotNegotiated);
            }
            match buffer.pts() {
                Some(pts) => state
                    .fill_to(pts, settings.live())
                    .map_err(|err| self.error(err))?,
                None if state.start.is_none() => {
                    return Err(self.error("the first audio buffer needs a timestamp"));
                }
                None => (),
            }
            // A GAP buffer's own duration is filled when the next buffer
            // or GAP event arrives; at EOS there is nothing left to fill.
            if !buffer.flags().contains(gst::BufferFlags::GAP) {
                let map = buffer
                    .map_readable()
                    .map_err(|err| self.error(&err.to_string()))?;
                let (samples, _) = map.as_slice().as_chunks::<4>();
                if !settings.live() && state.audio.end() + samples.len() > MAX_SAMPLES {
                    return Err(self.error("input exceeds an hour; set chunk-duration"));
                }
                state
                    .audio
                    .samples
                    .extend(samples.iter().map(|bytes| f32::from_le_bytes(*bytes)));
            }
        }
        self.announce_start();
        if settings.live() {
            while let Some(job) = self.next_job(&settings, false) {
                self.process(&job)?;
            }
        }
        Ok(gst::FlowSuccess::Ok)
    }

    /// Tells downstream, with an empty gap, where the text stream starts, so
    /// sinks preroll before the first cue (or a `tee` branch can stall).
    fn announce_start(&self) {
        let start = {
            let mut state = lock(&self.state);
            match (state.start, state.pushed_until) {
                (Some(start), None) => {
                    state.pushed_until = Some(start);
                    start
                }
                _ => return,
            }
        };
        let _pushed = self.srcpad.push_event(
            gst::event::Gap::builder(start)
                .duration(gst::ClockTime::ZERO)
                .build(),
        );
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        let settings = lock(&self.state).settings.clone();
        match event.view() {
            gst::EventView::FlushStart(_) => {
                self.flushing.store(true, Ordering::SeqCst);
            }
            gst::EventView::FlushStop(_) => {
                self.reset(&settings);
                self.flushing.store(false, Ordering::SeqCst);
            }
            gst::EventView::StreamStart(_) => {
                self.reset(&settings);
                let mut state = lock(&self.state);
                state.caps = false;
                state.language.clear();
            }
            gst::EventView::Caps(caps) => {
                let valid = caps.caps().is_fixed()
                    && caps.caps().is_subset(&self.sinkpad.pad_template_caps());
                lock(&self.state).caps = valid;
                return valid && self.srcpad.push_event(gst::event::Caps::new(&text_caps()));
            }
            gst::EventView::Segment(segment) => {
                let segment = segment.segment();
                if segment.format() != gst::Format::Time
                    || segment.rate().to_bits() != 1_f64.to_bits()
                {
                    gst::element_imp_error!(
                        self,
                        gst::StreamError::Format,
                        ["needs a forward, unit-rate TIME segment"]
                    );
                    return false;
                }
                self.reset(&settings);
            }
            gst::EventView::Gap(gap) => {
                let (pts, duration) = gap.get();
                let end = duration.map_or(pts, |duration| pts.saturating_add(duration));
                let filled = {
                    let mut state = lock(&self.state);
                    state.start.get_or_insert(pts);
                    state.fill_to(end, settings.live())
                };
                if let Err(err) = filled {
                    let _flow = self.error(err);
                    return false;
                }
                self.announce_start();
                if settings.live() {
                    while let Some(job) = self.next_job(&settings, false) {
                        if self.process(&job).is_err() {
                            return false;
                        }
                    }
                }
                return true;
            }
            gst::EventView::Eos(_) => {
                let result = match self.next_job(&settings, true) {
                    Some(job) => self.process(&job),
                    None => self.close_open_cue(),
                };
                if let Err(err) = result
                    && err != gst::FlowError::Flushing
                {
                    return false;
                }
            }
            _ => (),
        }
        self.srcpad.push_event(event)
    }

    fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        if let gst::QueryViewMut::Latency(latency) = query.view_mut() {
            let mut upstream = gst::query::Latency::new();
            if !self.sinkpad.peer_query(&mut upstream) {
                return false;
            }
            let (live, min, max) = upstream.result();
            let ours = lock(&self.settings).latency();
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

    /// Starts the stream timeline again, dropping audio not yet transcribed.
    fn reset(&self, settings: &Settings) {
        if settings.mode == Mode::Stream
            && let Some(transcriber) = lock(&self.transcriber).as_mut()
        {
            transcriber.reset_stream();
        }
        lock(&self.state).reset_audio(settings);
    }

    /// Copies out the next window once a chunk of audio past the commit point
    /// is buffered, or the rest at EOS. No window runs on the same audio
    /// twice, and after one that left the commit point where it was, such as
    /// when a word runs past the holdback, the next waits for a chunk less the
    /// holdback of new audio.
    fn next_job(&self, settings: &Settings, last: bool) -> Option<Job> {
        if settings.mode == Mode::Stream {
            return self.next_step(settings, last);
        }
        let mut state = lock(&self.state);
        let stitcher = state.stitcher.as_ref()?;
        let committed = stitcher.committed_until();
        let context_start = stitcher.context_start();
        let end_ms = sample_ms(state.audio.end());
        let chunk = i64::from(settings.chunk_duration);
        let ready = if last {
            end_ms > committed
        } else {
            end_ms - committed >= chunk
                && end_ms > state.windowed_until
                && (committed > state.windowed_committed
                    || end_ms - state.windowed_until >= chunk - i64::from(settings.holdback))
        };
        if !ready {
            return None;
        }
        let first = ms_sample(context_start).max(state.audio.first);
        let samples = state
            .audio
            .samples
            .get(first - state.audio.first..)?
            .to_vec();
        state.windowed_until = end_ms;
        state.windowed_committed = committed;
        Some(Job {
            start: state.start?,
            samples,
            first,
            window: Window { end_ms, last },
        })
    }

    /// Copies out the audio not yet given to the stream once a chunk of it is
    /// buffered, or the rest at EOS, which also stops the stream.
    fn next_step(&self, settings: &Settings, last: bool) -> Option<Job> {
        let mut state = lock(&self.state);
        let end = state.audio.end();
        let new = end.saturating_sub(state.sent);
        let ready = if last {
            new > 0 || state.streaming
        } else {
            new >= ms_sample(i64::from(settings.chunk_duration))
        };
        if !ready {
            return None;
        }
        let first = state.sent.max(state.audio.first);
        let samples = state
            .audio
            .samples
            .get(first - state.audio.first..)?
            .to_vec();
        state.sent = end;
        state.streaming = !last;
        Some(Job {
            start: state.start?,
            samples,
            first,
            window: Window {
                end_ms: sample_ms(end),
                last,
            },
        })
    }

    /// Runs the engine on one window or stream step and returns its words
    /// on the stream timeline, with how long the engine took.
    fn transcribe(
        &self,
        job: &Job,
        stream: bool,
    ) -> Result<(transcriber::Transcript, Duration), gst::FlowError> {
        let request = lock(&self.state).request.clone();
        let begin = Instant::now();
        let replies = {
            let mut transcriber = lock(&self.transcriber);
            let transcriber = transcriber.as_mut().ok_or(gst::FlowError::Flushing)?;
            if stream {
                transcriber.stream(&job.samples, &request, job.window.last)
            } else {
                transcriber
                    .transcribe(&job.samples, &request)
                    .map(|json| vec![json])
            }
            .map_err(|err| self.error(&format!("transcription failed: {err}")))?
        };
        let elapsed = begin.elapsed();
        if self.flushing.load(Ordering::SeqCst) {
            return Err(gst::FlowError::Flushing);
        }
        // Stream words are timed from the start of the stream, which is the
        // start of the timeline.
        let offset_ms = if stream { 0 } else { sample_ms(job.first) };
        let mut transcript = transcriber::Transcript::default();
        for json in &replies {
            let reply = transcriber::parse(json).map_err(|err| self.error(&err))?;
            transcript
                .words
                .extend(reply.words.into_iter().map(|mut word| {
                    word.start_ms += offset_ms;
                    word.end_ms += offset_ms;
                    word
                }));
            if !reply.language.is_empty() {
                transcript.language = reply.language;
            }
            transcript.pending = reply.pending;
            transcript.received_ms = reply.received_ms;
        }
        Ok((transcript, elapsed))
    }

    /// Transcribes one window or stream step, commits its words, and pushes
    /// the cues that closed.
    fn process(&self, job: &Job) -> Result<(), gst::FlowError> {
        let (stream, chunk_ms) = {
            let state = lock(&self.state);
            (
                state.settings.mode == Mode::Stream,
                i64::from(state.settings.chunk_duration),
            )
        };
        let (transcript, elapsed) = self.transcribe(job, stream)?;
        let window = if stream && !job.window.last {
            // A stream's words are final. Silence counts as settled up to its
            // last word, or, once nothing is pending, a step and a margin
            // behind the audio it received: a word can still be confirmed
            // later that started before `received`.
            let silence = transcript
                .pending
                .is_empty()
                .then(|| transcript.received_ms - chunk_ms - STREAM_SETTLE_MS);
            let last_word = transcript.words.last().map(|word| word.end_ms);
            Window {
                end_ms: silence.max(last_word).unwrap_or(0),
                last: false,
            }
        } else {
            job.window
        };
        let words = transcript.words.len();
        let (cues, committed, safe_ms, language) = {
            let mut state = lock(&self.state);
            let Some(stitcher) = state.stitcher.as_mut() else {
                return Err(gst::FlowError::Flushing);
            };
            let window = Window {
                end_ms: window.end_ms.max(stitcher.committed_until()),
                ..window
            };
            let committed = stitcher.accept(transcript.words, window);
            let safe_ms = stitcher
                .open_since()
                .unwrap_or(i64::MAX)
                .min(stitcher.committed_until());
            let language = (!transcript.language.is_empty()
                && transcript.language != state.language)
                .then(|| {
                    state.language.clone_from(&transcript.language);
                    transcript.language.clone()
                });
            (committed.cues, committed.words.len(), safe_ms, language)
        };
        let start_ms = sample_ms(job.first);
        gst::debug!(
            CAT,
            imp = self,
            "{:.2}-{:.2} s in {elapsed:?}: {words} words, {committed} committed, {} cues",
            seconds(start_ms),
            seconds(job.window.end_ms),
            cues.len()
        );
        let summary = gst::Structure::builder("cactuswhistle")
            .field("window-start", seconds(start_ms))
            .field("window-end", seconds(job.window.end_ms))
            .field("transcription-seconds", elapsed.as_secs_f64())
            .field("words", u32::try_from(words).unwrap_or(u32::MAX))
            .field("committed", u32::try_from(committed).unwrap_or(u32::MAX))
            .field("cues", u32::try_from(cues.len()).unwrap_or(u32::MAX))
            .field("language", transcript.language.as_str())
            .field("pending", transcript.pending.as_str())
            .build();
        let _posted = self.obj().post_message(
            gst::message::Element::builder(summary)
                .src(&*self.obj())
                .build(),
        );
        if let Some(language) = language {
            let mut tags = gst::TagList::new();
            tags.make_mut()
                .add::<gst::tags::LanguageCode>(&language.as_str(), gst::TagMergeMode::Replace);
            let _pushed = self.srcpad.push_event(gst::event::Tag::new(tags));
        }
        self.push(cues, safe_ms, job.start)?;
        let mut state = lock(&self.state);
        let keep = if stream {
            Some(state.sent)
        } else {
            state
                .stitcher
                .as_ref()
                .map(|stitcher| ms_sample(stitcher.context_start()))
        };
        if let Some(keep) = keep {
            state.audio.discard_before(keep);
        }
        Ok(())
    }

    /// Pushes the open cue at EOS when no audio is left to transcribe.
    fn close_open_cue(&self) -> Result<(), gst::FlowError> {
        let (cues, start) = {
            let mut state = lock(&self.state);
            let Some(start) = state.start else {
                return Ok(());
            };
            let Some(stitcher) = state.stitcher.as_mut() else {
                return Ok(());
            };
            let end_ms = stitcher.committed_until();
            let committed = stitcher.accept(Vec::new(), Window { end_ms, last: true });
            (committed.cues, start)
        };
        self.push(cues, i64::MAX, start)
    }

    /// Pushes cues, then a gap up to `safe_ms`, before which no later cue
    /// can start.
    fn push(
        &self,
        cues: Vec<Cue>,
        safe_ms: i64,
        start: gst::ClockTime,
    ) -> Result<(), gst::FlowError> {
        let at = |ms: i64| {
            start.saturating_add(gst::ClockTime::from_mseconds(
                u64::try_from(ms).unwrap_or(0),
            ))
        };
        for cue in cues {
            if self.flushing.load(Ordering::SeqCst) {
                return Err(gst::FlowError::Flushing);
            }
            // A held word can come back a little earlier than the end of the
            // cue before it.
            let pushed = lock(&self.state).pushed_until.unwrap_or(start);
            let pts = at(cue.start_ms).max(pushed);
            let end = at(cue.end_ms).max(pts.saturating_add(gst::ClockTime::MSECOND));
            let mut buffer = gst::Buffer::from_slice(cue.text.into_bytes());
            {
                let buffer = buffer.make_mut();
                buffer.set_pts(pts);
                buffer.set_duration(end.saturating_sub(pts));
            }
            lock(&self.state).pushed_until = Some(end);
            self.srcpad.push(buffer)?;
        }
        if safe_ms < i64::MAX {
            let pushed = lock(&self.state).pushed_until.unwrap_or(start);
            let until = at(safe_ms);
            if until > pushed {
                lock(&self.state).pushed_until = Some(until);
                let gap = gst::event::Gap::builder(pushed)
                    .duration(until.saturating_sub(pushed))
                    .build();
                let _pushed = self.srcpad.push_event(gap);
            }
        }
        Ok(())
    }
}
