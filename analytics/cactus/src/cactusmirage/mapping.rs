//! Mirage JSON to relation metadata.
//!
//! | Mirage | Metadata |
//! | --- | --- |
//! | `objects[]` | top-level `ODMtd` in frame pixels, `name` as label, `score` |
//! | object polygon `mask` | `x-polygon` under the `ODMtd`, divided by 1000 |
//! | object `id`, `description`, `position`, `point` | `cactus/mirage/object` under the `ODMtd` (`json-output=full`) |
//! | `relations[]` | `x-relation` node and the direct `RELATE_TO` subject → object |
//! | `text[]` | `ODMtd` labelled `text-line` plus `x-text` |
//! | `reasoning`, else `scene` | frame `x-caption` |
//! | `form` choice and integer fields | one frame-level `ClsMtd` per field, labels `field=value` |
//! | the whole response | frame `cactus/mirage/describe` with engine, weights and PTS |
//!
//! `regions` and `events` stay in the frame record only.
//!
//! Coordinates are on a 0–1000 scale of the analyzed image, which keeps the
//! frame's aspect ratio, so they map onto the frame directly.

use std::collections::HashMap;

use analytics_meta::{json, schema};
use gst::glib;
use gst_analytics::AnalyticsRelationMeta;
use gst_analytics::prelude::*;
use serde_json::{Map, Value, json};

use super::settings::JsonOutput;

pub const DESCRIBE_SCHEMA: &str = "cactus/mirage/describe";
pub const OBJECT_SCHEMA: &str = "cactus/mirage/object";
/// The label of recognized text lines.
pub const TEXT_LINE: &str = "text-line";

pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub json_output: JsonOutput,
    /// SHA-256 of the loaded weights.
    pub weights: &'a str,
    /// PTS of the analyzed frame.
    pub pts: Option<u64>,
}

/// What was written, for logging and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub objects: usize,
    pub text_lines: usize,
    pub relations: usize,
    pub classifications: usize,
    /// The size of a frame record that exceeded the document cap and was
    /// dropped.
    pub record_dropped: Option<usize>,
    /// Parts that could not be written, such as malformed polygons.
    pub skipped: Vec<String>,
}

type MetaMut<'a, 'b> = &'a mut gst::MetaRefMut<'b, AnalyticsRelationMeta, gst::meta::Standalone>;

#[expect(
    clippy::cast_possible_truncation,
    reason = "values are clamped to the frame, far inside i32"
)]
fn pixel(value: f64, size: u32) -> i32 {
    (value * f64::from(size) / 1000.0)
        .round()
        .clamp(0.0, f64::from(size)) as i32
}

/// A 0–1000 `[x0, y0, x1, y1]` box as frame pixels `(x, y, w, h)`.
fn rect(bbox: &Value, frame: &Frame<'_>) -> Option<(i32, i32, i32, i32)> {
    let [x0, y0, x1, y1] = bbox
        .as_array()?
        .iter()
        .map(Value::as_f64)
        .collect::<Option<Vec<f64>>>()?
        .try_into()
        .ok()?;
    let (left, top) = (
        pixel(x0.min(x1), frame.width),
        pixel(y0.min(y1), frame.height),
    );
    let (right, bottom) = (
        pixel(x0.max(x1), frame.width),
        pixel(y0.max(y1), frame.height),
    );
    Some((left, top, right - left, bottom - top))
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "scores and probabilities are in 0..1"
)]
fn as_f32(value: &Value) -> Option<f32> {
    value.as_f64().map(|value| value as f32)
}

/// Polygon rings from a mask: one flat ring or a list of them, 0–1000.
#[expect(
    clippy::cast_possible_truncation,
    reason = "normalized coordinates are in 0..1"
)]
fn rings(mask: &Value) -> Option<Vec<Vec<f32>>> {
    let list = mask.as_array()?;
    let ring = |values: &Vec<Value>| -> Option<Vec<f32>> {
        values
            .iter()
            .map(|value| value.as_f64().map(|v| (v / 1000.0) as f32))
            .collect()
    };
    if list.first().is_some_and(Value::is_number) {
        return Some(vec![ring(list)?]);
    }
    list.iter()
        .map(|item| item.as_array().and_then(ring))
        .collect()
}

/// The caption the demo shows: `reasoning` parts without `lang:`, else the
/// `scene` attributes as `key: value` lines.
fn caption(response: &Map<String, Value>) -> Option<String> {
    if let Some(reasoning) = response.get("reasoning").and_then(Value::as_str) {
        let parts: Vec<&str> = reasoning
            .split('|')
            .map(str::trim)
            .filter(|part| !part.is_empty() && !part.starts_with("lang:"))
            .collect();
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    let scene = response.get("scene")?.as_object()?;
    let lines: Vec<String> = scene
        .iter()
        .map(|(key, value)| match value {
            Value::String(text) => format!("{key}: {text}"),
            other => format!("{key}: {other}"),
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn error(err: &glib::BoolError) -> String {
    err.to_string()
}

fn write_objects(
    meta: MetaMut<'_, '_>,
    response: &Map<String, Value>,
    frame: &Frame<'_>,
    written: &mut Written,
) -> Result<HashMap<i64, u32>, String> {
    let mut ids = HashMap::new();
    for object in response
        .get("objects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("object");
        let Some((x, y, w, h)) = object.get("bbox").and_then(|bbox| rect(bbox, frame)) else {
            written.skipped.push(format!("object {name} without a box"));
            continue;
        };
        let score = object.get("score").and_then(as_f32).unwrap_or(-1.0);
        let od = meta
            .add_od_mtd(glib::Quark::from_str(name), x, y, w, h, score)
            .map_err(|err| error(&err))?
            .id();
        written.objects += 1;
        if let Some(id) = object.get("id").and_then(Value::as_i64) {
            ids.insert(id, od);
        }
        if let Some(rings) = object
            .get("mask")
            .and_then(rings)
            .filter(|rings| !rings.is_empty())
            && let Err(err) = schema::add_polygon(meta, od, &schema::Polygon { rings })
        {
            written.skipped.push(format!("polygon of {name}: {err}"));
        }
        if frame.json_output == JsonOutput::Full {
            let details: Map<String, Value> = ["id", "position", "description", "point"]
                .into_iter()
                .filter_map(|key| Some((key.to_owned(), object.get(key)?.clone())))
                .collect();
            schema::add_document_under(meta, od, OBJECT_SCHEMA, &details)
                .map_err(|err| error(&err))?;
        }
    }
    Ok(ids)
}

fn write_text(
    meta: MetaMut<'_, '_>,
    response: &Map<String, Value>,
    frame: &Frame<'_>,
    written: &mut Written,
) -> Result<(), String> {
    for line in response
        .get("text")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(string) = line.get("string").and_then(Value::as_str) else {
            written
                .skipped
                .push("text line without a string".to_owned());
            continue;
        };
        let Some((x, y, w, h)) = line.get("bbox").and_then(|bbox| rect(bbox, frame)) else {
            written
                .skipped
                .push(format!("text {string:?} without a box"));
            continue;
        };
        let score = line.get("score").and_then(as_f32);
        let od = meta
            .add_od_mtd(
                glib::Quark::from_str(TEXT_LINE),
                x,
                y,
                w,
                h,
                score.unwrap_or(-1.0),
            )
            .map_err(|err| error(&err))?
            .id();
        let text = schema::Text {
            string: string.to_owned(),
            score,
        };
        schema::add_text(meta, od, &text).map_err(|err| error(&err))?;
        written.text_lines += 1;
    }
    Ok(())
}

fn write_form(
    meta: MetaMut<'_, '_>,
    response: &Map<String, Value>,
    written: &mut Written,
) -> Result<(), String> {
    for (field, answer) in response
        .get("form")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        // Choice and integer fields carry a distribution; text fields do not.
        let Some(probabilities) = answer.get("probabilities").and_then(Value::as_object) else {
            continue;
        };
        let mut classes: Vec<(String, f32)> = probabilities
            .iter()
            .filter_map(|(value, probability)| {
                Some((format!("{field}={value}"), as_f32(probability)?))
            })
            .collect();
        if classes.is_empty() {
            continue;
        }
        classes.sort_by(|a, b| a.0.cmp(&b.0));
        let levels: Vec<f32> = classes.iter().map(|(_, level)| *level).collect();
        let quarks: Vec<glib::Quark> = classes
            .iter()
            .map(|(label, _)| glib::Quark::from_str(label))
            .collect();
        meta.add_cls_mtd(&levels, &quarks)
            .map_err(|err| error(&err))?;
        written.classifications += 1;
    }
    Ok(())
}

/// Writes one Mirage response onto a frame's relation meta.
pub fn write(meta: MetaMut<'_, '_>, response: &str, frame: &Frame<'_>) -> Result<Written, String> {
    let parsed: Value =
        serde_json::from_str(response).map_err(|err| format!("Mirage JSON: {err}"))?;
    let Value::Object(parsed) = parsed else {
        return Err("Mirage JSON is not an object".to_owned());
    };
    let mut written = Written::default();
    let ids = write_objects(meta, &parsed, frame, &mut written)?;
    for relation in parsed
        .get("relations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = |key: &str| {
            relation
                .get(key)
                .and_then(Value::as_i64)
                .and_then(|id| ids.get(&id))
        };
        let predicate = relation.get("predicate").and_then(Value::as_str);
        if let (Some(&subject), Some(&object), Some(predicate)) =
            (id("subject"), id("object"), predicate)
        {
            schema::add_relation(meta, subject, object, predicate).map_err(|err| error(&err))?;
            written.relations += 1;
        } else {
            written
                .skipped
                .push(format!("relation {relation} without both objects"));
        }
    }
    write_text(meta, &parsed, frame, &mut written)?;
    if let Some(caption) = caption(&parsed)
        && let Err(err) = schema::add_caption(meta, &caption)
    {
        written.skipped.push(format!("caption: {err}"));
    }
    write_form(meta, &parsed, &mut written)?;
    if frame.json_output != JsonOutput::None {
        // The response is embedded unchanged, so the record stays lossless.
        let record = format!(
            r#"{{"engine":{},"weights":{},"pts":{},"response":{}}}"#,
            json!(cactus::ENGINE_VERSION),
            json!(frame.weights),
            json!(frame.pts),
            response
        );
        if record.len() > json::MAX_JSON_BYTES {
            written.record_dropped = Some(record.len());
        } else {
            json::add(meta, DESCRIBE_SCHEMA, &record).map_err(|err| error(&err))?;
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use analytics_meta::json::JsonMtdExt as _;
    use gst_analytics::{AnalyticsClassificationMtd, AnalyticsODMtd, RelTypes};

    use super::*;

    const RESPONSE: &str = r#"{
        "scene": {"setting": "kitchen"},
        "objects": [
            {"id": 1, "name": "man", "bbox": [100, 200, 500, 1000], "score": 0.69,
             "mask": [[100, 200, 500, 200, 500, 1000]], "position": "left",
             "description": {"color": "blue"}},
            {"id": 2, "name": "cup", "bbox": [600, 500, 700, 600], "score": 0.8,
             "mask": [600, 500, 700, 500, 700, 600]},
            {"id": 3, "name": "ghost"}
        ],
        "text": [{"string": "EXIT", "bbox": [0, 0, 100, 50], "score": 0.7}],
        "relations": [
            {"subject": 1, "predicate": "holding", "object": 2},
            {"subject": 1, "predicate": "near", "object": 9}
        ],
        "reasoning": "lang: en | kitchen | a man holds a cup",
        "form": {
            "person_present": {"value": "yes", "probabilities": {"yes": 0.7, "no": 0.2, "-": 0.1}},
            "notes": {"value": "hi", "probability": 0.5}
        }
    }"#;

    fn frame(json_output: JsonOutput) -> Frame<'static> {
        Frame {
            width: 200,
            height: 100,
            json_output,
            weights: "abc",
            pts: Some(40),
        }
    }

    #[test]
    fn maps_every_section() {
        gst::init().expect("GStreamer initializes");
        let mut buffer = gst::Buffer::new();
        let written = {
            let mut meta = analytics_meta::relation_meta_mut(buffer.make_mut()).expect("meta");
            write(&mut meta, RESPONSE, &frame(JsonOutput::Full)).expect("write")
        };
        assert_eq!(
            (
                written.objects,
                written.text_lines,
                written.relations,
                written.classifications
            ),
            (2, 1, 1, 1)
        );
        assert_eq!(
            written.skipped,
            [
                "object ghost without a box",
                r#"relation {"object":9,"predicate":"near","subject":1} without both objects"#,
            ]
        );
        let meta = buffer.meta::<AnalyticsRelationMeta>().expect("meta");
        let objects: Vec<_> = meta
            .iter::<AnalyticsODMtd>()
            .map(|od| {
                let location = od.location().expect("location");
                (
                    od.obj_type().expect("label").as_str().to_string(),
                    (location.x, location.y, location.w, location.h),
                )
            })
            .collect();
        assert_eq!(
            objects,
            [
                ("man".to_owned(), (20, 20, 80, 80)),
                ("cup".to_owned(), (120, 50, 20, 10)),
                (TEXT_LINE.to_owned(), (0, 0, 20, 5)),
            ]
        );
        let man = meta.iter::<AnalyticsODMtd>().next().expect("man").id();
        assert_eq!(
            schema::polygons(&meta, man),
            [schema::Polygon {
                rings: vec![vec![0.1, 0.2, 0.5, 0.2, 0.5, 1.0]]
            }]
        );
        let edges = schema::relations(&meta);
        assert_eq!(edges.len(), 1);
        assert_eq!(meta.relation(man, edges[0].object), RelTypes::RELATE_TO);
        assert_eq!(
            schema::caption(&meta).map(|c| c.text).as_deref(),
            Some("kitchen\na man holds a cup")
        );
        let cls = meta
            .iter::<AnalyticsClassificationMtd>()
            .next()
            .expect("form field");
        let labels: Vec<String> = cls
            .iterate()
            .map(|(quark, _)| quark.as_str().to_string())
            .collect();
        assert_eq!(
            labels,
            [
                "person_present=-",
                "person_present=no",
                "person_present=yes"
            ]
        );
        let object_doc = meta
            .iter_direct_related::<analytics_meta::JsonMtd>(man, RelTypes::CONTAIN)
            .filter_map(|mtd| mtd.document())
            .find(|doc| doc.schema == OBJECT_SCHEMA)
            .expect("object document");
        assert_eq!(
            serde_json::from_str::<Value>(object_doc.json).expect("JSON"),
            json!({"id": 1, "position": "left", "description": {"color": "blue"}})
        );
        let record = json::documents_with_schema(&meta, DESCRIBE_SCHEMA)
            .next()
            .expect("frame record");
        let record: Value = serde_json::from_str(record.json).expect("JSON");
        assert_eq!(record["weights"], "abc");
        assert_eq!(record["pts"], 40);
        assert_eq!(record["response"]["objects"][0]["name"], "man");
    }

    #[test]
    fn json_output_controls_only_cactus_documents() {
        gst::init().expect("GStreamer initializes");
        for (output, objects, records) in [(JsonOutput::Frame, 0, 1), (JsonOutput::None, 0, 0)] {
            let mut buffer = gst::Buffer::new();
            {
                let mut meta = analytics_meta::relation_meta_mut(buffer.make_mut()).expect("meta");
                write(&mut meta, RESPONSE, &frame(output)).expect("write");
            }
            let meta = buffer.meta::<AnalyticsRelationMeta>().expect("meta");
            let count = |schema: &str| json::documents_with_schema(&meta, schema).count();
            assert_eq!(count(OBJECT_SCHEMA), objects, "{output:?}");
            assert_eq!(count(DESCRIBE_SCHEMA), records, "{output:?}");
            assert_eq!(count(schema::POLYGON), 2, "generic documents stay");
            assert_eq!(count(schema::CAPTION), 1);
        }
    }

    #[test]
    fn an_oversized_record_is_dropped() {
        gst::init().expect("GStreamer initializes");
        let big = format!(r#"{{"reasoning":"{}"}}"#, "a".repeat(json::MAX_JSON_BYTES));
        let mut buffer = gst::Buffer::new();
        let mut meta = analytics_meta::relation_meta_mut(buffer.make_mut()).expect("meta");
        let written = write(&mut meta, &big, &frame(JsonOutput::Full)).expect("write");
        assert!(
            written
                .record_dropped
                .is_some_and(|bytes| bytes > json::MAX_JSON_BYTES)
        );
        write(&mut meta, "[1]", &frame(JsonOutput::Full)).expect_err("not an object");
        write(&mut meta, "{", &frame(JsonOutput::Full)).expect_err("not JSON");
    }

    #[test]
    fn scene_is_the_caption_without_reasoning() {
        let parsed: Map<String, Value> =
            serde_json::from_str(r#"{"scene":{"a":"b","n":1},"reasoning":"lang: en"}"#)
                .expect("JSON");
        assert_eq!(caption(&parsed).as_deref(), Some("a: b\nn: 1"));
    }
}
