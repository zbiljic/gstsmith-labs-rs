//! Raw FFI to the Cactus engine, declared by hand from the pinned `cactus.h`.
//!
//! The engine has no handle: one process-wide engine holds every loaded
//! model, and its functions are not thread-safe. Callers serialize all calls
//! behind one lock (the `cactus` crate does). `cactus_last_error` describes
//! the most recent failure and must be read before any other call.
//!
//! Entry points do not catch C++ exceptions, and only allocation failure or
//! an engine bug can raise one; Rust aborts the process if a foreign
//! exception reaches an `extern "C"` call.

#![no_std]

use core::ffi::{c_char, c_float, c_int, c_uchar, c_ulonglong};

/// The engine release these declarations were written against, pinned in
/// `build.rs`.
pub const ENGINE_VERSION: &str = env!("CACTUS_ENGINE_VERSION");
/// SHA-256 of the `cactus.h` these declarations were written from. When an
/// engine bump changes the header, a test fails until the declarations are
/// re-read and this value is updated.
pub const HEADER_SHA256: &str = "9be7eb25b1d89180495d04a0d2c27053f282dac85add50cacc48d73302819d9d";

/// `cactus_models()` bits.
pub const CACTUS_TEXT: c_int = 1;
pub const CACTUS_SPEECH: c_int = 2;
pub const CACTUS_VISION: c_int = 4;

#[expect(unsafe_code, reason = "the declarations of the engine's C ABI")]
unsafe extern "C" {
    /// Loads a `.cact` archive of `n` bytes; the engine copies it. Returns a
    /// negative value on failure.
    pub fn cactus_load(cact: *const c_uchar, n: c_ulonglong) -> c_int;
    /// The bitset of loaded models (`CACTUS_TEXT`, `CACTUS_SPEECH`, `CACTUS_VISION`).
    pub fn cactus_models() -> c_int;
    /// The last error as a NUL-terminated string owned by the engine.
    pub fn cactus_last_error() -> *const c_char;
    pub fn cactus_init(
        system_prompt: *const c_char,
        tools_json: *const c_char,
        tool_index_path: *const c_char,
    ) -> c_int;
    pub fn cactus_complete(
        input: *const c_char,
        pcm: *const c_float,
        samples: c_int,
        max_new_tokens: c_int,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;
    pub fn cactus_set_audio(
        language: *const c_char,
        keywords: *const c_char,
        word_timestamps: c_int,
    );
    /// Sets the thread count for models loaded afterwards.
    pub fn cactus_set_threads(threads: c_int);
    pub fn cactus_reset();
    pub fn cactus_transcribe(
        pcm: *const c_float,
        samples: c_int,
        language: *const c_char,
        keywords: *const c_char,
        word_timestamps: c_int,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;
    pub fn cactus_stream_transcribe_process(
        pcm: *const c_float,
        samples: c_int,
        language: *const c_char,
        keywords: *const c_char,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;
    pub fn cactus_stream_transcribe_stop(out: *mut c_char, out_capacity: c_int) -> c_int;
    /// Describes a tightly packed RGB24 image of `width` × `height`. Writes
    /// NUL-terminated JSON to `out` and returns a token count, or a negative
    /// value on failure (including an `out` that is too small, reported only
    /// after inference ran).
    pub fn cactus_describe(
        rgb: *const c_uchar,
        width: c_int,
        height: c_int,
        options: *const c_char,
        language: *const c_char,
        keywords: *const c_char,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;
    pub fn cactus_embed(
        input: *const c_char,
        pcm: *const c_float,
        samples: c_int,
        out: *mut c_float,
        out_capacity: c_int,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    #[test]
    fn declarations_match_the_linked_header() {
        let linked = env!("CACTUS_HEADER_SHA256");
        if linked != "unchecked" {
            assert_eq!(
                linked,
                super::HEADER_SHA256,
                "cactus.h changed: re-read the declarations and update HEADER_SHA256"
            );
        }
    }

    #[test]
    #[expect(unsafe_code, reason = "calls the engine to prove it links")]
    fn the_engine_links_and_starts_empty() {
        // SAFETY: a plain query of the process-wide engine, with no models.
        let models = unsafe { super::cactus_models() };
        assert_eq!(models, 0);
    }
}
