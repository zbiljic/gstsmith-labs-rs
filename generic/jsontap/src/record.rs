//! Conversion of buffers, caps, and metas to JSON records.

use gst::prelude::*;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value, json};

fn clock_time(time: Option<gst::ClockTime>) -> Option<u64> {
    time.map(gst::ClockTime::nseconds)
}

fn offset(value: u64) -> Option<u64> {
    (value != u64::MAX).then_some(value)
}

struct Flags(gst::BufferFlags);

impl Serialize for Flags {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
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
        serializer.collect_seq(
            names
                .into_iter()
                .filter_map(|(flag, name)| self.0.contains(flag).then_some(name)),
        )
    }
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
    } else if let Ok(v) = value.get::<&str>() {
        json!(v)
    } else if let Ok(v) = value.get::<gst::ArrayRef<'_>>() {
        Value::Array(v.iter().map(self::value).collect())
    } else if let Ok(v) = value.get::<gst::ListRef<'_>>() {
        Value::Array(v.iter().map(self::value).collect())
    } else if let Ok(v) = value.get::<&gst::StructureRef>() {
        structure(v)
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

#[derive(Serialize)]
pub(crate) struct BufferRecord {
    dts: Option<u64>,
    duration: Option<u64>,
    flags: Flags,
    kind: &'static str,
    metas: Value,
    offset: Option<u64>,
    offset_end: Option<u64>,
    pts: Option<u64>,
    pub(crate) seq: u64,
    size: usize,
}

pub(crate) fn buffer(buffer: &gst::BufferRef) -> BufferRecord {
    BufferRecord {
        dts: clock_time(buffer.dts()),
        duration: clock_time(buffer.duration()),
        flags: Flags(buffer.flags()),
        kind: "buffer",
        metas: metas(buffer),
        offset: offset(buffer.offset()),
        offset_end: offset(buffer.offset_end()),
        pts: clock_time(buffer.pts()),
        seq: 0,
        size: buffer.size(),
    }
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

    fn buffer(seq: u64, buffer: &gst::BufferRef) -> Value {
        let mut record = super::buffer(buffer);
        record.seq = seq;
        serde_json::to_value(record).expect("serializable buffer record")
    }

    #[test]
    fn buffer_record_preserves_wire_format_and_numeric_limits() {
        gst::init().expect("GStreamer should initialize");
        CustomMeta::register("JsonTapCompatibilityMeta", &[]);
        let mut buf = gst::Buffer::new();
        {
            let buf = buf.get_mut().expect("writable");
            buf.set_dts(gst::ClockTime::from_nseconds(u64::MAX - 1));
            buf.set_offset_end(u64::MAX - 1);
            buf.set_flags(gst::BufferFlags::LIVE | gst::BufferFlags::DELTA_UNIT);
            let mut meta = CustomMeta::add(buf, "JsonTapCompatibilityMeta").expect("meta adds");
            let fields = meta.mut_structure();
            fields.set("name", "quoted \"name\"\nbackslash: \\ Unicode: ž");
            fields.set("enabled", true);
            fields.set("signed", i32::MIN);
            fields.set("unsigned", u32::MAX);
            fields.set("long", i64::MIN);
            fields.set("ulong", u64::MAX);
            fields.set("small_float", 1.25f32);
            fields.set("float", 1.25f64);
            fields.set("non_finite", f64::NAN);
            fields.set("empty", gst::Array::new(std::iter::empty::<u64>()));
        }
        let expected = json!({
            "kind": "buffer",
            "seq": u64::MAX,
            "pts": null,
            "dts": u64::MAX - 1,
            "duration": null,
            "offset": null,
            "offset_end": u64::MAX - 1,
            "flags": ["live", "delta-unit"],
            "size": 0,
            "metas": [{"api": "JsonTapCompatibilityMeta", "fields": {
                "name": "quoted \"name\"\nbackslash: \\ Unicode: ž",
                "enabled": true,
                "signed": i32::MIN,
                "unsigned": u32::MAX,
                "long": i64::MIN,
                "ulong": u64::MAX,
                "small_float": 1.25,
                "float": 1.25,
                "non_finite": null,
                "empty": [],
            }}],
        });
        let mut record = super::buffer(&buf);
        record.seq = u64::MAX;
        assert_eq!(
            serde_json::to_string(&record).expect("JSON record"),
            serde_json::to_string(&expected).expect("original JSON wire format"),
        );
    }

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
