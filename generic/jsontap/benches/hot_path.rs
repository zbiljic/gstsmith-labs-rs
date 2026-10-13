#![expect(
    clippy::expect_used,
    reason = "benchmark setup and synchronous harness transfers must succeed"
)]

use std::sync::Once;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use gst::prelude::*;

const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().expect("initializing GStreamer");
        gstjsontap::plugin_register_static().expect("registering jsontap");
        gst::meta::CustomMeta::register("JsonTapBenchmarkMeta", &[]);
    });
}

fn input(with_meta: bool) -> gst::Buffer {
    let mut input = gst::Buffer::new();
    let buffer = input.get_mut().expect("writable buffer");
    buffer.set_pts(gst::ClockTime::from_mseconds(40));
    buffer.set_duration(gst::ClockTime::from_mseconds(33));
    buffer.set_flags(gst::BufferFlags::DISCONT | gst::BufferFlags::DELTA_UNIT);
    if with_meta {
        for index in 0..4u64 {
            let mut meta =
                gst::meta::CustomMeta::add(buffer, "JsonTapBenchmarkMeta").expect("custom meta");
            let fields = meta.mut_structure();
            fields.set("index", index);
            fields.set("confidence", 0.5f64);
            fields.set("name", "quoted \"name\" with a newline\nand Unicode: ž");
            fields.set("ratio", gst::Fraction::new(30, 1));
            fields.set("scores", gst::Array::new([0.1f64, 0.2, 0.3]));
            fields.set(
                "nested",
                gst::Structure::builder("nested")
                    .field("depth", 2i32)
                    .field("tags", gst::List::new(["first", "second"]))
                    .build(),
            );
        }
    }
    input
}

fn benchmark_hot_path(criterion: &mut Criterion) {
    init();
    let dir = tempfile::tempdir().expect("temporary output directory");

    for output in ["null", "file"] {
        let mut group = criterion.benchmark_group(format!("jsontap-{output}"));
        group.throughput(Throughput::Elements(1));
        group.warm_up_time(Duration::from_millis(500));
        group.measurement_time(Duration::from_secs(2));

        for (name, with_meta) in [("plain", false), ("nested-meta", true)] {
            let path = dir.path().join(format!("{name}.jsonl"));
            let location = if output == "null" {
                NULL_DEVICE
            } else {
                path.to_str().expect("UTF-8 temporary path")
            };
            let tap = gst::ElementFactory::make("jsontap")
                .property("location", location)
                .build()
                .expect("tap");
            let mut harness = gst_check::Harness::with_element(&tap, Some("sink"), Some("src"));
            harness.set_src_caps_str("application/x-test");
            let input = input(with_meta);

            group.bench_function(name, |bencher| {
                bencher.iter(|| {
                    harness.push(input.clone()).expect("pushing one buffer");
                    std::hint::black_box(harness.pull().expect("pulling one buffer"));
                });
            });
        }
        group.finish();
    }
}

fn benchmark_pull(criterion: &mut Criterion) {
    init();
    let mut group = criterion.benchmark_group("jsontap-pull");
    group.throughput(Throughput::Elements(1));
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));

    for (name, with_meta) in [("plain", false), ("nested-meta", true)] {
        let tap = gst::ElementFactory::make("jsontap")
            .property("location", NULL_DEVICE)
            .build()
            .expect("tap");
        let sink = tap.static_pad("sink").expect("sink pad");
        let src = tap.static_pad("src").expect("source pad");
        let input = input(with_meta);
        let upstream = gst::Pad::builder(gst::PadDirection::Src)
            .getrange_function(move |_, _, _, _, _| {
                Ok(gst::PadGetRangeSuccess::NewBuffer(input.clone()))
            })
            .build();
        upstream.link(&sink).expect("link source");
        src.activate_mode(gst::PadMode::Pull, true)
            .expect("activate pull mode");
        assert!(sink.send_event(gst::event::StreamStart::new("benchmark")));
        assert!(sink.send_event(gst::event::Caps::new(
            &gst::Caps::builder("application/x-test").build()
        )));
        assert!(
            sink.send_event(gst::event::Segment::new(&gst::FormattedSegment::<
                gst::ClockTime,
            >::new()))
        );

        group.bench_function(name, |bencher| {
            bencher.iter(|| {
                std::hint::black_box(src.range(0, 1).expect("pulling one buffer"));
            });
        });

        src.activate_mode(gst::PadMode::Pull, false)
            .expect("deactivate pull mode");
        upstream.unlink(&sink).expect("unlink source");
    }
    group.finish();
}

criterion_group!(benches, benchmark_hot_path, benchmark_pull);
criterion_main!(benches);
