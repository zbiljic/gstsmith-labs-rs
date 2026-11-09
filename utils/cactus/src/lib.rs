//! A safe API over the process-wide Cactus engine.
//!
//! The C ABI has no handle: one engine per process holds every loaded model
//! (text, speech, vision), and its calls are not thread-safe. [`Engine::lock`]
//! returns the only way in, a guard over one process-wide mutex, so vision
//! and speech work is serialized: a 300 ms `describe` delays a `transcribe`
//! by 300 ms.

use std::ffi::{CStr, CString, c_char, c_int};
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

pub mod weights;

pub use weights::Kind;

/// The engine release this crate links.
pub const ENGINE_VERSION: &str = cactus_sys::ENGINE_VERSION;
/// Initial size of the output buffer shared by `describe` and `transcribe`.
pub const INITIAL_OUTPUT: usize = 1 << 20;
/// The largest output buffer this crate will grow to.
pub const MAX_OUTPUT: usize = 256 << 20;
/// Stream calls cannot be retried with a bigger buffer, so the buffer is
/// sized for them first: this much per second of audio in the step, and at
/// least [`INITIAL_OUTPUT`]. A second of speech with word times needs well
/// under 1 KiB.
const STREAM_BYTES_PER_SECOND: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The caller's input cannot be passed to the engine.
    Input(String),
    /// The weights are not what the call needs.
    Weights(String),
    /// Different weights of this kind are already loaded in this process.
    ModelInUse {
        kind: Kind,
        loaded: String,
        requested: String,
    },
    /// Another caller holds the engine's one speech stream.
    StreamInUse,
    /// The stream was closed, or never opened by this caller.
    StreamClosed,
    /// The engine reported a failure; `detail` is `cactus_last_error`.
    Engine {
        operation: &'static str,
        code: i32,
        detail: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(detail) | Self::Weights(detail) => f.write_str(detail),
            Self::ModelInUse {
                kind,
                loaded,
                requested,
            } => write!(
                f,
                "{kind} weights with SHA-256 {loaded} are loaded, and the process-wide engine cannot replace them with {requested}"
            ),
            Self::StreamInUse => f.write_str(
                "the process-wide engine holds one speech stream, and another caller is using it",
            ),
            Self::StreamClosed => f.write_str("the speech stream is not open"),
            Self::Engine {
                operation,
                code,
                detail,
            } => write!(f, "cactus {operation} failed ({code}): {detail}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Which models the engine holds (`cactus_models()`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Models(c_int);

impl Models {
    #[must_use]
    pub fn text(self) -> bool {
        self.0 & cactus_sys::CACTUS_TEXT != 0
    }

    #[must_use]
    pub fn speech(self) -> bool {
        self.0 & cactus_sys::CACTUS_SPEECH != 0
    }

    #[must_use]
    pub fn vision(self) -> bool {
        self.0 & cactus_sys::CACTUS_VISION != 0
    }
}

/// What `set_threads` did. The engine setting is process-global and applies
/// only to models loaded afterwards, so set threads before loading weights.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Threads {
    /// 0 requested: the engine keeps its own choice.
    EngineDefault,
    /// The value was applied, or was already in effect.
    Applied(u32),
    /// Another explicit value came first and stays in effect.
    Ignored { requested: u32, current: u32 },
    /// A model is already loaded and keeps the engine default.
    TooLate { requested: u32 },
}

/// A tightly packed RGB24 image: `width * height * 3` bytes, no row padding.
#[derive(Clone, Copy, Debug)]
pub struct PackedRgb<'a> {
    data: &'a [u8],
    width: c_int,
    height: c_int,
}

impl<'a> PackedRgb<'a> {
    pub fn new(data: &'a [u8], width: u32, height: u32) -> Result<Self> {
        let expected = usize::try_from(width)
            .ok()
            .zip(usize::try_from(height).ok())
            .and_then(|(w, h)| w.checked_mul(h)?.checked_mul(3));
        let (Ok(w), Ok(h)) = (c_int::try_from(width), c_int::try_from(height)) else {
            return Err(Error::Input(format!("image {width}x{height} is too large")));
        };
        if width == 0 || height == 0 || expected != Some(data.len()) {
            return Err(Error::Input(format!(
                "a {width}x{height} packed RGB image needs {expected:?} bytes, got {}",
                data.len()
            )));
        }
        Ok(Self {
            data,
            width: w,
            height: h,
        })
    }
}

/// One `describe` call: options as a JSON object, an optional language and
/// newline-separated keywords.
#[derive(Clone, Copy, Debug, Default)]
pub struct Request<'a> {
    pub options: &'a str,
    pub language: Option<&'a str>,
    pub keywords: Option<&'a str>,
}

/// A `describe` result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Description {
    /// The engine's JSON, unchanged.
    pub json: String,
    /// The token count the engine returned.
    pub tokens: u32,
    /// The output buffer size when this call had to grow it, which means
    /// inference ran more than once.
    pub grew_to: Option<usize>,
}

/// One `transcribe` call: an optional language code (`en`, `de`, `fr`,
/// `es`, `it`, `nl` or `pl`; none detects it), newline-separated keywords to
/// bias decoding towards, and whether to return word timestamps.
#[derive(Clone, Copy, Debug, Default)]
pub struct SpeechRequest<'a> {
    pub language: Option<&'a str>,
    pub keywords: Option<&'a str>,
    pub word_timestamps: bool,
}

/// A `transcribe` result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transcription {
    /// The engine's JSON, unchanged.
    pub json: String,
    /// The token count the engine returned.
    pub tokens: u32,
    /// The output buffer size when this call had to grow it.
    pub grew_to: Option<usize>,
}

/// The engine's one speech stream, owned by whoever opened it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamId(u64);

#[derive(Clone, Copy, Debug)]
struct Stream {
    id: StreamId,
    /// The engine has audio of this stream that `stream_stop` would flush.
    active: bool,
}

struct State {
    vision: Option<String>,
    speech: Option<String>,
    threads: Option<u32>,
    stream: Option<Stream>,
    streams: u64,
    output: Vec<u8>,
}

static ENGINE: Mutex<State> = Mutex::new(State {
    vision: None,
    speech: None,
    threads: None,
    stream: None,
    streams: 0,
    output: Vec::new(),
});

/// Exclusive access to the process-wide engine.
pub struct Engine(MutexGuard<'static, State>);

/// The SHA-256 of `bytes` in lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _infallible = write!(hex, "{byte:02x}");
            hex
        })
}

fn c_string(name: &str, value: &str) -> Result<CString> {
    CString::new(value).map_err(|_err| Error::Input(format!("{name} contains a NUL byte")))
}

fn optional(name: &str, value: Option<&str>) -> Result<Option<CString>> {
    value
        .filter(|value| !value.is_empty())
        .map(|value| c_string(name, value))
        .transpose()
}

fn as_ptr(value: Option<&CString>) -> *const c_char {
    value.map_or(std::ptr::null(), |value| value.as_ptr())
}

/// The size an "output buffer is too small: N bytes needed" error asks for.
fn needed_bytes(detail: &str) -> Option<usize> {
    let (_, rest) = detail.split_once("too small")?;
    rest.split(|c: char| !c.is_ascii_digit())
        .find(|part| !part.is_empty())?
        .parse()
        .ok()
}

/// Text up to the first NUL of `buffer`, if there is one.
fn until_nul(buffer: &[u8]) -> Option<&[u8]> {
    let end = buffer.iter().position(|&byte| byte == 0)?;
    buffer.get(..end)
}

#[expect(unsafe_code, reason = "every engine call goes through this guard")]
impl Engine {
    /// Locks the engine. A panic while it was held does not poison it: every
    /// call leaves the engine in a usable state.
    #[must_use]
    pub fn lock() -> Self {
        Self(ENGINE.lock().unwrap_or_else(PoisonError::into_inner))
    }

    #[must_use]
    pub fn models(&self) -> Models {
        // SAFETY: a plain query; the guard serializes engine calls.
        Models(unsafe { cactus_sys::cactus_models() })
    }

    /// The SHA-256 (hex) of the loaded vision weights.
    #[must_use]
    pub fn vision_weights(&self) -> Option<&str> {
        self.0.vision.as_deref()
    }

    /// The SHA-256 (hex) of the loaded speech weights.
    #[must_use]
    pub fn speech_weights(&self) -> Option<&str> {
        self.0.speech.as_deref()
    }

    /// The last engine error. Read it right after a failing call.
    #[expect(
        clippy::unused_self,
        reason = "taking the guard proves the engine lock is held"
    )]
    fn last_error(&self) -> String {
        // SAFETY: the guard serializes engine calls; the engine owns the
        // returned string until its next call, and it is copied at once.
        let text = unsafe { cactus_sys::cactus_last_error() };
        if text.is_null() {
            return String::new();
        }
        // SAFETY: a non-null result is a NUL-terminated C string.
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .trim()
            .to_owned()
    }

    /// Sets the engine thread count. `0` leaves the engine default and never
    /// calls the engine; the first explicit value wins for the process, and
    /// only before any model is loaded. Values above `i32::MAX` are clamped.
    pub fn set_threads(&mut self, threads: u32) -> Threads {
        if threads == 0 {
            return Threads::EngineDefault;
        }
        let threads = threads.min(u32::try_from(c_int::MAX).unwrap_or(u32::MAX));
        if self.0.threads.is_none() && self.models() != Models::default() {
            return Threads::TooLate { requested: threads };
        }
        match self.0.threads {
            Some(current) if current == threads => Threads::Applied(threads),
            Some(current) => Threads::Ignored {
                requested: threads,
                current,
            },
            None => {
                let value = c_int::try_from(threads).unwrap_or(c_int::MAX);
                // SAFETY: a plain setter; the guard serializes engine calls.
                unsafe { cactus_sys::cactus_set_threads(value) };
                self.0.threads = Some(threads);
                Threads::Applied(threads)
            }
        }
    }

    /// Loads vision weights, identified by their SHA-256. Loading the same
    /// bytes again does nothing. Different vision weights are refused for
    /// the life of the process: the engine has one vision slot and no unload,
    /// and another element may be using the loaded model.
    pub fn load_vision(&mut self, weights: &[u8]) -> Result<()> {
        self.load(Kind::Vision, weights)
    }

    /// Loads speech weights, with the same rules as [`Self::load_vision`]
    /// for the engine's one speech slot.
    pub fn load_speech(&mut self, weights: &[u8]) -> Result<()> {
        self.load(Kind::Speech, weights)
    }

    fn load(&mut self, kind: Kind, weights: &[u8]) -> Result<()> {
        match weights::kind(weights) {
            Some(found) if found == kind => {}
            Some(other) => {
                return Err(Error::Weights(format!(
                    "these are {other} weights, not {kind} weights"
                )));
            }
            None if weights::tag(weights).is_some_and(|tag| tag != weights::GENERATION_TAG) => {
                return Err(Error::Weights(format!(
                    "the weights are from another engine generation; engine {ENGINE_VERSION} needs tag {:#010x}",
                    weights::GENERATION_TAG
                )));
            }
            None => {
                return Err(Error::Weights(format!(
                    "the weights are not a recognized .cact {kind} archive"
                )));
            }
        }
        let identity = sha256_hex(weights);
        let slot = match kind {
            Kind::Vision => &self.0.vision,
            Kind::Speech => &self.0.speech,
            Kind::Text => {
                return Err(Error::Weights("text models are not supported".to_owned()));
            }
        };
        match slot.as_deref() {
            Some(loaded) if loaded == identity => return Ok(()),
            Some(loaded) => {
                return Err(Error::ModelInUse {
                    kind,
                    loaded: loaded.to_owned(),
                    requested: identity,
                });
            }
            None => {}
        }
        let len = u64::try_from(weights.len())
            .map_err(|_err| Error::Input("weights are too large".to_owned()))?;
        // SAFETY: the engine reads `len` bytes from `weights`, which stay
        // borrowed for the call, and copies them; the guard serializes calls.
        let code = unsafe { cactus_sys::cactus_load(weights.as_ptr(), len) };
        if code < 0 {
            return Err(Error::Engine {
                operation: "load",
                code,
                detail: self.last_error(),
            });
        }
        let models = self.models();
        // Text was refused above.
        let (loaded, slot) = if kind == Kind::Speech {
            (models.speech(), &mut self.0.speech)
        } else {
            (models.vision(), &mut self.0.vision)
        };
        if !loaded {
            return Err(Error::Weights(format!(
                "the engine did not load the weights as a {kind} model"
            )));
        }
        *slot = Some(identity);
        Ok(())
    }

    /// Describes an image. The output buffer starts at [`INITIAL_OUTPUT`]
    /// and doubles when the engine reports an overflow; each retry runs
    /// inference again, which [`Description::grew_to`] reports. The grown
    /// buffer is kept for later calls.
    pub fn describe(&mut self, image: PackedRgb<'_>, request: Request<'_>) -> Result<Description> {
        if !self.models().vision() {
            return Err(Error::Weights("no vision model is loaded".to_owned()));
        }
        // An empty request still sends a JSON object.
        let options = if request.options.is_empty() {
            "{}"
        } else {
            request.options
        };
        let options = c_string("options", options)?;
        let language = optional("language", request.language)?;
        let keywords = optional("keywords", request.keywords)?;
        let (json, tokens, grew_to) = self.run("describe", true, |out, capacity| {
            // SAFETY: `image` holds width * height * 3 bytes (checked by
            // `PackedRgb::new`); the strings are NUL-terminated or null; `out`
            // has `capacity` writable bytes. All stay borrowed for the call,
            // and the guard serializes engine calls.
            unsafe {
                cactus_sys::cactus_describe(
                    image.data.as_ptr(),
                    image.width,
                    image.height,
                    options.as_ptr(),
                    as_ptr(language.as_ref()),
                    as_ptr(keywords.as_ref()),
                    out,
                    capacity,
                )
            }
        })?;
        Ok(Description {
            json,
            tokens,
            grew_to,
        })
    }

    /// Transcribes 16 kHz mono PCM. Up to 30 seconds run as one pass; the
    /// engine windows longer audio itself, with word times from the start of
    /// `pcm`. Silence returns empty text and language. The call is stateless,
    /// so several callers can take turns. Output grows as in
    /// [`Self::describe`].
    pub fn transcribe(&mut self, pcm: &[f32], request: SpeechRequest<'_>) -> Result<Transcription> {
        if !self.models().speech() {
            return Err(Error::Weights("no speech model is loaded".to_owned()));
        }
        let samples = c_int::try_from(pcm.len()).map_err(|_err| {
            Error::Input(format!("{} samples are too many for one call", pcm.len()))
        })?;
        let language = optional("language", request.language)?;
        let keywords = optional("keywords", request.keywords)?;
        let timestamps = c_int::from(request.word_timestamps);
        let (json, tokens, grew_to) = self.run("transcribe", true, |out, capacity| {
            // SAFETY: the engine reads `samples` floats from `pcm` (an empty
            // slice still has a non-null pointer); the strings are
            // NUL-terminated or null; `out` has `capacity` writable bytes. All
            // stay borrowed for the call, and the guard serializes calls.
            unsafe {
                cactus_sys::cactus_transcribe(
                    pcm.as_ptr(),
                    samples,
                    as_ptr(language.as_ref()),
                    as_ptr(keywords.as_ref()),
                    timestamps,
                    out,
                    capacity,
                )
            }
        })?;
        Ok(Transcription {
            json,
            tokens,
            grew_to,
        })
    }

    /// Reserves the engine's one speech stream. The engine keeps a single
    /// stream per process; other speech calls, `describe` and loading
    /// weights may still run between its steps.
    pub fn open_stream(&mut self) -> Result<StreamId> {
        if self.0.stream.is_some() {
            return Err(Error::StreamInUse);
        }
        self.0.streams += 1;
        let id = StreamId(self.0.streams);
        self.0.stream = Some(Stream { id, active: false });
        Ok(id)
    }

    fn owned_stream(&mut self, id: StreamId) -> Result<&mut Stream> {
        self.0
            .stream
            .as_mut()
            .filter(|stream| stream.id == id)
            .ok_or(Error::StreamClosed)
    }

    /// Appends PCM (about a second per step works well) to the stream and
    /// returns the engine's JSON: the words two consecutive passes agreed
    /// on, with times from the start of the stream, the unconfirmed
    /// `pending` text, and the seconds `received`. A word can be confirmed
    /// a step or two after a step that reported nothing pending, with times
    /// before that step's `received`. The first step after
    /// [`Self::stream_stop`] starts a new stream at time zero. A step the
    /// engine fails, including an output overflow, which is not retried
    /// because a retry would feed the audio again, ends the stream.
    pub fn stream_process(
        &mut self,
        id: StreamId,
        pcm: &[f32],
        request: SpeechRequest<'_>,
    ) -> Result<Transcription> {
        self.owned_stream(id)?;
        if !self.models().speech() {
            return Err(Error::Weights("no speech model is loaded".to_owned()));
        }
        let samples = c_int::try_from(pcm.len()).map_err(|_err| {
            Error::Input(format!("{} samples are too many for one call", pcm.len()))
        })?;
        let language = optional("language", request.language)?;
        let keywords = optional("keywords", request.keywords)?;
        self.reserve(
            (pcm.len() / 16_000)
                .saturating_mul(STREAM_BYTES_PER_SECOND)
                .max(INITIAL_OUTPUT),
        );
        let result = self.run("stream transcribe", false, |out, capacity| {
            // SAFETY: as in `transcribe`.
            unsafe {
                cactus_sys::cactus_stream_transcribe_process(
                    pcm.as_ptr(),
                    samples,
                    as_ptr(language.as_ref()),
                    as_ptr(keywords.as_ref()),
                    out,
                    capacity,
                )
            }
        });
        match result {
            Ok((json, tokens, grew_to)) => {
                self.owned_stream(id)?.active = true;
                Ok(Transcription {
                    json,
                    tokens,
                    grew_to,
                })
            }
            Err(err) => {
                self.end_stream();
                Err(err)
            }
        }
    }

    /// Ends the stream's current audio and returns the words committed from
    /// its tail, in the JSON form of [`Self::stream_process`]. The stream
    /// stays reserved; the next step starts again at time zero. Stopping a
    /// stream that was never fed returns an empty result. If the engine
    /// fails, the tail is lost, but the stream is still reset.
    pub fn stream_stop(&mut self, id: StreamId) -> Result<Transcription> {
        self.owned_stream(id)?;
        self.reserve(INITIAL_OUTPUT);
        let result = self.run("stream stop", false, |out, capacity| {
            // SAFETY: `out` has `capacity` writable bytes for the call, and
            // the guard serializes engine calls.
            unsafe { cactus_sys::cactus_stream_transcribe_stop(out, capacity) }
        });
        if result.is_err() {
            self.discard_stream_audio();
        }
        self.owned_stream(id)?.active = false;
        let (json, tokens, grew_to) = result?;
        Ok(Transcription {
            json,
            tokens,
            grew_to,
        })
    }

    /// Discards the stream's audio and releases the stream.
    pub fn close_stream(&mut self, id: StreamId) {
        if self.owned_stream(id).is_ok() {
            self.end_stream();
        }
    }

    /// Stops the engine's stream, discarding its tail, and releases it.
    fn end_stream(&mut self) {
        if let Some(stream) = self.0.stream.take()
            && stream.active
        {
            self.discard_stream_audio();
        }
    }

    /// Stops the engine's stream with a one-byte buffer: the engine resets
    /// the stream, and its output (or overflow error) is discarded.
    #[expect(
        clippy::unused_self,
        reason = "taking the guard proves the engine lock is held"
    )]
    fn discard_stream_audio(&mut self) {
        let mut tail = [0u8; 1];
        // SAFETY: `tail` has one writable byte for the call, and the guard
        // serializes engine calls.
        let _code = unsafe {
            cactus_sys::cactus_stream_transcribe_stop(tail.as_mut_ptr().cast::<c_char>(), 1)
        };
    }

    /// Grows the output buffer to at least `bytes`, up to [`MAX_OUTPUT`].
    fn reserve(&mut self, bytes: usize) {
        let bytes = bytes.min(MAX_OUTPUT);
        if self.0.output.len() < bytes {
            self.0.output.resize(bytes, 0);
        }
    }

    /// Runs `call` with the output buffer and returns its NUL-terminated
    /// UTF-8 output, its non-negative result, and the buffer size when it
    /// grew. The engine reports `output buffer is too small: N bytes needed`
    /// only after inference ran; with `retry`, the buffer grows and `call`
    /// runs again, inference included.
    fn run(
        &mut self,
        operation: &'static str,
        retry: bool,
        mut call: impl FnMut(*mut c_char, c_int) -> c_int,
    ) -> Result<(String, u32, Option<usize>)> {
        if self.0.output.is_empty() {
            self.0.output.resize(INITIAL_OUTPUT, 0);
        }
        let mut grew_to = None;
        loop {
            let capacity = c_int::try_from(self.0.output.len()).unwrap_or(c_int::MAX);
            if let Some(first) = self.0.output.first_mut() {
                *first = 0;
            }
            let code = call(self.0.output.as_mut_ptr().cast::<c_char>(), capacity);
            if code >= 0 {
                let text = until_nul(&self.0.output).ok_or_else(|| Error::Engine {
                    operation,
                    code,
                    detail: "the response is not NUL-terminated".to_owned(),
                })?;
                let json = String::from_utf8(text.to_vec()).map_err(|_err| Error::Engine {
                    operation,
                    code,
                    detail: "the response is not UTF-8".to_owned(),
                })?;
                return Ok((json, u32::try_from(code).unwrap_or(0), grew_to));
            }
            let mut detail = self.last_error();
            if detail.is_empty() {
                detail = until_nul(&self.0.output)
                    .map(|text| String::from_utf8_lossy(text).trim().to_owned())
                    .unwrap_or_default();
            }
            let current = self.0.output.len();
            match needed_bytes(&detail) {
                Some(needed) if retry && needed >= current && current < MAX_OUTPUT => {
                    let size = needed
                        .saturating_add(1)
                        .max(current.saturating_mul(2))
                        .min(MAX_OUTPUT);
                    self.0.output.resize(size, 0);
                    grew_to = Some(size);
                }
                _ => {
                    return Err(Error::Engine {
                        operation,
                        code,
                        detail,
                    });
                }
            }
        }
    }

    /// Shrinks the output buffer, so tests can exercise its growth.
    #[doc(hidden)]
    pub fn set_output_capacity_for_tests(&mut self, size: usize) {
        self.0.output = vec![0; size];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_content_hashes() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn parses_the_overflow_message() {
        assert_eq!(
            needed_bytes("output buffer is too small: 3275 bytes needed"),
            Some(3275)
        );
        assert_eq!(needed_bytes("invalid option sections"), None);
    }

    #[test]
    fn reads_text_up_to_the_terminator() {
        assert_eq!(until_nul(b"{}\0junk"), Some(&b"{}"[..]));
        assert_eq!(until_nul(b"{}"), None);
    }

    #[test]
    fn packed_rgb_checks_its_size() {
        let data = [0u8; 12];
        PackedRgb::new(&data, 2, 2).expect("a 2x2 image fits");
        assert!(matches!(PackedRgb::new(&data, 2, 3), Err(Error::Input(_))));
        assert!(matches!(PackedRgb::new(&[], 0, 0), Err(Error::Input(_))));
        assert!(matches!(
            PackedRgb::new(&data, u32::MAX, 1),
            Err(Error::Input(_))
        ));
    }

    #[test]
    fn requests_reject_nul_bytes() {
        assert!(matches!(c_string("options", "{\0}"), Err(Error::Input(_))));
        assert_eq!(optional("language", Some("")), Ok(None));
    }

    #[test]
    fn the_engine_refuses_non_vision_weights_and_describes_nothing_without_them() {
        let mut engine = Engine::lock();
        let speech = weights::tests::archive(13, 3246.0);
        assert!(matches!(
            engine.load_vision(&speech),
            Err(Error::Weights(_))
        ));
        let mut old = weights::tests::archive(36, 4721.0);
        old[0..4].copy_from_slice(&0x05e1_2a83u32.to_le_bytes());
        let error = engine.load_vision(&old).expect_err("old generation");
        assert!(
            error.to_string().contains("another engine generation"),
            "{error}"
        );
        let image = [0u8; 3];
        let image = PackedRgb::new(&image, 1, 1).expect("image");
        assert!(matches!(
            engine.describe(image, Request::default()),
            Err(Error::Weights(_))
        ));
        assert_eq!(engine.vision_weights(), None);
    }

    #[test]
    fn the_engine_refuses_non_speech_weights_and_transcribes_nothing_without_them() {
        let mut engine = Engine::lock();
        let vision = weights::tests::archive(36, 4721.0);
        let error = engine.load_speech(&vision).expect_err("vision weights");
        assert_eq!(
            error.to_string(),
            "these are vision weights, not speech weights"
        );
        assert!(matches!(
            engine.transcribe(&[0.0; 160], SpeechRequest::default()),
            Err(Error::Weights(_))
        ));
        assert_eq!(engine.speech_weights(), None);
    }

    #[test]
    fn the_stream_has_one_owner() {
        let mut engine = Engine::lock();
        let id = engine.open_stream().expect("free");
        assert_eq!(engine.open_stream(), Err(Error::StreamInUse));
        let other = StreamId(u64::MAX);
        assert!(matches!(
            engine.stream_stop(other),
            Err(Error::StreamClosed)
        ));
        engine.close_stream(other);
        assert_eq!(engine.open_stream(), Err(Error::StreamInUse));
        engine.close_stream(id);
        let id = engine.open_stream().expect("free again");
        engine.close_stream(id);
    }

    #[test]
    fn the_first_explicit_thread_count_wins() {
        let mut engine = Engine::lock();
        engine.0.threads = None;
        assert_eq!(engine.set_threads(0), Threads::EngineDefault);
        assert_eq!(engine.set_threads(4), Threads::Applied(4));
        assert_eq!(engine.set_threads(4), Threads::Applied(4));
        assert_eq!(
            engine.set_threads(2),
            Threads::Ignored {
                requested: 2,
                current: 4
            }
        );
        assert_eq!(engine.set_threads(0), Threads::EngineDefault);
    }
}
