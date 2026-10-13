#![expect(
    clippy::expect_used,
    reason = "benchmark setup and synchronous harness transfers must succeed"
)]

use std::sync::Once;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use gst::prelude::*;

fn benchmark_hot_path(criterion: &mut Criterion) {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().expect("initializing GStreamer");
        gstptssampler::plugin_register_static().expect("registering ptssampler");
    });

    let mut group = criterion.benchmark_group("ptssampler");
    group.throughput(Throughput::Elements(1));
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));

    for (name, period) in [
        ("disabled", 0),
        ("keep-every-buffer", 1),
        ("drop-29-of-30", 1_000_000_000),
    ] {
        let mut harness = gst_check::Harness::new_parse(&format!("ptssampler period={period}"));
        harness.set_src_caps_str("application/x-test");
        let mut input = gst::Buffer::new();
        let mut pts = 0;

        group.bench_function(name, |bencher| {
            bencher.iter(|| {
                input
                    .get_mut()
                    .expect("previous output has been released")
                    .set_pts(gst::ClockTime::from_nseconds(pts));
                pts += 33_333_334;
                harness.push(input.clone()).expect("pushing one buffer");
                std::hint::black_box(harness.try_pull());
            });
        });
    }
    group.finish();

    let mut group = criterion.benchmark_group("ptssampler-pull");
    group.throughput(Throughput::Elements(1));
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(2));

    for (name, period) in [("disabled", 0u64), ("keep-every-buffer", 1)] {
        let sampler = gst::ElementFactory::make("ptssampler")
            .property("period", period)
            .build()
            .expect("sampler");
        let sink = sampler.static_pad("sink").expect("sink pad");
        let src = sampler.static_pad("src").expect("source pad");
        let input = atomic_refcell::AtomicRefCell::new(gst::Buffer::new());
        let upstream = gst::Pad::builder(gst::PadDirection::Src)
            .getrange_function(move |_, _, offset, _, _| {
                let mut input = input.borrow_mut();
                input
                    .get_mut()
                    .expect("previous output has been released")
                    .set_pts(gst::ClockTime::from_nseconds(offset));
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
        let mut pts = 0;

        group.bench_function(name, |bencher| {
            bencher.iter(|| {
                std::hint::black_box(src.range(pts, 1).expect("pulling one buffer"));
                pts += 33_333_334;
            });
        });

        src.activate_mode(gst::PadMode::Pull, false)
            .expect("deactivate pull mode");
        upstream.unlink(&sink).expect("unlink source");
    }
    group.finish();
}

criterion_group!(benches, benchmark_hot_path);
criterion_main!(benches);
