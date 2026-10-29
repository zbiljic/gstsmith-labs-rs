#![expect(
    clippy::expect_used,
    reason = "benchmark setup and synchronous harness transfers must succeed"
)]

use std::sync::Once;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

const SOURCE: &str = "video/x-raw,format=RGBA,width=1920,height=1080,framerate=30/1";

fn benchmark_hot_path(criterion: &mut Criterion) {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().expect("initializing GStreamer");
        gstwallwarp::plugin_register_static().expect("registering wallwarp");
    });

    let mut group = criterion.benchmark_group("wallwarp-1080p-source");
    group.throughput(Throughput::Elements(1));
    group.warm_up_time(Duration::from_millis(500));
    group.measurement_time(Duration::from_secs(3));
    group.sample_size(20);

    // A perspective screen, a rotated full-resolution screen, and the
    // identity passthrough as a baseline.
    let perspective = "quad=0.0417,0.0926,0.4063,0.0833,0.4167,0.4630,0.0469,0.4815 \
                       output-width=1280 output-height=720";
    let rotated = "quad=0.7135,0.0556,0.6927,0.8519,0.5521,0.8333,0.5729,0.0278 \
                   output-width=1920 output-height=1080";
    for (name, properties) in [
        (
            "perspective-720p-nearest",
            format!("{perspective} interpolation=nearest"),
        ),
        (
            "perspective-720p-bilinear",
            format!("{perspective} interpolation=bilinear"),
        ),
        (
            "rotated-1080p-nearest",
            format!("{rotated} interpolation=nearest"),
        ),
        (
            "rotated-1080p-bilinear",
            format!("{rotated} interpolation=bilinear"),
        ),
        (
            "perspective-720p-bilinear-4-threads",
            format!("{perspective} interpolation=bilinear n-threads=4"),
        ),
        (
            "rotated-1080p-bilinear-4-threads",
            format!("{rotated} interpolation=bilinear n-threads=4"),
        ),
        ("identity-passthrough", String::new()),
    ] {
        let mut harness = gst_check::Harness::new_parse(&format!("wallwarp {properties}"));
        harness.set_src_caps_str(SOURCE);
        let input = gst::Buffer::with_size(1920 * 1080 * 4).expect("1080p frame");
        group.bench_function(name, |bencher| {
            bencher.iter(|| {
                harness.push(input.clone()).expect("pushing one frame");
                std::hint::black_box(harness.try_pull());
            });
        });
    }
    group.finish();
}

criterion_group!(benches, benchmark_hot_path);
criterion_main!(benches);
