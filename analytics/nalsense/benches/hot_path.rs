#![expect(
    clippy::expect_used,
    reason = "benchmark setup and each synchronous harness transfer must succeed"
)]

use std::sync::Once;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const BUFFER_SIZES: [BufferSize; 3] = [
    BufferSize {
        name: "1KiB",
        bytes: 1_024,
    },
    BufferSize {
        name: "16KiB",
        bytes: 16 * 1_024,
    },
    BufferSize {
        name: "256KiB",
        bytes: 256 * 1_024,
    },
];

const CODECS: [CodecCase; 2] = [
    CodecCase {
        name: "h264",
        caps: "video/x-h264,parsed=true,stream-format=byte-stream,alignment=au",
        access_unit: h264_p_access_unit,
        primer: None,
    },
    CodecCase {
        name: "h265",
        caps: "video/x-h265,parsed=true,stream-format=byte-stream,alignment=au",
        access_unit: h265_p_access_unit,
        primer: Some(h265_pps_access_unit),
    },
];

const TOPOLOGIES: [Topology; 3] = [
    Topology {
        name: "nalsenseactivity",
        pipeline: "nalsenseactivity",
    },
    Topology {
        name: "nalsensereplay-start-active",
        pipeline: "nalsensereplay start-active=true",
    },
    Topology {
        name: "nalsenseactivity-nalsensereplay-start-active",
        pipeline: "nalsenseactivity ! nalsensereplay start-active=true",
    },
];

#[derive(Clone, Copy)]
struct BufferSize {
    name: &'static str,
    bytes: usize,
}

#[derive(Clone, Copy)]
struct CodecCase {
    name: &'static str,
    caps: &'static str,
    access_unit: fn(usize) -> Vec<u8>,
    primer: Option<fn() -> Vec<u8>>,
}

#[derive(Clone, Copy)]
struct Topology {
    name: &'static str,
    pipeline: &'static str,
}

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().expect("initializing GStreamer for NALSense benchmarks");
        gstnalsense::plugin_register_static()
            .expect("registering the static NALSense benchmark plugin");
    });
}

fn h264_p_access_unit(size: usize) -> Vec<u8> {
    assert!(size >= 6, "an H.264 P access unit needs six bytes");
    let mut bytes = vec![0x55; size];
    bytes
        .get_mut(..6)
        .expect("checked H.264 fixture prefix")
        .copy_from_slice(&[0, 0, 0, 1, 0x41, 0xe0]);
    bytes
}

fn h265_pps_access_unit() -> Vec<u8> {
    // PPS id 0, SPS id 0, no dependent segments/output flag/extra header bits.
    vec![0, 0, 0, 1, 0x44, 0x01, 0xc0]
}

fn h265_p_access_unit(size: usize) -> Vec<u8> {
    assert!(size >= 7, "an H.265 P access unit needs seven bytes");
    let mut bytes = vec![0x55; size];
    bytes
        .get_mut(..7)
        .expect("checked H.265 fixture prefix")
        // TRAIL_R NAL followed by first-slice, PPS-id 0, P-slice header bits.
        .copy_from_slice(&[0, 0, 0, 1, 0x02, 0x01, 0xd0]);
    bytes
}

fn delta_buffer(bytes: Vec<u8>) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(bytes);
    buffer
        .get_mut()
        .expect("new benchmark buffer has one writable owner")
        .set_flags(gst::BufferFlags::DELTA_UNIT);
    buffer
}

fn harness_for(topology: Topology, codec: CodecCase) -> gst_check::Harness {
    let mut harness = gst_check::Harness::new_parse(topology.pipeline);
    harness.set_src_caps_str(codec.caps);
    harness.play();

    if let Some(primer) = codec.primer {
        harness
            .push_and_pull(gst::Buffer::from_mut_slice(primer()))
            .expect("priming the H.265 PPS scanner state");
    }

    harness
}

fn benchmark_hot_path(criterion: &mut Criterion) {
    init();

    for topology in TOPOLOGIES {
        let mut group = criterion.benchmark_group(topology.name);
        group.sample_size(20);
        group.warm_up_time(Duration::from_millis(500));
        group.measurement_time(Duration::from_secs(2));

        for codec in CODECS {
            for size in BUFFER_SIZES {
                let mut harness = harness_for(topology, codec);
                let mut reusable = Some(delta_buffer((codec.access_unit)(size.bytes)));
                group.throughput(Throughput::Bytes(
                    u64::try_from(size.bytes).expect("benchmark buffer size fits u64"),
                ));
                group.bench_with_input(
                    BenchmarkId::new(codec.name, size.name),
                    &size,
                    |bencher, _size| {
                        bencher.iter(|| {
                            let input = reusable
                                .take()
                                .expect("previous harness iteration returned its buffer");
                            let output = harness
                                .push_and_pull(input)
                                .expect("pushing and pulling one benchmark access unit");
                            reusable = Some(output);
                        });
                    },
                );
            }
        }
        group.finish();
    }
}

criterion_group!(benches, benchmark_hot_path);
criterion_main!(benches);
