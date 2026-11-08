//! Links the pinned Cactus engine archive.
//!
//! With `CACTUS_LIB_DIR` set, `libcactus.a` (and, if present, `cactus.h`)
//! are taken from that directory. Otherwise the `download` feature fetches
//! both for the target from the pinned Hugging Face revision and checks them
//! against SHA-256 values copied from the release `manifest.json`.
//!
//! The engine's C++ runtime is LLVM libc++ (`std::__1`). macOS links the
//! system `libc++.dylib`. Linux links `libc++.a` and `libc++abi.a`
//! statically, and they stay hidden in the plugin library, so a process that
//! also loads libstdc++ plugins never sees two C++ runtimes. At run time
//! only glibc and `libgcc_s` are needed; the archives reference only
//! long-standing glibc symbols (`__isoc99_*`, no `__isoc23_*`), so the glibc
//! minimum is set by the build host's libc++. The macOS archive is built for
//! macOS 27.2 (`LC_BUILD_VERSION`); older releases are untested.

use std::env;
use std::error::Error;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

const VERSION: &str = "4.1.0";
#[cfg(feature = "download")]
const REVISION: &str = "4e822c9036be2eb0e47ef556df32a818bb1572ef";
#[cfg(feature = "download")]
const BASE_URL: &str = "https://huggingface.co/Cactus-Compute/cactus/resolve";
/// `cactus.h` is identical on every platform.
#[cfg(feature = "download")]
const HEADER_SHA256: &str = "9be7eb25b1d89180495d04a0d2c27053f282dac85add50cacc48d73302819d9d";

/// Supported targets: (`target_os`, `target_arch`, release platform, archive SHA-256).
const ARCHIVES: &[(&str, &str, &str, &str)] = &[
    (
        "macos",
        "aarch64",
        "macos-arm64",
        "4105ce155432e60f1337f158074059c96d5b8907549051ef869a0b0414f0c3ae",
    ),
    (
        "linux",
        "aarch64",
        "linux-arm64",
        "cd020ac6b125f80cf16f9ee25d9d606402369612b77e5fa2845dc85d7aaebcbc",
    ),
    (
        "linux",
        "x86_64",
        "linux-x86_64",
        "0c58951e0e4353a2a302621d714e19a3069b11c9f3e06b1409457f571cc900ec",
    ),
];

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        hasher.update(chunk.get(..read).unwrap_or_default());
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _infallible = write!(hex, "{byte:02x}");
            hex
        }))
}

/// Upper bound for one downloaded file; the archives are a few MiB.
#[cfg(feature = "download")]
const MAX_DOWNLOAD: u64 = 512 << 20;

#[cfg(feature = "download")]
fn download(agent: &ureq::Agent, url: &str, path: &Path) -> Result<()> {
    let mut response = agent.get(url).call()?;
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .reader();
    let mut file = fs::File::create(path)?;
    std::io::copy(&mut reader, &mut file)?;
    Ok(())
}

/// Downloads `url` to `path` unless a file with `expected` SHA-256 is there.
/// Redirects are followed and proxy environment variables honoured.
#[cfg(feature = "download")]
fn fetch(url: &str, path: &Path, expected: &str) -> Result<()> {
    use std::time::Duration;

    if path.exists() && sha256(path)? == expected {
        return Ok(());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_global(Some(Duration::from_secs(600)))
        .build()
        .into();
    let partial = path.with_extension("partial");
    let mut attempt = 1;
    while let Err(err) = download(&agent, url, &partial) {
        if attempt == 3 {
            let _ignored = fs::remove_file(&partial);
            return Err(format!("fetching {url}: {err}").into());
        }
        println!("cargo:warning=fetching {url} failed ({err}); retrying");
        attempt += 1;
    }
    let actual = sha256(&partial)?;
    if actual != expected {
        let _ignored = fs::remove_file(&partial);
        return Err(format!("{url} has SHA-256 {actual}, expected {expected}").into());
    }
    fs::rename(&partial, path)?;
    Ok(())
}

/// The directory holding `libcactus.a`, and the SHA-256 of the header found
/// with it, if any.
fn engine_dir(platform: &str, archive_sha256: &str) -> Result<(PathBuf, Option<String>)> {
    if let Some(dir) = env::var_os("CACTUS_LIB_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.join("libcactus.a").is_file() {
            return Err(format!("CACTUS_LIB_DIR={} has no libcactus.a", dir.display()).into());
        }
        println!(
            "cargo:rerun-if-changed={}",
            dir.join("libcactus.a").display()
        );
        let header = dir.join("cactus.h");
        println!("cargo:rerun-if-changed={}", header.display());
        let header_sha = if header.is_file() {
            Some(sha256(&header)?)
        } else {
            None
        };
        return Ok((dir, header_sha));
    }
    download_engine(platform, archive_sha256)
}

#[cfg(not(feature = "download"))]
fn download_engine(_platform: &str, _archive_sha256: &str) -> Result<(PathBuf, Option<String>)> {
    Err("set CACTUS_LIB_DIR to a directory with libcactus.a, or enable the download feature".into())
}

#[cfg(feature = "download")]
fn download_engine(platform: &str, archive_sha256: &str) -> Result<(PathBuf, Option<String>)> {
    let dir = PathBuf::from(env::var("OUT_DIR")?)
        .join(format!("cactus-{VERSION}"))
        .join(platform);
    fs::create_dir_all(&dir)?;
    let base = format!("{BASE_URL}/{REVISION}/releases/{VERSION}/{platform}");
    fetch(
        &format!("{base}/libcactus.a"),
        &dir.join("libcactus.a"),
        archive_sha256,
    )?;
    fetch(
        &format!("{base}/cactus.h"),
        &dir.join("cactus.h"),
        HEADER_SHA256,
    )?;
    Ok((dir, Some(HEADER_SHA256.to_owned())))
}

fn llvm_version(dir: &Path) -> u32 {
    dir.parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("llvm-"))
        .and_then(|version| version.parse().ok())
        .unwrap_or(0)
}

/// Finds the directory holding static `libc++.a` and `libc++abi.a` from
/// LLVM `min_version` or newer, preferring the newest.
fn libcxx_dir(min_version: u32) -> Result<PathBuf> {
    if let Some(dir) = env::var_os("CACTUS_LIBCXX_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let mut candidates: Vec<PathBuf> = fs::read_dir("/usr/lib")
        .map(|entries| {
            entries
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path().join("lib"))
                .filter(|dir| llvm_version(dir) > 0)
                .filter(|dir| dir.join("libc++.a").is_file() && dir.join("libc++abi.a").is_file())
                .collect()
        })
        .unwrap_or_default();
    candidates.sort_by_key(|dir| std::cmp::Reverse(llvm_version(dir)));
    match candidates.first() {
        Some(dir) if llvm_version(dir) >= min_version => Ok(dir.clone()),
        Some(dir) => Err(format!(
            "{} has libc++ {}, but this engine needs {min_version} or newer: install libc++-{min_version}-dev and libc++abi-{min_version}-dev, or set CACTUS_LIBCXX_DIR",
            dir.display(),
            llvm_version(dir)
        )
        .into()),
        None => Err(
            "no static libc++ found: install libc++-dev and libc++abi-dev, or set CACTUS_LIBCXX_DIR"
                .into(),
        ),
    }
}

fn main() -> Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CACTUS_LIB_DIR");
    println!("cargo:rerun-if-env-changed=CACTUS_LIBCXX_DIR");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    println!("cargo:rustc-env=CACTUS_ENGINE_VERSION={VERSION}");
    // Documentation builds skip linking. This is not a feature, because the
    // workspace gates build with --all-features and must link the engine.
    if env::var_os("DOCS_RS").is_some() {
        println!("cargo:rustc-env=CACTUS_HEADER_SHA256=unchecked");
        return Ok(());
    }
    let os = env::var("CARGO_CFG_TARGET_OS")?;
    let arch = env::var("CARGO_CFG_TARGET_ARCH")?;
    let env_name = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let Some(&(_, _, platform, archive_sha256)) = ARCHIVES
        .iter()
        .find(|(o, a, _, _)| *o == os && *a == arch && (os != "linux" || env_name == "gnu"))
    else {
        return Err(
            format!("Cactus {VERSION} has no static engine for {arch}-{os}-{env_name}").into(),
        );
    };
    let (dir, header_sha) = engine_dir(platform, archive_sha256)?;
    println!(
        "cargo:rustc-env=CACTUS_HEADER_SHA256={}",
        header_sha.as_deref().unwrap_or("unchecked")
    );
    println!("cargo:root={}", dir.display());
    println!("cargo:include={}", dir.display());
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=cactus");
    if os == "macos" {
        println!("cargo:rustc-link-lib=dylib=c++");
    } else {
        // The x86_64 archive calls std::__1::__hash_memory, which libc++
        // exports only from LLVM 21 on; the arm64 archive links with 18.
        let libcxx = libcxx_dir(if arch == "x86_64" { 21 } else { 18 })?;
        println!("cargo:rustc-link-search=native={}", libcxx.display());
        println!("cargo:rustc-link-lib=static=c++");
        println!("cargo:rustc-link-lib=static=c++abi");
    }
    Ok(())
}
