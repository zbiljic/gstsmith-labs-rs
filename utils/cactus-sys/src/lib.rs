//! Raw FFI to the Cactus engine, declared by hand from the pinned `cactus.h`.
//!
//! The engine has no handle: it keeps its models, speech stream and settings
//! in globals, and its functions are not thread-safe. This crate embeds the
//! engine as a shared library, and [`Api::load_copy`] loads another copy of
//! it each time. Every copy is an independent engine: calls to one copy must
//! be serialized (the `cactus` crate does), while different copies can run
//! at the same time. `cactus_last_error` describes the most recent failure
//! of its copy and must be read before any other call to that copy. Copies
//! are never unloaded.
//!
//! Entry points do not catch C++ exceptions, and only allocation failure or
//! an engine bug can raise one; Rust aborts the process if a foreign
//! exception reaches an `extern "C"` call.

use std::ffi::{CStr, c_char, c_float, c_int, c_uchar, c_ulonglong, c_void};

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

/// The engine library `build.rs` linked from the pinned archive. It is empty
/// in documentation builds.
static ENGINE_LIBRARY: &[u8] = include_bytes!(env!("CACTUS_ENGINE_LIBRARY"));

macro_rules! engine_api {
    ($( $(#[$doc:meta])* fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?; )*) => {
        /// The functions of one engine copy.
        #[derive(Clone, Copy, Debug)]
        pub struct Api {
            $( $(#[$doc])* pub $name: unsafe extern "C" fn($($ty),*) $(-> $ret)?, )*
        }

        impl Api {
            /// Resolves every function in `handle`.
            ///
            /// # Safety
            ///
            /// `handle` is a loaded copy of the engine library.
            #[expect(unsafe_code, reason = "resolves the engine's functions")]
            unsafe fn resolve(handle: *mut c_void) -> Result<Self, String> {
                Ok(Self {
                    $( $name: {
                        let name = concat!(stringify!($name), "\0");
                        // SAFETY: `handle` is a live library handle and
                        // `name` is NUL-terminated.
                        let symbol = unsafe { libc::dlsym(handle, name.as_ptr().cast()) };
                        if symbol.is_null() {
                            return Err(format!(
                                "the engine library has no {}",
                                stringify!($name)
                            ));
                        }
                        // SAFETY: the engine exports this function with the
                        // signature declared from `cactus.h`.
                        unsafe {
                            std::mem::transmute::<
                                *mut c_void,
                                unsafe extern "C" fn($($ty),*) $(-> $ret)?,
                            >(symbol)
                        }
                    }, )*
                })
            }
        }
    };
}

engine_api! {
    /// Loads a `.cact` archive of `n` bytes; the engine copies it. Returns a
    /// negative value on failure.
    fn cactus_load(cact: *const c_uchar, n: c_ulonglong) -> c_int;
    /// The bitset of loaded models (`CACTUS_TEXT`, `CACTUS_SPEECH`, `CACTUS_VISION`).
    fn cactus_models() -> c_int;
    /// The last error as a NUL-terminated string owned by the engine.
    fn cactus_last_error() -> *const c_char;
    fn cactus_init(
        system_prompt: *const c_char,
        tools_json: *const c_char,
        tool_index_path: *const c_char,
    ) -> c_int;
    fn cactus_complete(
        input: *const c_char,
        pcm: *const c_float,
        samples: c_int,
        max_new_tokens: c_int,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;
    fn cactus_set_audio(
        language: *const c_char,
        keywords: *const c_char,
        word_timestamps: c_int,
    );
    /// Sets the thread count for models loaded afterwards.
    fn cactus_set_threads(threads: c_int);
    fn cactus_reset();
    fn cactus_transcribe(
        pcm: *const c_float,
        samples: c_int,
        language: *const c_char,
        keywords: *const c_char,
        word_timestamps: c_int,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;
    fn cactus_stream_transcribe_process(
        pcm: *const c_float,
        samples: c_int,
        language: *const c_char,
        keywords: *const c_char,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;
    fn cactus_stream_transcribe_stop(out: *mut c_char, out_capacity: c_int) -> c_int;
    /// Describes a tightly packed RGB24 image of `width` × `height`. Writes
    /// NUL-terminated JSON to `out` and returns a token count, or a negative
    /// value on failure (including an `out` that is too small, reported only
    /// after inference ran).
    fn cactus_describe(
        rgb: *const c_uchar,
        width: c_int,
        height: c_int,
        options: *const c_char,
        language: *const c_char,
        keywords: *const c_char,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;
    fn cactus_embed(
        input: *const c_char,
        pcm: *const c_float,
        samples: c_int,
        out: *mut c_float,
        out_capacity: c_int,
    ) -> c_int;
}

impl Api {
    /// Loads another copy of the engine, with no models and default
    /// settings. The copy stays loaded for the life of the process.
    pub fn load_copy() -> Result<Self, String> {
        if ENGINE_LIBRARY.is_empty() {
            return Err("this build embeds no engine".to_owned());
        }
        let handle = open_copy()?;
        // SAFETY: `handle` is a freshly loaded copy of the engine library.
        #[expect(unsafe_code, reason = "resolves the engine's functions")]
        let api = unsafe { Self::resolve(handle) };
        if api.is_err() {
            // The embedded library exports exactly these names, so this does
            // not happen; if it did, retries would otherwise leak a copy each.
            // SAFETY: `handle` is loaded, and nothing from it is in use.
            #[expect(unsafe_code, reason = "unloads a copy that is not in use")]
            let _code = unsafe { libc::dlclose(handle) };
        }
        api
    }
}

/// The text of the last `dlopen` failure on this thread.
#[expect(unsafe_code, reason = "reads the loader's error")]
fn dlerror() -> String {
    // SAFETY: `dlerror` returns null or a NUL-terminated string that stays
    // valid until the next loader call on this thread; it is copied at once.
    let text = unsafe { libc::dlerror() };
    if text.is_null() {
        return "unknown error".to_owned();
    }
    // SAFETY: non-null, NUL-terminated (above).
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

/// Loads `path` with its own symbols, apart from every other copy.
#[expect(unsafe_code, reason = "loads the engine library")]
fn dlopen(path: &std::path::Path) -> Result<*mut c_void, String> {
    use std::os::unix::ffi::OsStrExt as _;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_err| format!("{} contains a NUL byte", path.display()))?;
    // SAFETY: `c_path` is NUL-terminated. The library runs no code at load
    // beyond C++ static initializers of the engine.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(format!("loading the engine library: {}", dlerror()));
    }
    Ok(handle)
}

/// Loads a copy from memory, or from a temporary file where `memfd_create`
/// or `/proc` is unavailable.
#[cfg(target_os = "linux")]
fn open_copy() -> Result<*mut c_void, String> {
    open_in_memory()
        .or_else(|memory| open_from_temp_file().map_err(|file| format!("{memory}; then {file}")))
}

#[cfg(not(target_os = "linux"))]
fn open_copy() -> Result<*mut c_void, String> {
    open_from_temp_file()
}

/// Loads the embedded library from an anonymous in-memory file, so nothing
/// is written to disk and a `noexec` temporary directory does not matter.
#[cfg(target_os = "linux")]
#[expect(unsafe_code, reason = "creates an in-memory file for the engine")]
fn open_in_memory() -> Result<*mut c_void, String> {
    use std::io::Write as _;
    use std::os::fd::{FromRawFd as _, IntoRawFd as _};

    let create = |flags| {
        // SAFETY: the name is NUL-terminated.
        unsafe { libc::memfd_create(c"cactus-engine".as_ptr(), flags) }
    };
    // With the `vm.memfd_noexec` sysctl, a memfd must ask to be executable;
    // kernels before 6.3 refuse that flag with EINVAL.
    let mut fd = create(libc::MFD_CLOEXEC | libc::MFD_EXEC);
    if fd < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
        fd = create(libc::MFD_CLOEXEC);
    }
    if fd < 0 {
        return Err(format!(
            "creating an in-memory file for the engine: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `fd` is a new descriptor that nothing else owns.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(ENGINE_LIBRARY)
        .map_err(|err| format!("writing the engine library: {err}"))?;
    let handle = dlopen(std::path::Path::new(&format!("/proc/self/fd/{fd}")))?;
    // The descriptor stays open for the life of the process: glibc knows a
    // loaded library by its path, and a later copy given the same descriptor
    // number would get this copy back.
    let _kept = file.into_raw_fd();
    Ok(handle)
}

/// Loads the embedded library from a file of its own in the temporary
/// directory, removed once loaded: the dynamic loader treats a second load
/// of one file as the same library. The mapping keeps the removed file's
/// inode, so a later copy never gets the same one.
fn open_from_temp_file() -> Result<*mut c_void, String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COPIES: AtomicU64 = AtomicU64::new(0);
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let dir = std::env::temp_dir();
    let (path, mut file) = loop {
        let path = dir.join(format!(
            "cactus-engine-{}-{}.{extension}",
            std::process::id(),
            COPIES.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => break (path, file),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(format!("creating {}: {err}", path.display())),
        }
    };
    let loaded = file
        .write_all(ENGINE_LIBRARY)
        .map_err(|err| format!("writing {}: {err}", path.display()))
        .and_then(|()| {
            drop(file);
            dlopen(&path)
        });
    let _ignored = std::fs::remove_file(&path);
    loaded
}

#[cfg(test)]
mod tests {
    use super::Api;

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
    #[expect(unsafe_code, reason = "calls an engine copy")]
    fn a_copy_also_loads_from_a_temporary_file() {
        let handle = super::open_from_temp_file().expect("loaded");
        // SAFETY: a freshly loaded copy of the engine library.
        let copy = unsafe { Api::resolve(handle) }.expect("resolved");
        // SAFETY: a plain query of an engine with no models.
        assert_eq!(unsafe { (copy.cactus_models)() }, 0);
    }

    #[test]
    #[expect(unsafe_code, reason = "calls two engine copies")]
    fn every_copy_is_a_new_engine() {
        let first = Api::load_copy().expect("first copy");
        let second = Api::load_copy().expect("second copy");
        assert_ne!(
            first.cactus_models as usize, second.cactus_models as usize,
            "each copy has its own code and globals"
        );
        for copy in [first, second] {
            // SAFETY: a plain query of an engine with no models, called from
            // this thread only.
            assert_eq!(unsafe { (copy.cactus_models)() }, 0);
        }
    }
}
