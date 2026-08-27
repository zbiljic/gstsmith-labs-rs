use gst::prelude::*;

use crate::analyzer::ActivityEvent;

pub(crate) fn post(element: &super::NalSenseActivity, stream_id: &str, event: ActivityEvent) {
    let mut structure = gst::Structure::builder("nalsense-activity")
        .field("type", event.kind.name())
        .field("stream-id", stream_id)
        .field("frame-number", event.observation.frame_number)
        .field("picture-type", event.observation.picture_type.name())
        .field("reference-picture", event.observation.is_reference_picture)
        .field("score", event.score)
        .field("intensity", event.intensity)
        .field("frame-size", u64::from(event.observation.encoded_vcl_bytes))
        .field("baseline-size", event.baseline_size)
        .build();
    if let Some(timestamp_us) = event.observation.timestamp_us {
        structure.set("timestamp-us", timestamp_us);
    }
    let downstream_event = gst::event::CustomDownstream::new(structure.clone());
    let message = gst::message::Element::builder(structure)
        .src(element)
        .build();
    let _posted = element.post_message(message);
    if let Some(srcpad) = element.static_pad("src") {
        let _forwarded = srcpad.push_event(downstream_event);
    }
}
