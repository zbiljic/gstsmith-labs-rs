use std::str::FromStr;
use std::sync::Once;

use gst::prelude::*;

#[expect(
    clippy::expect_used,
    reason = "all tests require successful one-time GStreamer and plugin initialization"
)]
fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().expect("initializing GStreamer");
        gstnalsense::plugin_register_static().expect("registering NALSense plugin");
    });
}

#[test]
fn registers_nalsense_activity() {
    init();
    let factory = gst::ElementFactory::find("nalsenseactivity").expect("NALSense activity factory");
    assert_eq!(factory.rank(), gst::Rank::NONE);
    assert!(
        gst::ElementFactory::find("nalsense").is_none(),
        "the ambiguous pre-release element name must not remain registered"
    );
    assert!(gst::ElementFactory::find("nalsensereplay").is_some());
}

#[test]
fn exposes_only_au_aligned_annex_b_h264_and_h265() {
    init();
    let element = gst::ElementFactory::make("nalsenseactivity")
        .build()
        .expect("constructing NALSense");
    for pad_name in ["sink", "src"] {
        let caps = element
            .static_pad(pad_name)
            .expect("NALSense pad")
            .pad_template_caps();
        for supported in [
            "video/x-h264,parsed=true,stream-format=byte-stream,alignment=au",
            "video/x-h265,parsed=true,stream-format=byte-stream,alignment=au",
        ] {
            let supported = gst::Caps::from_str(supported).expect("valid supported caps");
            assert!(caps.can_intersect(&supported));
        }
        for unsupported in [
            "video/x-h264,parsed=true,stream-format=byte-stream,alignment=nal",
            "video/x-h264,parsed=true,stream-format=avc,alignment=au",
            "video/x-h265,parsed=true,stream-format=byte-stream,alignment=nal",
            "video/x-h265,parsed=true,stream-format=hvc1,alignment=au",
            "video/x-h265,parsed=true,stream-format=hev1,alignment=au",
        ] {
            let unsupported = gst::Caps::from_str(unsupported).expect("valid unsupported caps");
            assert!(!caps.can_intersect(&unsupported));
        }
    }
}

#[test]
fn exposes_documented_property_defaults() {
    init();
    let element = gst::ElementFactory::make("nalsenseactivity")
        .build()
        .expect("constructing NALSense");
    assert_eq!(element.property::<String>("stream-id"), "stream");
    assert!((element.property::<f64>("activity-threshold") - 3.0).abs() < f64::EPSILON);
    assert!((element.property::<f64>("baseline-alpha") - 0.02).abs() < f64::EPSILON);
    assert_eq!(element.property::<u32>("activity-min-frames"), 3);
    assert_eq!(element.property::<u32>("activity-clear-frames"), 4);
    assert_eq!(element.property::<u32>("warmup-frames"), 30);
    assert_eq!(element.property::<u32>("post-idr-guard-frames"), 0);
}

#[test]
fn exposes_documented_replay_contract() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .build()
        .expect("constructing NALSense replay");
    let expected = gst::Caps::from_str(
        "video/x-h264,parsed=true,stream-format=byte-stream,alignment=au;\
         video/x-h265,parsed=true,stream-format=byte-stream,alignment=au",
    )
    .expect("valid expected caps");
    for pad_name in ["sink", "src"] {
        let caps = element
            .static_pad(pad_name)
            .expect("NALSense replay pad")
            .pad_template_caps();
        assert_eq!(caps, expected);
    }

    assert_eq!(element.property::<u32>("max-buffer-frames"), 300);
    assert_eq!(element.property::<u64>("max-buffer-bytes"), 16_777_216);
    assert!(!element.property::<bool>("start-active"));
    assert!(!element.property::<bool>("active"));
    for (name, value) in [
        ("buffered-frames", 0_u64),
        ("buffered-bytes", 0),
        ("replayed-frames", 0),
        ("replayed-bytes", 0),
        ("forced-wakes", 0),
        ("peak-buffered-frames", 0),
        ("peak-buffered-bytes", 0),
    ] {
        assert_eq!(element.property::<u64>(name), value);
        let property = element.find_property(name).expect("documented property");
        assert!(property.flags().contains(gst::glib::ParamFlags::READABLE));
        assert!(!property.flags().contains(gst::glib::ParamFlags::WRITABLE));
    }
}

#[test]
fn passes_annex_b_access_units_and_metadata_unchanged() {
    init();
    let element = gst::ElementFactory::make("nalsenseactivity")
        .build()
        .expect("constructing NALSense");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    let bytes = annex_b_au(32, true);
    let mut input = gst::Buffer::from_slice(bytes.clone());
    {
        let input = input.get_mut().expect("writable input buffer");
        input.set_pts(gst::ClockTime::from_seconds(5));
        input.set_dts(gst::ClockTime::from_seconds(4));
        input.set_duration(gst::ClockTime::from_mseconds(40));
        input.set_offset(11);
        input.set_offset_end(19);
        input.set_flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER);
        let reference = gst::Caps::builder("timestamp/x-test").build();
        gst::ReferenceTimestampMeta::add(input, &reference, gst::ClockTime::from_seconds(42), None);
    }

    assert_eq!(harness.push(input), Ok(gst::FlowSuccess::Ok));
    let output = harness.pull().expect("pulling pass-through buffer");
    assert_eq!(
        output
            .map_readable()
            .expect("mapping pass-through buffer")
            .as_slice(),
        bytes
    );
    assert_eq!(output.pts(), Some(gst::ClockTime::from_seconds(5)));
    assert_eq!(output.dts(), Some(gst::ClockTime::from_seconds(4)));
    assert_eq!(output.duration(), Some(gst::ClockTime::from_mseconds(40)));
    assert_eq!(output.offset(), 11);
    assert_eq!(output.offset_end(), 19);
    assert!(
        output
            .flags()
            .contains(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
    );
    assert!(output.meta::<gst::ReferenceTimestampMeta>().is_some());
}

#[test]
fn passes_h265_access_units_and_metadata_unchanged() {
    init();
    let element = gst::ElementFactory::make("nalsenseactivity")
        .build()
        .expect("constructing NALSense");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h265,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    let bytes = h265_annex_b_au(32, 20);
    let mut input = gst::Buffer::from_slice(bytes.clone());
    {
        let input = input.get_mut().expect("writable input buffer");
        input.set_pts(gst::ClockTime::from_seconds(5));
        input.set_dts(gst::ClockTime::from_seconds(4));
        input.set_duration(gst::ClockTime::from_mseconds(40));
        input.set_offset(11);
        input.set_offset_end(19);
        input.set_flags(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER);
        let reference = gst::Caps::builder("timestamp/x-test").build();
        gst::ReferenceTimestampMeta::add(input, &reference, gst::ClockTime::from_seconds(42), None);
    }

    assert_eq!(harness.push(input), Ok(gst::FlowSuccess::Ok));
    let output = harness.pull().expect("pulling H.265 pass-through buffer");
    assert_eq!(
        output
            .map_readable()
            .expect("mapping H.265 pass-through buffer")
            .as_slice(),
        bytes
    );
    assert_eq!(output.pts(), Some(gst::ClockTime::from_seconds(5)));
    assert_eq!(output.dts(), Some(gst::ClockTime::from_seconds(4)));
    assert_eq!(output.duration(), Some(gst::ClockTime::from_mseconds(40)));
    assert_eq!(output.offset(), 11);
    assert_eq!(output.offset_end(), 19);
    assert!(
        output
            .flags()
            .contains(gst::BufferFlags::DISCONT | gst::BufferFlags::MARKER)
    );
    assert!(output.meta::<gst::ReferenceTimestampMeta>().is_some());
}

#[test]
fn h265_activity_preserves_parameter_sets_across_continuity_resets() {
    init();
    for reset in ["discont", "segment", "flush"] {
        let element = gst::ElementFactory::make("nalsenseactivity")
            .build()
            .expect("constructing H.265 activity");
        let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
        harness.set_src_caps_str("video/x-h265,parsed=true,stream-format=byte-stream,alignment=au");
        harness.play();
        assert_eq!(
            harness.push(h265_replay_buffer(0, 20)),
            Ok(gst::FlowSuccess::Ok)
        );
        let _idr = harness.pull().expect("initial IDR with PPS");

        let bytes = vec![0, 0, 0, 1, 0x02, 0x01, 0xd0, 0x55];
        let mut delta = uninspected_delta_buffer(bytes.clone(), 1);
        assert_eq!(harness.push(delta.clone()), Ok(gst::FlowSuccess::Ok));
        let _delta = harness.pull().expect("slice using cached PPS");
        if reset == "discont" {
            delta.make_mut().set_flags(gst::BufferFlags::DISCONT);
        } else {
            if reset == "flush" {
                assert!(harness.push_event(gst::event::FlushStart::new()));
                assert!(harness.push_event(gst::event::FlushStop::new(false)));
            }
            let segment = gst::FormattedSegment::<gst::ClockTime>::new();
            assert!(harness.push_event(gst::event::Segment::new(&segment)));
        }
        assert_eq!(harness.push(delta), Ok(gst::FlowSuccess::Ok), "{reset}");
        let output = harness.pull().expect("slice after continuity reset");
        assert_eq!(
            output.map_readable().expect("mapping output").as_slice(),
            bytes
        );

        // A replacement stream must not inherit the previous stream's PPS.
        assert!(harness.push_event(gst::event::StreamStart::new("replacement")));
        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        assert!(harness.push_event(gst::event::Segment::new(&segment)));
        assert_eq!(
            harness.push(uninspected_delta_buffer(bytes, 2)),
            Err(gst::FlowError::Error)
        );
    }
}

#[test]
fn emits_configured_activity_transitions() {
    init();
    let element = gst::ElementFactory::make("nalsenseactivity")
        .property("stream-id", "camera-1")
        .property("activity-threshold", 1.0_f64)
        .property("baseline-alpha", 0.1_f64)
        .property("activity-min-frames", 2_u32)
        .property("activity-clear-frames", 2_u32)
        .property("warmup-frames", 1_u32)
        .build()
        .expect("constructing configured NALSense");
    let bus = gst::Bus::new();
    element.set_bus(Some(&bus));
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    for (frame_number, size) in [
        (0_u64, 100_u32),
        (1, 100),
        (2, 200),
        (3, 200),
        (4, 119),
        (5, 119),
    ] {
        let mut input = gst::Buffer::from_mut_slice(annex_b_au(size, false));
        let input = input.get_mut().expect("writable sample buffer");
        input.set_pts(gst::ClockTime::from_useconds(
            frame_number.saturating_mul(40_000),
        ));
        input.set_flags(gst::BufferFlags::DELTA_UNIT);
        assert_eq!(harness.push(input.to_owned()), Ok(gst::FlowSuccess::Ok));
        let _output = harness.pull().expect("pulling analyzed sample buffer");
    }

    for (expected_frame, expected_type) in [(3_u64, "activity-start"), (5, "activity-stop")] {
        let message = bus
            .timed_pop_filtered(gst::ClockTime::SECOND, &[gst::MessageType::Element])
            .expect("receiving NALSense event");
        assert!(matches!(message.view(), gst::MessageView::Element(_)));
        let gst::MessageView::Element(element_message) = message.view() else {
            continue;
        };
        let structure = element_message
            .structure()
            .expect("NALSense element message structure");
        assert_eq!(structure.name(), "nalsense-activity");
        assert_eq!(
            structure.get::<String>("type").as_deref(),
            Ok(expected_type)
        );
        assert_eq!(
            structure.get::<String>("stream-id").as_deref(),
            Ok("camera-1")
        );
        assert_eq!(structure.get::<u64>("frame-number"), Ok(expected_frame));
        assert_eq!(structure.get::<String>("picture-type").as_deref(), Ok("P"));
        assert_eq!(structure.get::<bool>("reference-picture"), Ok(true));
        assert_eq!(
            structure.get::<u64>("timestamp-us"),
            Ok(expected_frame.saturating_mul(40_000))
        );
        let _score = structure.get::<f64>("score").expect("numeric score");
        let _intensity = structure
            .get::<f64>("intensity")
            .expect("numeric intensity");
        let _frame_size = structure
            .get::<u64>("frame-size")
            .expect("numeric frame size");
        let _baseline_size = structure
            .get::<f64>("baseline-size")
            .expect("numeric baseline size");
    }
    assert!(
        bus.timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Element])
            .is_none()
    );

    let mut downstream_types = Vec::new();
    while let Some(event) = harness.try_pull_event() {
        let gst::EventView::CustomDownstream(custom) = event.view() else {
            continue;
        };
        let Some(structure) = custom.structure() else {
            continue;
        };
        if structure.name() == "nalsense-activity"
            && let Ok(kind) = structure.get::<String>("type")
        {
            downstream_types.push(kind);
        }
    }
    assert_eq!(downstream_types, ["activity-start", "activity-stop"]);
}

#[test]
fn emits_configured_h265_activity_transitions() {
    init();
    let element = gst::ElementFactory::make("nalsenseactivity")
        .property("stream-id", "camera-hevc")
        .property("activity-threshold", 1.0_f64)
        .property("baseline-alpha", 0.1_f64)
        .property("activity-min-frames", 2_u32)
        .property("activity-clear-frames", 2_u32)
        .property("warmup-frames", 1_u32)
        .build()
        .expect("constructing configured H.265 NALSense");
    let bus = gst::Bus::new();
    element.set_bus(Some(&bus));
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h265,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    for (frame_number, size) in [
        (0_u64, 100_u32),
        (1, 100),
        (2, 200),
        (3, 200),
        (4, 119),
        (5, 119),
    ] {
        let mut input = gst::Buffer::from_mut_slice(h265_annex_b_au(size, 1));
        let input = input.get_mut().expect("writable H.265 sample buffer");
        input.set_pts(gst::ClockTime::from_useconds(
            frame_number.saturating_mul(40_000),
        ));
        input.set_flags(gst::BufferFlags::DELTA_UNIT);
        assert_eq!(harness.push(input.to_owned()), Ok(gst::FlowSuccess::Ok));
        let _output = harness.pull().expect("pulling analyzed H.265 buffer");
    }

    for (expected_frame, expected_type) in [(3_u64, "activity-start"), (5, "activity-stop")] {
        let message = bus
            .timed_pop_filtered(gst::ClockTime::SECOND, &[gst::MessageType::Element])
            .expect("receiving H.265 NALSense event");
        let gst::MessageView::Element(element_message) = message.view() else {
            panic!("expected H.265 activity element message");
        };
        let structure = element_message
            .structure()
            .expect("H.265 NALSense element message structure");
        assert_eq!(
            structure.get::<String>("type").as_deref(),
            Ok(expected_type)
        );
        assert_eq!(structure.get::<u64>("frame-number"), Ok(expected_frame));
        assert_eq!(structure.get::<String>("picture-type").as_deref(), Ok("P"));
        assert_eq!(structure.get::<bool>("reference-picture"), Ok(true));
    }
}

#[test]
fn replay_buffers_from_idr_and_wakes_on_serialized_activity_event() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .build()
        .expect("constructing NALSense replay");
    let bus = gst::Bus::new();
    element.set_bus(Some(&bus));
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(replay_buffer(10, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness.push(replay_buffer(11, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(harness.buffers_in_queue(), 0);
    assert_eq!(element.property::<u64>("buffered-frames"), 2);

    assert!(harness.push_event(activity_event("activity-start", 12)));
    assert_eq!(harness.buffers_in_queue(), 2);
    let idr = harness.pull().expect("replayed IDR");
    let delta = harness.pull().expect("replayed dependent frame");
    assert_eq!(idr.pts(), Some(gst::ClockTime::from_mseconds(400)));
    assert_eq!(delta.pts(), Some(gst::ClockTime::from_mseconds(440)));
    assert!(idr.flags().contains(gst::BufferFlags::DISCONT));
    assert!(delta.flags().contains(gst::BufferFlags::DELTA_UNIT));
    assert!(element.property::<bool>("active"));
    assert_eq!(element.property::<u64>("buffered-frames"), 0);
    assert_eq!(element.property::<u64>("replayed-frames"), 2);
    assert_eq!(element.property::<u64>("replayed-bytes"), 72);
    assert_eq!(element.property::<u64>("peak-buffered-frames"), 2);
    assert_eq!(element.property::<u64>("peak-buffered-bytes"), 72);

    assert_eq!(
        harness.push(replay_buffer(12, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness.pull().expect("live frame after replay").pts(),
        Some(gst::ClockTime::from_mseconds(480))
    );

    let message = bus
        .timed_pop_filtered(gst::ClockTime::SECOND, &[gst::MessageType::Element])
        .expect("replay wake message");
    let gst::MessageView::Element(message) = message.view() else {
        panic!("expected replay element message");
    };
    let structure = message.structure().expect("replay message structure");
    assert_eq!(structure.name(), "nalsense-replay");
    assert_eq!(
        structure.get::<String>("reason").as_deref(),
        Ok("activity-event")
    );
    assert_eq!(structure.get::<u64>("replayed-frames"), Ok(2));
}

#[test]
fn replay_buffers_h265_from_idr_and_ignores_cra_as_a_safe_boundary() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .build()
        .expect("constructing H.265 NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h265,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(h265_replay_buffer(10, 21)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(element.property::<u64>("buffered-frames"), 0);
    assert_eq!(
        harness.push(h265_replay_buffer(11, 20)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness.push(h265_replay_buffer(12, 1)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(element.property::<u64>("buffered-frames"), 2);

    assert!(harness.push_event(activity_event("activity-start", 13)));
    assert_eq!(harness.buffers_in_queue(), 2);
    let idr = harness.pull().expect("replayed H.265 IDR");
    let delta = harness.pull().expect("replayed H.265 dependent frame");
    assert_eq!(idr.pts(), Some(gst::ClockTime::from_mseconds(440)));
    assert_eq!(delta.pts(), Some(gst::ClockTime::from_mseconds(480)));
    assert!(idr.flags().contains(gst::BufferFlags::DISCONT));
    assert!(delta.flags().contains(gst::BufferFlags::DELTA_UNIT));
}

#[test]
fn linked_activity_event_wakes_replay_before_the_triggering_access_unit() {
    init();
    let activity = gst::ElementFactory::make("nalsenseactivity")
        .property("activity-threshold", 1.0_f64)
        .property("baseline-alpha", 0.1_f64)
        .property("activity-min-frames", 1_u32)
        .property("warmup-frames", 1_u32)
        .build()
        .expect("constructing configured NALSense activity element");
    let replay = gst::ElementFactory::make("nalsensereplay")
        .build()
        .expect("constructing NALSense replay");
    let bin = gst::Bin::new();
    bin.add_many([&activity, &replay])
        .expect("adding NALSense elements to test bin");
    activity
        .link(&replay)
        .expect("linking NALSense activity and replay elements");

    let activity_sink = activity
        .static_pad("sink")
        .expect("NALSense activity sink pad");
    let replay_src = replay
        .static_pad("src")
        .expect("NALSense replay source pad");
    let sink = gst::GhostPad::builder_with_target(&activity_sink)
        .expect("building test-bin sink ghost pad")
        .name("sink")
        .build();
    let src = gst::GhostPad::builder_with_target(&replay_src)
        .expect("building test-bin source ghost pad")
        .name("src")
        .build();
    bin.add_pad(&sink).expect("adding test-bin sink ghost pad");
    bin.add_pad(&src).expect("adding test-bin source ghost pad");

    let element = bin.upcast::<gst::Element>();
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(replay_sized_buffer(0, 32, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness.push(replay_sized_buffer(1, 32, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(harness.buffers_in_queue(), 0);
    assert_eq!(
        harness.push(replay_sized_buffer(2, 96, false)),
        Ok(gst::FlowSuccess::Ok)
    );

    assert_eq!(harness.buffers_in_queue(), 3);
    for (expected_frame, expected_discontinuity) in [(0_u64, true), (1, false), (2, false)] {
        let output = harness.pull().expect("linked NALSense output buffer");
        assert_eq!(
            output.pts(),
            Some(gst::ClockTime::from_mseconds(
                expected_frame.saturating_mul(40)
            ))
        );
        assert_eq!(
            output.flags().contains(gst::BufferFlags::DISCONT),
            expected_discontinuity
        );
    }
    assert!(replay.property::<bool>("active"));
    assert_eq!(replay.property::<u64>("replayed-frames"), 2);
}

#[test]
fn replay_sleep_waits_for_next_idr_boundary() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .property("start-active", true)
        .build()
        .expect("constructing active NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(replay_buffer(20, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    let _first = harness.pull().expect("initial live frame");
    element.emit_by_name::<()>("sleep", &[]);
    assert!(element.property::<bool>("active"));

    assert_eq!(
        harness.push(replay_buffer(21, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    let _tail = harness.pull().expect("delta before sleep boundary");
    assert_eq!(
        harness.push(replay_buffer(22, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert!(!element.property::<bool>("active"));
    assert_eq!(
        harness.push(replay_buffer(23, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(harness.buffers_in_queue(), 0);

    element.emit_by_name::<()>("wake", &[]);
    assert_eq!(
        harness.push(replay_buffer(24, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(harness.buffers_in_queue(), 3);
    for expected_frame in [22_u64, 23, 24] {
        assert_eq!(
            harness.pull().expect("replayed sleep interval").pts(),
            Some(gst::ClockTime::from_mseconds(
                expected_frame.saturating_mul(40)
            ))
        );
    }
    assert!(element.property::<bool>("active"));
}

#[test]
fn replay_propagates_push_failures_and_counts_only_delivered_prefix_buffers() {
    init();
    for wake in ["activity", "action", "limit"] {
        for fail_at in 0..2 {
            for error in [gst::FlowError::Error, gst::FlowError::Flushing] {
                let warmup = if wake == "activity" { 1 } else { 30 };
                let bin = gst::parse::bin_from_description(
                    &format!(
                        "nalsenseactivity activity-threshold=1 baseline-alpha=0.1 \
                         activity-min-frames=1 warmup-frames={warmup} ! \
                         nalsensereplay name=replay max-buffer-frames=2"
                    ),
                    true,
                )
                .expect("linked activity and replay");
                let replay = bin.by_name("replay").expect("replay element");
                let mut harness = gst_check::Harness::with_element(&bin, Some("sink"), Some("src"));
                harness.set_src_caps_str(
                    "video/x-h264,parsed=true,stream-format=byte-stream,alignment=au",
                );
                harness.play();
                let attempts = std::sync::atomic::AtomicU64::new(0);
                harness.sinkpad().expect("harness sink").add_probe(
                    gst::PadProbeType::BUFFER,
                    move |_, info| {
                        if attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == fail_at {
                            let _buffer = info.take_buffer();
                            info.flow_res = Err(error);
                            gst::PadProbeReturn::Handled
                        } else {
                            gst::PadProbeReturn::Ok
                        }
                    },
                );
                assert_eq!(
                    harness.push(replay_buffer(0, true)),
                    Ok(gst::FlowSuccess::Ok)
                );
                assert_eq!(
                    harness.push(replay_buffer(1, false)),
                    Ok(gst::FlowSuccess::Ok)
                );
                if wake == "action" {
                    replay.emit_by_name::<()>("wake", &[]);
                }
                assert_eq!(
                    harness.push(replay_sized_buffer(2, 96, false)),
                    Err(error),
                    "{wake}, failure at {fail_at}"
                );
                assert_eq!(replay.property::<u64>("replayed-frames"), fail_at);
                assert_eq!(replay.property::<u64>("replayed-bytes"), fail_at * 36);
                assert!(!replay.property::<bool>("active"));
                assert_eq!(replay.property::<u64>("buffered-frames"), 0);
                assert_eq!(harness.push(replay_buffer(3, false)), Err(error));
                while harness.try_pull().is_some() {}

                assert!(harness.push_event(gst::event::FlushStart::new()));
                assert!(harness.push_event(gst::event::FlushStop::new(false)));
                let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                assert!(harness.push_event(gst::event::Segment::new(&segment)));
                assert_eq!(
                    harness.push(replay_buffer(4, false)),
                    Ok(gst::FlowSuccess::Ok)
                );
                assert_eq!(harness.buffers_in_queue(), 0);
                assert_eq!(
                    harness.push(replay_buffer(5, true)),
                    Ok(gst::FlowSuccess::Ok)
                );
                let output = harness.pull().expect("fresh IDR after recovery");
                assert!(output.flags().contains(gst::BufferFlags::DISCONT));
                assert!(replay.property::<bool>("active"));
                assert_eq!(replay.property::<u64>("replayed-frames"), fail_at);
            }
        }
    }
}

#[test]
fn active_replay_forwards_an_uninspected_h264_delta_buffer_with_metadata() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .property("start-active", true)
        .build()
        .expect("constructing active NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    // DELTA_UNIT is the parser-provided negative-candidate fast path, so this
    // deliberately non-Annex-B payload must not be inspected by replay.
    let bytes = vec![0xde, 0xad, 0xbe, 0xef];
    let mut input = gst::Buffer::from_slice(bytes.clone());
    {
        let input = input.get_mut().expect("writable delta test buffer");
        input.set_pts(gst::ClockTime::from_seconds(5));
        input.set_dts(gst::ClockTime::from_seconds(4));
        input.set_duration(gst::ClockTime::from_mseconds(40));
        input.set_flags(gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER);
    }

    assert_eq!(harness.push(input), Ok(gst::FlowSuccess::Ok));
    let output = harness.pull().expect("forwarded active delta buffer");
    assert_eq!(
        output
            .map_readable()
            .expect("mapping forwarded delta buffer")
            .as_slice(),
        bytes
    );
    assert_eq!(output.pts(), Some(gst::ClockTime::from_seconds(5)));
    assert_eq!(output.dts(), Some(gst::ClockTime::from_seconds(4)));
    assert_eq!(output.duration(), Some(gst::ClockTime::from_mseconds(40)));
    assert!(
        output
            .flags()
            .contains(gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER)
    );
}

#[test]
fn active_h265_replay_forwards_a_slice_only_delta_without_pps_state() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .property("start-active", true)
        .build()
        .expect("constructing active H.265 NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h265,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    // This valid H.265 delta slice has no preceding PPS in replay state. The
    // active path must forward it without the activity parser's PPS lookup.
    let bytes = vec![0, 0, 1, 0x02, 0x01, 0xd0];
    let mut input = gst::Buffer::from_slice(bytes.clone());
    input
        .get_mut()
        .expect("writable H.265 delta test buffer")
        .set_flags(gst::BufferFlags::DELTA_UNIT);

    assert_eq!(harness.push(input), Ok(gst::FlowSuccess::Ok));
    assert_eq!(
        harness
            .pull()
            .expect("forwarded H.265 delta slice")
            .map_readable()
            .expect("mapping forwarded H.265 delta slice")
            .as_slice(),
        bytes
    );
}

#[test]
fn dormant_replay_drops_an_uninspected_delta_without_an_idr_prefix() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .build()
        .expect("constructing dormant NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    // DELTA_UNIT makes this malformed payload an uninspected negative
    // candidate rather than accepted upstream Annex-B input.
    assert_eq!(
        harness.push(uninspected_delta_buffer(vec![0xff], 1)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(harness.buffers_in_queue(), 0);
    assert_eq!(element.property::<u64>("buffered-frames"), 0);
}

#[test]
fn replay_sleep_forwards_uninspected_deltas_until_a_true_idr() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .property("start-active", true)
        .build()
        .expect("constructing active NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    element.emit_by_name::<()>("sleep", &[]);
    // The parser-provided delta flag avoids inspecting this intentionally
    // malformed payload while sleep waits for an actual IDR boundary.
    assert_eq!(
        harness.push(uninspected_delta_buffer(vec![0xff], 2)),
        Ok(gst::FlowSuccess::Ok)
    );
    let _delta = harness
        .pull()
        .expect("forwarded delta before sleep boundary");
    assert!(element.property::<bool>("active"));

    assert_eq!(
        harness.push(replay_buffer(3, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert!(!element.property::<bool>("active"));
    assert_eq!(harness.buffers_in_queue(), 0);
}

#[test]
fn replay_waits_for_idr_when_woken_without_a_decodable_prefix() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .build()
        .expect("constructing NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(replay_buffer(30, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    element.emit_by_name::<()>("wake", &[]);
    assert_eq!(
        harness.push(replay_buffer(31, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(harness.buffers_in_queue(), 0);
    assert_eq!(
        harness.push(replay_buffer(32, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    let idr = harness.pull().expect("first IDR after wake request");
    assert_eq!(idr.pts(), Some(gst::ClockTime::from_mseconds(1_280)));
    assert!(idr.flags().contains(gst::BufferFlags::DISCONT));
    assert!(element.property::<bool>("active"));
}

#[test]
fn replay_forces_a_wake_before_its_frame_bound_is_exceeded() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .property("max-buffer-frames", 2_u32)
        .build()
        .expect("constructing bounded NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(replay_buffer(40, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness.push(replay_buffer(41, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness.push(replay_buffer(42, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(harness.buffers_in_queue(), 3);
    assert_eq!(element.property::<u64>("forced-wakes"), 1);
    assert_eq!(element.property::<u64>("replayed-frames"), 2);
    assert!(element.property::<bool>("active"));
    assert!(
        harness
            .pull()
            .expect("forced replay IDR")
            .flags()
            .contains(gst::BufferFlags::DISCONT)
    );
    let _delta_one = harness.pull().expect("forced replay delta");
    let _delta_two = harness.pull().expect("live overflow delta");
}

#[test]
fn replay_forces_a_wake_when_one_idr_exceeds_its_byte_bound() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .property("max-buffer-bytes", 16_u64)
        .build()
        .expect("constructing byte-bounded NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(replay_buffer(50, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(harness.buffers_in_queue(), 1);
    assert_eq!(element.property::<u64>("buffered-bytes"), 0);
    assert_eq!(element.property::<u64>("forced-wakes"), 1);
    assert!(element.property::<bool>("active"));
    assert!(
        harness
            .pull()
            .expect("oversized safety-wake IDR")
            .flags()
            .contains(gst::BufferFlags::DISCONT)
    );
}

#[test]
fn replay_discards_a_cached_prefix_after_an_upstream_discontinuity() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .build()
        .expect("constructing NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(replay_buffer(60, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness.push(replay_buffer(61, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(element.property::<u64>("buffered-frames"), 2);

    let mut discontinuity = replay_buffer(62, false);
    discontinuity
        .make_mut()
        .set_flags(gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::DISCONT);
    assert_eq!(harness.push(discontinuity), Ok(gst::FlowSuccess::Ok));
    assert_eq!(element.property::<u64>("buffered-frames"), 0);

    element.emit_by_name::<()>("wake", &[]);
    assert_eq!(
        harness.push(replay_buffer(63, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(harness.buffers_in_queue(), 0);
    assert_eq!(
        harness.push(replay_buffer(64, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(harness.buffers_in_queue(), 1);
}

#[test]
fn replay_keeps_an_active_gate_and_counters_across_a_discontinuity() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .build()
        .expect("constructing NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(replay_buffer(70, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness.push(replay_buffer(71, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert!(harness.push_event(activity_event("activity-start", 72)));
    let _idr = harness.pull().expect("replayed IDR");
    let _delta = harness.pull().expect("replayed dependent frame");
    assert_eq!(element.property::<u64>("replayed-frames"), 2);

    assert_eq!(
        harness.push(replay_discontinuity_buffer(72, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness.pull().expect("live discontinuous frame").pts(),
        Some(gst::ClockTime::from_mseconds(2_880))
    );
    assert!(element.property::<bool>("active"));
    assert_eq!(element.property::<u64>("replayed-frames"), 2);
    assert_eq!(element.property::<u64>("replayed-bytes"), 72);
    assert_eq!(element.property::<u64>("peak-buffered-frames"), 2);
}

#[test]
fn replay_keeps_an_active_gate_and_counters_across_stream_events() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .build()
        .expect("constructing NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(replay_buffer(75, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness.push(replay_buffer(76, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert!(harness.push_event(activity_event("activity-start", 77)));
    let _idr = harness.pull().expect("replayed IDR");
    let _delta = harness.pull().expect("replayed dependent frame");

    assert!(harness.push_event(gst::event::StreamStart::new("replacement-stream")));
    let caps =
        gst::Caps::from_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au")
            .expect("valid replay caps");
    assert!(harness.push_event(gst::event::Caps::new(&caps)));
    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
    assert!(harness.push_event(gst::event::Segment::new(&segment)));
    assert!(harness.push_event(gst::event::FlushStart::new()));
    assert!(harness.push_event(gst::event::FlushStop::new(false)));
    assert!(harness.push_event(gst::event::Segment::new(&segment)));

    assert!(element.property::<bool>("active"));
    assert_eq!(element.property::<u64>("replayed-frames"), 2);
    assert_eq!(element.property::<u64>("replayed-bytes"), 72);
    assert_eq!(element.property::<u64>("peak-buffered-frames"), 2);
    assert_eq!(
        harness.push(replay_buffer(77, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness
            .pull()
            .expect("live frame after stream events")
            .pts(),
        Some(gst::ClockTime::from_mseconds(3_080))
    );
}

#[test]
fn replay_keeps_a_pending_wake_but_discards_its_stale_prefix() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .build()
        .expect("constructing NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(replay_buffer(80, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(
        harness.push(replay_buffer(81, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    element.emit_by_name::<()>("wake", &[]);

    assert_eq!(
        harness.push(replay_discontinuity_buffer(82, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(element.property::<u64>("buffered-frames"), 0);
    assert_eq!(harness.buffers_in_queue(), 0);
    assert_eq!(
        harness.push(replay_buffer(83, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(harness.buffers_in_queue(), 0);

    assert_eq!(
        harness.push(replay_buffer(84, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    let idr = harness.pull().expect("fresh IDR after pending wake");
    assert_eq!(idr.pts(), Some(gst::ClockTime::from_mseconds(3_360)));
    assert!(idr.flags().contains(gst::BufferFlags::DISCONT));
    assert!(element.property::<bool>("active"));
}

#[test]
fn replay_completes_a_pending_sleep_at_a_discontinuity() {
    init();
    let element = gst::ElementFactory::make("nalsensereplay")
        .property("start-active", true)
        .build()
        .expect("constructing active NALSense replay");
    let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
    harness.set_src_caps_str("video/x-h264,parsed=true,stream-format=byte-stream,alignment=au");
    harness.play();

    assert_eq!(
        harness.push(replay_buffer(90, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    let _live = harness.pull().expect("initial live frame");
    element.emit_by_name::<()>("sleep", &[]);
    assert!(element.property::<bool>("active"));

    assert_eq!(
        harness.push(replay_discontinuity_buffer(91, false)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert!(!element.property::<bool>("active"));
    assert_eq!(harness.buffers_in_queue(), 0);
    assert_eq!(
        harness.push(replay_buffer(92, true)),
        Ok(gst::FlowSuccess::Ok)
    );
    assert_eq!(element.property::<u64>("buffered-frames"), 1);
    assert_eq!(harness.buffers_in_queue(), 0);

    assert!(harness.push_event(activity_event("activity-start", 93)));
    assert_eq!(harness.buffers_in_queue(), 1);
    assert!(element.property::<bool>("active"));
}

#[test]
fn rejects_non_annex_b_input_cleanly() {
    init();
    for media_type in ["video/x-h264", "video/x-h265"] {
        let element = gst::ElementFactory::make("nalsenseactivity")
            .build()
            .expect("constructing NALSense");
        let mut harness = gst_check::Harness::with_element(&element, Some("sink"), Some("src"));
        harness.set_src_caps_str(&format!(
            "{media_type},parsed=true,stream-format=byte-stream,alignment=au"
        ));
        harness.play();
        assert_eq!(
            harness.push(gst::Buffer::from_slice([1_u8, 2, 3])),
            Err(gst::FlowError::Error)
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "the generated test fixture has checked, bounded dimensions"
)]
fn annex_b_au(encoded_vcl_bytes: u32, keyframe: bool) -> Vec<u8> {
    let payload_size = usize::try_from(encoded_vcl_bytes).expect("fixture size fits usize");
    assert!(
        payload_size > 1,
        "a VCL NAL requires a header and slice payload"
    );
    let total = payload_size
        .checked_add(4)
        .expect("fixture allocation size");
    let mut bytes = vec![0x55; total];
    bytes
        .get_mut(..4)
        .expect("fixture has a start-code prefix")
        .copy_from_slice(&[0, 0, 0, 1]);
    *bytes.get_mut(4).expect("fixture has a NAL header") = if keyframe { 0x65 } else { 0x41 };
    *bytes.get_mut(5).expect("fixture has a slice header") = if keyframe { 0xb8 } else { 0xe0 };
    bytes
}

#[expect(
    clippy::expect_used,
    reason = "the generated H.265 fixture has checked, bounded dimensions"
)]
fn h265_annex_b_au(encoded_vcl_bytes: u32, nal_type: u8) -> Vec<u8> {
    let payload_size = usize::try_from(encoded_vcl_bytes).expect("fixture size fits usize");
    assert!(
        payload_size > 2,
        "an H.265 VCL NAL requires a two-byte header and slice payload"
    );
    let mut bytes = Vec::with_capacity(
        payload_size
            .checked_add(11)
            .expect("H.265 fixture allocation size"),
    );
    // PPS id 0, SPS id 0, no dependent segments/output flag/extra header bits.
    bytes.extend_from_slice(&[0, 0, 0, 1, 0x44, 0x01, 0xc0]);
    bytes.extend_from_slice(&[0, 0, 0, 1, nal_type << 1, 0x01]);
    // first_slice_segment_in_pic_flag, optional IRAP flag, PPS id, slice_type.
    bytes.push(if (16..=21).contains(&nal_type) {
        0xac
    } else {
        0xd0
    });
    bytes.resize(
        bytes
            .len()
            .checked_add(payload_size.saturating_sub(3))
            .expect("H.265 fixture final size"),
        0x55,
    );
    bytes
}

fn replay_buffer(frame: u64, is_idr: bool) -> gst::Buffer {
    replay_sized_buffer(frame, 32, is_idr)
}

#[expect(
    clippy::expect_used,
    reason = "a newly allocated H.265 test buffer has one writable owner"
)]
fn h265_replay_buffer(frame: u64, nal_type: u8) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(h265_annex_b_au(32, nal_type));
    let buffer_ref = buffer
        .get_mut()
        .expect("unique H.265 replay fixture buffer");
    buffer_ref.set_pts(gst::ClockTime::from_mseconds(frame.saturating_mul(40)));
    buffer_ref.set_duration(gst::ClockTime::from_mseconds(40));
    if !(16..=21).contains(&nal_type) {
        buffer_ref.set_flags(gst::BufferFlags::DELTA_UNIT);
    }
    buffer
}

#[expect(
    clippy::expect_used,
    reason = "a newly allocated test buffer has one writable owner"
)]
fn replay_sized_buffer(frame: u64, encoded_vcl_bytes: u32, is_idr: bool) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(annex_b_au(encoded_vcl_bytes, is_idr));
    let buffer_ref = buffer.get_mut().expect("unique replay fixture buffer");
    buffer_ref.set_pts(gst::ClockTime::from_mseconds(frame.saturating_mul(40)));
    buffer_ref.set_duration(gst::ClockTime::from_mseconds(40));
    if !is_idr {
        buffer_ref.set_flags(gst::BufferFlags::DELTA_UNIT);
    }
    buffer
}

fn replay_discontinuity_buffer(frame: u64, is_idr: bool) -> gst::Buffer {
    let mut buffer = replay_buffer(frame, is_idr);
    let flags = buffer.flags();
    buffer
        .make_mut()
        .set_flags(flags | gst::BufferFlags::DISCONT);
    buffer
}

#[expect(
    clippy::expect_used,
    reason = "a newly allocated delta test buffer has one writable owner"
)]
fn uninspected_delta_buffer(bytes: Vec<u8>, frame: u64) -> gst::Buffer {
    let mut buffer = gst::Buffer::from_mut_slice(bytes);
    let buffer_ref = buffer
        .get_mut()
        .expect("unique uninspected delta test buffer");
    buffer_ref.set_pts(gst::ClockTime::from_mseconds(frame.saturating_mul(40)));
    buffer_ref.set_flags(gst::BufferFlags::DELTA_UNIT);
    buffer
}

fn activity_event(kind: &str, frame: u64) -> gst::Event {
    gst::event::CustomDownstream::new(
        gst::Structure::builder("nalsense-activity")
            .field("type", kind)
            .field("frame-number", frame)
            .build(),
    )
}
