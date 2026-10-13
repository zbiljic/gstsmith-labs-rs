//! Conversion of buffers, caps, and metas to JSON records.

use gst::prelude::*;
use serde_json::{Map, Value, json};

fn clock_time(time: Option<gst::ClockTime>) -> Value {
    time.map_or(Value::Null, |time| json!(time.nseconds()))
}

fn offset(value: u64) -> Value {
    if value == u64::MAX {
        Value::Null
    } else {
        json!(value)
    }
}

fn flags(flags: gst::BufferFlags) -> Value {
    let names = [
        (gst::BufferFlags::LIVE, "live"),
        (gst::BufferFlags::DECODE_ONLY, "decode-only"),
        (gst::BufferFlags::DISCONT, "discont"),
        (gst::BufferFlags::RESYNC, "resync"),
        (gst::BufferFlags::CORRUPTED, "corrupted"),
        (gst::BufferFlags::MARKER, "marker"),
        (gst::BufferFlags::HEADER, "header"),
        (gst::BufferFlags::GAP, "gap"),
        (gst::BufferFlags::DROPPABLE, "droppable"),
        (gst::BufferFlags::DELTA_UNIT, "delta-unit"),
        (gst::BufferFlags::TAG_MEMORY, "tag-memory"),
        (gst::BufferFlags::SYNC_AFTER, "sync-after"),
        (gst::BufferFlags::NON_DROPPABLE, "non-droppable"),
    ];
    Value::Array(
        names
            .into_iter()
            .filter(|(flag, _)| flags.contains(*flag))
            .map(|(_, name)| json!(name))
            .collect(),
    )
}

/// A `GValue` as JSON: numbers, booleans, and strings natively, arrays and
/// lists as JSON arrays, structures as objects, anything else in its
/// `GStreamer` serialized form.
fn value(value: &glib::SendValue) -> Value {
    if let Ok(v) = value.get::<bool>() {
        json!(v)
    } else if let Ok(v) = value.get::<i32>() {
        json!(v)
    } else if let Ok(v) = value.get::<u32>() {
        json!(v)
    } else if let Ok(v) = value.get::<i64>() {
        json!(v)
    } else if let Ok(v) = value.get::<u64>() {
        json!(v)
    } else if let Ok(v) = value.get::<f32>() {
        json!(v)
    } else if let Ok(v) = value.get::<f64>() {
        json!(v)
    } else if let Ok(v) = value.get::<String>() {
        json!(v)
    } else if let Ok(v) = value.get::<gst::Array>() {
        Value::Array(v.iter().map(self::value).collect())
    } else if let Ok(v) = value.get::<gst::List>() {
        Value::Array(v.iter().map(self::value).collect())
    } else if let Ok(v) = value.get::<gst::Structure>() {
        structure(&v)
    } else {
        value
            .serialize()
            .map_or(Value::Null, |serialized| json!(serialized.as_str()))
    }
}

use gst::glib;

fn structure(structure: &gst::StructureRef) -> Value {
    Value::Object(
        structure
            .iter()
            .map(|(name, v)| (name.to_string(), value(v)))
            .collect::<Map<_, _>>(),
    )
}

/// Every meta on `buffer`, by API name; custom metas include their fields.
fn metas(buffer: &gst::BufferRef) -> Value {
    Value::Array(
        buffer
            .iter_meta::<gst::Meta>()
            .map(|meta| {
                let mut record = Map::new();
                match meta.try_as_custom_meta() {
                    Some(custom) => {
                        record.insert("api".to_owned(), json!(custom.type_().name()));
                        record.insert("fields".to_owned(), structure(custom.structure()));
                    }
                    None => {
                        record.insert("api".to_owned(), json!(meta.api().name()));
                    }
                }
                Value::Object(record)
            })
            .collect(),
    )
}

pub(crate) fn buffer(seq: u64, buffer: &gst::BufferRef) -> Value {
    json!({
        "kind": "buffer",
        "seq": seq,
        "pts": clock_time(buffer.pts()),
        "dts": clock_time(buffer.dts()),
        "duration": clock_time(buffer.duration()),
        "offset": offset(buffer.offset()),
        "offset_end": offset(buffer.offset_end()),
        "flags": flags(buffer.flags()),
        "size": buffer.size(),
        "metas": metas(buffer),
    })
}

pub(crate) fn caps(caps: &gst::CapsRef) -> Value {
    json!({ "kind": "caps", "caps": caps.to_string() })
}

pub(crate) fn eos() -> Value {
    json!({ "kind": "eos" })
}

#[cfg(test)]
mod tests {
    use gst::meta::CustomMeta;

    use super::*;

    #[test]
    fn buffer_record_keeps_each_instance_of_the_same_custom_meta() {
        gst::init().expect("GStreamer should initialize");
        CustomMeta::register("JsonTapRepeatedMeta", &[]);
        let mut buf = gst::Buffer::new();
        for index in [1u64, 2] {
            CustomMeta::add(buf.get_mut().expect("writable"), "JsonTapRepeatedMeta")
                .expect("meta adds")
                .mut_structure()
                .set("index", index);
        }
        assert_eq!(
            buffer(0, &buf)["metas"],
            json!([
                {"api": "JsonTapRepeatedMeta", "fields": {"index": 1}},
                {"api": "JsonTapRepeatedMeta", "fields": {"index": 2}},
            ])
        );
    }

    #[test]
    fn buffer_record_includes_memory_and_sync_flags() {
        gst::init().expect("GStreamer should initialize");
        let mut buf = gst::Buffer::new();
        buf.get_mut()
            .expect("writable")
            .set_flags(gst::BufferFlags::TAG_MEMORY | gst::BufferFlags::SYNC_AFTER);
        assert_eq!(
            buffer(0, &buf)["flags"],
            json!(["tag-memory", "sync-after"])
        );
    }

    #[test]
    fn buffer_record_includes_timing_flags_and_custom_meta_fields() {
        gst::init().expect("GStreamer should initialize");
        CustomMeta::register("JsonTapTestMeta", &[]);

        let mut buf = gst::Buffer::with_size(8).expect("buffer allocates");
        {
            let buf = buf.get_mut().expect("writable");
            buf.set_pts(gst::ClockTime::from_mseconds(40));
            buf.set_flags(gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::DISCONT);
            let mut meta = CustomMeta::add(buf, "JsonTapTestMeta").expect("meta adds");
            meta.mut_structure().set("index", 7u64);
            meta.mut_structure().set("confidence", 0.5f32);
            meta.mut_structure().set("name", "x");
            meta.mut_structure().set("ratio", gst::Fraction::new(30, 1));
            meta.mut_structure()
                .set("events", gst::Array::new(["{\"a\":1}", "{\"b\":2}"]));
            meta.mut_structure().set(
                "nested",
                gst::Structure::builder("n")
                    .field("depth", 2i32)
                    .field("tags", gst::List::new(["x", "y"]))
                    .build(),
            );
        }

        let record = buffer(3, &buf);
        assert_eq!(record["seq"], 3);
        assert_eq!(record["pts"], 40_000_000u64);
        assert_eq!(record["dts"], Value::Null);
        assert_eq!(record["offset"], Value::Null);
        assert_eq!(record["size"], 8);
        assert_eq!(record["flags"], json!(["discont", "delta-unit"]));
        assert_eq!(
            record["metas"],
            json!([{"api": "JsonTapTestMeta", "fields": {
                "index": 7,
                "confidence": 0.5,
                "name": "x",
                "ratio": "30/1",
                "events": ["{\"a\":1}", "{\"b\":2}"],
                "nested": {"depth": 2, "tags": ["x", "y"]},
            }}])
        );
    }
}
