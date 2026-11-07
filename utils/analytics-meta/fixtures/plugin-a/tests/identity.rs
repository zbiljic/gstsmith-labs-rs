//! Stage-0 identity check: two plugin libraries, each with its own copy of
//! `analytics-meta` (and so its own `x-json` impl static), must agree on one
//! `JsonMtd` type and read each other's documents, even when both touch the
//! type for the first time at once on different threads.
//!
//! Each scenario needs a process in which nothing has resolved the type yet,
//! so the parent test re-runs this binary as a child with libtest's
//! `--exact`/`--nocapture` flags. That assumes plain `cargo test`, not a
//! custom harness or nextest.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context, Result, ensure};
use gst::prelude::*;

const CHILD_ENV: &str = "JSONMTD_IDENTITY_CHILD";
const RUNS: usize = 24;

/// Finds a fixture plugin library. Cargo leaves the metadata hash out of
/// `cdylib` file names of path packages, and builds a dev-dependency's
/// `cdylib` in `deps/` next to this binary (the package's own one is uplifted
/// to the parent directory).
fn plugin_file(stem: &str) -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let deps = exe.parent().context("test binary directory")?;
    let file = format!(
        "{}{stem}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    [Some(deps), deps.parent()]
        .into_iter()
        .flatten()
        .map(|dir| dir.join(&file))
        .find(|path| path.exists())
        .with_context(|| format!("{file} not built next to {}", exe.display()))
}

fn load_plugins() -> Result<(gst::Element, gst::Element)> {
    gst::init()?;
    for stem in ["gstjsonmtdfixturea", "gstjsonmtdfixtureb"] {
        gst::Plugin::load_file(plugin_file(stem)?)?;
    }
    let a = gst::ElementFactory::make("jsonmtdprobea").build()?;
    let b = gst::ElementFactory::make("jsonmtdprobeb").build()?;
    ensure!(
        a.property::<u64>("local-impl") != b.property::<u64>("local-impl"),
        "the libraries share one static, so the test proves nothing"
    );
    Ok((a, b))
}

/// Reads `mtd-type` of both elements at once, first use in each library.
fn race(a: &gst::Element, b: &gst::Element, reverse: bool) -> [u64; 2] {
    // Both threads spin on one flag, so neither waits on a wakeup; the spawn
    // order alternates between runs so each library gets a head start.
    let ready = Arc::new(AtomicUsize::new(0));
    let go = Arc::new(AtomicBool::new(false));
    let mut order = [(0, a.clone()), (1, b.clone())];
    if reverse {
        order.reverse();
    }
    let threads = order.map(|(slot, element)| {
        let (ready, go) = (Arc::clone(&ready), Arc::clone(&go));
        (
            slot,
            std::thread::spawn(move || {
                ready.fetch_add(1, Ordering::AcqRel);
                while !go.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                element.property::<u64>("mtd-type")
            }),
        )
    });
    while ready.load(Ordering::Acquire) < 2 {
        std::hint::spin_loop();
    }
    go.store(true, Ordering::Release);
    let mut types = [0u64; 2];
    for (slot, thread) in threads {
        if let Some(value) = types.get_mut(slot) {
            *value = thread.join().unwrap_or(0);
        }
    }
    types
}

/// Pushes one buffer through `first ! second`, each writing a document, and
/// returns what the second plugin saw.
fn exchange(first: &str, second: &str) -> Result<(String, gst::Buffer)> {
    let mut harness = gst_check::Harness::new_parse(&format!(
        "jsonmtdprobe{first} write-schema=x-from-{first} ! jsonmtdprobe{second} write-schema=x-from-{second}"
    ));
    harness.set_src_caps_str("application/x-test");
    let output = harness
        .push_and_pull(gst::Buffer::new())
        .map_err(|err| anyhow::anyhow!("push_and_pull: {err:?}"))?;
    let reader = harness
        .find_element(&format!("jsonmtdprobe{second}"))
        .context("second probe in harness")?;
    Ok((reader.property::<String>("seen"), output))
}

fn race_child(run: usize) -> Result<String> {
    let (a, b) = load_plugins()?;
    let (local_a, local_b) = (
        a.property::<u64>("local-impl"),
        b.property::<u64>("local-impl"),
    );
    let [type_a, type_b] = race(&a, &b, run % 2 == 1);
    ensure!(
        type_a != 0 && type_a == type_b,
        "types differ: {type_a:#x} vs {type_b:#x}"
    );
    ensure!(
        type_a == local_a || type_a == local_b,
        "winner is neither library's impl"
    );

    // A third copy, linked into this test binary, joins the same type.
    ensure!(
        <analytics_meta::JsonMtd as gst_analytics::AnalyticsMtd>::mtd_type() as u64 == type_a,
        "the test binary resolved a different type"
    );

    for (first, second) in [("a", "b"), ("b", "a")] {
        let (seen, output) = exchange(first, second)?;
        let expected = format!(
            r#"x-from-{first}={{"from":"jsonmtdprobe{first}"}};x-from-{second}={{"from":"jsonmtdprobe{second}"}}"#
        );
        ensure!(seen == expected, "plugin {second} saw {seen:?}");
        let meta = output
            .meta::<gst_analytics::AnalyticsRelationMeta>()
            .context("relation meta")?;
        let schemas: Vec<_> = analytics_meta::json::documents(&meta)
            .map(|doc| doc.schema)
            .collect();
        ensure!(
            schemas == [format!("x-from-{first}"), format!("x-from-{second}")],
            "test binary saw {schemas:?}"
        );
    }
    Ok(if type_a == local_a { "a" } else { "b" }.to_owned())
}

/// A foreign library already installed an `x-json` impl: both adopt it.
fn seeded_same_child() -> Result<String> {
    gst::init()?;
    let seeded =
        analytics_meta::json::seed_slot_for_tests(c"x-json").context("slot already set")? as u64;
    let (a, b) = load_plugins()?;
    let types = race(&a, &b, false);
    ensure!(types == [seeded; 2], "types {types:x?}, seeded {seeded:#x}");
    let (seen, _) = exchange("a", "b")?;
    ensure!(seen.contains("x-from-a="), "b did not read a: {seen:?}");
    Ok("seeded".to_owned())
}

/// The slot holds an impl with another name: each library stays local.
fn seeded_other_child() -> Result<String> {
    gst::init()?;
    analytics_meta::json::seed_slot_for_tests(c"x-not-json").context("slot already set")?;
    let (a, b) = load_plugins()?;
    let [type_a, type_b] = race(&a, &b, false);
    ensure!(
        type_a == a.property::<u64>("local-impl") && type_b == b.property::<u64>("local-impl"),
        "a foreign impl was adopted"
    );
    let (seen, _) = exchange("a", "b")?;
    ensure!(
        seen == r#"x-from-b={"from":"jsonmtdprobeb"}"#,
        "documents crossed: {seen:?}"
    );
    Ok("local".to_owned())
}

/// Runs this test function again in a fresh process with `scenario`.
fn run_child(test: &str, scenario: &str) -> Result<String> {
    let output = Command::new(std::env::current_exe()?)
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, scenario)
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    ensure!(
        output.status.success(),
        "{scenario} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // libtest prints the test name on the same line as the child's output.
    stdout
        .lines()
        .find_map(|line| line.split_once("result=").map(|(_, rest)| rest))
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_owned)
        .with_context(|| format!("{scenario} reported nothing:\n{stdout}"))
}

/// In a child, runs the scenario and prints its result; returns `true` then.
fn child(run: impl FnOnce(&str) -> Result<String>) -> Result<bool> {
    let Ok(scenario) = std::env::var(CHILD_ENV) else {
        return Ok(false);
    };
    println!("result={}", run(&scenario)?);
    Ok(true)
}

#[test]
fn json_mtd_identity_is_shared_across_plugin_libraries() -> Result<()> {
    if child(|scenario| race_child(scenario.parse()?))? {
        return Ok(());
    }
    let mut winners = Vec::new();
    for run in 0..RUNS {
        winners.push(run_child(
            "json_mtd_identity_is_shared_across_plugin_libraries",
            &run.to_string(),
        )?);
    }
    println!(
        "first-use race winners over {RUNS} runs: a={}, b={}",
        winners.iter().filter(|w| *w == "a").count(),
        winners.iter().filter(|w| *w == "b").count()
    );
    Ok(())
}

#[test]
fn a_foreign_x_json_impl_in_the_slot_is_adopted() -> Result<()> {
    if child(|_| seeded_same_child())? {
        return Ok(());
    }
    run_child("a_foreign_x_json_impl_in_the_slot_is_adopted", "seeded").map(drop)
}

#[test]
fn a_wrongly_named_impl_in_the_slot_is_ignored() -> Result<()> {
    if child(|_| seeded_other_child())? {
        return Ok(());
    }
    run_child("a_wrongly_named_impl_in_the_slot_is_ignored", "other").map(drop)
}
