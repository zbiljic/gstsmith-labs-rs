# cactus-sys

Raw FFI to the [Cactus](https://cactuscompute.com) 4.1.0 engine, which runs
Mirage (vision), Whistle (speech) and Needle (text) models. The 13 functions
of `cactus.h` are declared by hand; no bindgen or libclang is needed.

## Where the engine comes from

The 4.x runtime is Apache-2.0, but its source is not public: it ships only as
prebuilt per-platform `libcactus.a` and `cactus.h` files with a
`manifest.json` of SHA-256 values, in the Hugging Face model repository
`Cactus-Compute/cactus`. This crate is a deliberate, documented exception to
the workspace rule against opaque prebuilt libraries:

- the binaries are Apache-2.0;
- they are pinned by revision and SHA-256 in `build.rs`;
- they are fetched at build time, and nothing is committed.

`build.rs` downloads the target's `libcactus.a` and `cactus.h` into
`OUT_DIR` and checks both against SHA-256 values copied from the manifest
(the default `download` feature). Set `CACTUS_LIB_DIR` to a directory
holding `libcactus.a` (and `cactus.h`) to build offline. Documentation
builds (`DOCS_RS`) skip linking.

**Header pin.** `HEADER_SHA256` records the SHA-256 of the `cactus.h` the
declarations were written from. On an engine bump the download fails until
`build.rs` carries the new manifest SHA; then a test fails until the
declarations are re-read and `HEADER_SHA256` matches. With `CACTUS_LIB_DIR`,
the test checks the header found there.

## Platforms

| Rust target | Status | C++ runtime |
| --- | --- | --- |
| `aarch64-apple-darwin` | verified | system libc++ |
| `aarch64-unknown-linux-gnu` | verified | static LLVM libc++ 18 or newer |
| `x86_64-unknown-linux-gnu` | verified (emulated) | static LLVM libc++ 21 or newer |

Other targets (Windows, Intel macOS, musl) are not supported. On Linux,
the build needs the static `libc++.a` and `libc++abi.a`: `build.rs` takes
the newest under `/usr/lib/llvm-*/lib`, or the directory in
`CACTUS_LIBCXX_DIR`.

## Safety

The engine is one process-wide object with no handle, and its calls are not
thread-safe: every call must be serialized, and `cactus_last_error` read
right after a failure. The `cactus` crate does both. The exported functions
contain no C++ `catch` handlers; the only throws in the archive come from
libc++ (allocation failure, length and range checks, `regex` errors on fixed
patterns). If one ever reached an `extern "C"` call, Rust would abort the
process.

Only the `gstcactus` plugin may link this crate: `links = "cactus"` stops two
different crates with that key, not two dependents of this one, so
`ci/check-cactus-links.sh` (part of `mise run deps:check`) enforces it.
