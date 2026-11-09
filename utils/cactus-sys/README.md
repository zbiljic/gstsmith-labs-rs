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
builds (`DOCS_RS`) embed no engine.

## Independent copies

The engine keeps its models, speech stream and settings in globals. So
that one process can run several engines, `build.rs` links the archive into
a shared library that exports only the `cactus_*` functions, with the
target's C compiler (`CC` and the other variables of the `cc` crate apply),
and the crate embeds its bytes. `Api::load_copy` loads another copy each
time and returns its function pointers:

- on Linux from an anonymous in-memory file (`memfd_create`, asking for an
  executable one as the `vm.memfd_noexec` sysctl requires), so nothing
  touches the disk and a `noexec` temporary directory does not matter.
  Where that fails, or `/proc` is missing, it falls back to the temporary
  file below;
- elsewhere from a file of its own in the temporary directory, removed
  once loaded, because the dynamic loader treats a second load of one file
  as the same library.

Copies are loaded with local symbols and never unloaded. The library has
no Linux `SONAME` on purpose: glibc would treat every copy as the first.
On macOS the copies carry only the linker's ad-hoc signature, so a
hardened-runtime host with library validation refuses them even when the
plugin itself is signed; such a host needs the
`com.apple.security.cs.disable-library-validation` entitlement.

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

An engine copy has no handle, and its calls are not thread-safe: every call
to one copy must be serialized, and `cactus_last_error` read right after a
failure. Different copies can run at the same time. The `cactus` crate
does both. The exported functions
contain no C++ `catch` handlers; the only throws in the archive come from
libc++ (allocation failure, length and range checks, `regex` errors on fixed
patterns). If one ever reached an `extern "C"` call, Rust would abort the
process.

Only the `gstcactus` plugin may depend on this crate, so the engine is
embedded once: `links = "cactus"` stops two different crates with that key,
not two dependents of this one, so `ci/check-cactus-links.sh` (part of
`mise run deps:check`) enforces it.
