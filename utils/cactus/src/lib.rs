//! A safe API over the process-wide Cactus engine.
//!
//! The C ABI has no handle: one engine per process holds every loaded model
//! (text, speech, vision), and its calls are not thread-safe. [`Engine::lock`]
//! returns the only way in, a guard over one process-wide mutex, so vision
//! and speech work is serialized: a 300 ms `describe` stalls a streaming
//! transcription for 300 ms.

use std::ffi::{CStr, CString, c_char, c_int};
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

pub mod weights;

pub use weights::Kind;

/// The engine release this crate links.
pub const ENGINE_VERSION: &str = cactus_sys::ENGINE_VERSION;
/// Initial size of the `describe` output buffer.
pub const INITIAL_OUTPUT: usize = 1 << 20;
/// The largest `describe` output buffer this crate will grow to.
pub const MAX_OUTPUT: usize = 256 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The caller's input cannot be passed to the engine.
    Input(String),
    /// The weights are not what the call needs.
    Weights(String),
    /// Different vision weights are already loaded in this process.
    VisionInUse { loaded: String, requested: String },
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
            Self::VisionInUse { loaded, requested } => write!(
                f,
                "vision weights with SHA-256 {loaded} are loaded, and the process-wide engine cannot replace them with {requested}"
            ),
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

struct State {
    vision: Option<String>,
    threads: Option<u32>,
    output: Vec<u8>,
}

static ENGINE: Mutex<State> = Mutex::new(State {
    vision: None,
    threads: None,
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
        match weights::kind(weights) {
            Some(Kind::Vision) => {}
            Some(other) => {
                return Err(Error::Weights(format!(
                    "these are {other:?} weights, not vision weights"
                )));
            }
            None if weights::tag(weights).is_some_and(|tag| tag != weights::GENERATION_TAG) => {
                return Err(Error::Weights(format!(
                    "the weights are from another engine generation; engine {ENGINE_VERSION} needs tag {:#010x}",
                    weights::GENERATION_TAG
                )));
            }
            None => {
                return Err(Error::Weights(
                    "the weights are not a recognized .cact vision archive".to_owned(),
                ));
            }
        }
        let identity = sha256_hex(weights);
        match self.0.vision.as_deref() {
            Some(loaded) if loaded == identity => return Ok(()),
            Some(loaded) => {
                return Err(Error::VisionInUse {
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
        if !self.models().vision() {
            return Err(Error::Weights(
                "the engine did not load the weights as a vision model".to_owned(),
            ));
        }
        self.0.vision = Some(identity);
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
        if self.0.output.is_empty() {
            self.0.output.resize(INITIAL_OUTPUT, 0);
        }
        let mut grew_to = None;
        loop {
            let capacity = c_int::try_from(self.0.output.len()).unwrap_or(c_int::MAX);
            if let Some(first) = self.0.output.first_mut() {
                *first = 0;
            }
            // SAFETY: `image` holds width * height * 3 bytes (checked by
            // `PackedRgb::new`); the strings are NUL-terminated or null; the
            // output buffer has `capacity` writable bytes. All stay borrowed
            // for the call, and the guard serializes engine calls.
            let code = unsafe {
                cactus_sys::cactus_describe(
                    image.data.as_ptr(),
                    image.width,
                    image.height,
                    options.as_ptr(),
                    as_ptr(language.as_ref()),
                    as_ptr(keywords.as_ref()),
                    self.0.output.as_mut_ptr().cast::<c_char>(),
                    capacity,
                )
            };
            if code >= 0 {
                let text = until_nul(&self.0.output).ok_or_else(|| Error::Engine {
                    operation: "describe",
                    code,
                    detail: "the response is not NUL-terminated".to_owned(),
                })?;
                let json = String::from_utf8(text.to_vec()).map_err(|_err| Error::Engine {
                    operation: "describe",
                    code,
                    detail: "the response is not UTF-8".to_owned(),
                })?;
                return Ok(Description {
                    json,
                    tokens: u32::try_from(code).unwrap_or(0),
                    grew_to,
                });
            }
            let mut detail = self.last_error();
            if detail.is_empty() {
                detail = until_nul(&self.0.output)
                    .map(|text| String::from_utf8_lossy(text).trim().to_owned())
                    .unwrap_or_default();
            }
            let current = self.0.output.len();
            match needed_bytes(&detail) {
                Some(needed) if needed >= current && current < MAX_OUTPUT => {
                    let size = needed
                        .saturating_add(1)
                        .max(current.saturating_mul(2))
                        .min(MAX_OUTPUT);
                    self.0.output.resize(size, 0);
                    grew_to = Some(size);
                }
                _ => {
                    return Err(Error::Engine {
                        operation: "describe",
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
