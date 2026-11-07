//! A JSON record of a relation meta: every Mtd, its outgoing relations, and
//! the content of `JsonMtd` documents, keyed by Mtd id.
//!
//! ```json
//! {"mtds":[
//!   {"id":0,"type":"object-detection","label":"man","x":10,"y":10,"w":50,"h":80,
//!    "r":0.0,"confidence":0.7,"relations":[{"to":1,"type":"contain"}]},
//!   {"id":1,"type":"x-json","schema":"x-polygon","json":{"rings":[[0.1,0.1,0.5,0.1,0.5,0.5]]},
//!    "relations":[{"to":0,"type":"is-part-of"}]}
//! ]}
//! ```
//!
//! - `type` is upstream's impl name. Mtd types this crate does not know
//!   (such as 1.30's `text` or `group`) are recorded by name only.
//! - `relations` lists *outgoing* edges, `+`-joining combined flags.
//! - `json` is the document parsed and re-serialized: keys sorted and
//!   numbers normalized (`1e3` becomes `1000.0`), values unchanged. A
//!   document that is not valid JSON, which this crate never writes, is kept
//!   as a string under `text` instead.
//! - Floats held as `f32` are recorded at `f32` precision (`0.7`, not
//!   `0.699999988079071`).
//! - Segmentation masks are recorded by location, mask size and region ids,
//!   without pixels.

use std::ffi::CStr;

use gst_analytics::{
    AnalyticsAnyMtd, AnalyticsClassificationMtd, AnalyticsMetaRefExt as _, AnalyticsMtd as _,
    AnalyticsODMtd, AnalyticsRelationMeta, AnalyticsTrackingMtd, RelTypes, ffi,
};
use serde_json::{Map, Value, json};

use crate::json::{JsonMtd, JsonMtdExt as _};
use crate::segmentation::{self, SegmentationMtd};

#[expect(unsafe_code, reason = "reads the type name through the C API")]
fn type_name(mtd_type: ffi::GstAnalyticsMtdType) -> Option<String> {
    // SAFETY: the type comes from a live Mtd of the borrowed meta, so it is the
    // address of a registered impl whose name has static storage.
    let name = unsafe { ffi::gst_analytics_mtd_type_get_name(mtd_type) };
    if name.is_null() {
        return None;
    }
    // SAFETY: a non-null name is a NUL-terminated C string.
    let name = unsafe { CStr::from_ptr(name) };
    Some(name.to_string_lossy().into_owned())
}

/// An `f32` as JSON at `f32` precision: its shortest decimal form, widened.
#[must_use]
pub fn f32_value(value: f32) -> Value {
    value
        .to_string()
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

fn relation_name(relation: RelTypes) -> String {
    [
        (RelTypes::IS_PART_OF, "is-part-of"),
        (RelTypes::CONTAIN, "contain"),
        (RelTypes::RELATE_TO, "relate-to"),
        (RelTypes::N_TO_N, "n-to-n"),
    ]
    .into_iter()
    .filter(|(flag, _)| relation.contains(*flag))
    .map(|(_, name)| name)
    .collect::<Vec<_>>()
    .join("+")
}

/// The fields specific to one Mtd's type.
fn fields(
    meta: &gst::MetaRef<'_, AnalyticsRelationMeta>,
    id: u32,
    record: &mut Map<String, Value>,
) {
    let mtd_type = meta.mtd::<AnalyticsAnyMtd>(id).map(|mtd| mtd.mtd_type());
    if let Some(od) = meta.mtd::<AnalyticsODMtd>(id) {
        record.insert(
            "label".to_owned(),
            json!(od.obj_type().map(|quark| quark.as_str().to_string())),
        );
        if let Ok(location) = od.oriented_location() {
            record.extend([
                ("x".to_owned(), json!(location.x)),
                ("y".to_owned(), json!(location.y)),
                ("w".to_owned(), json!(location.w)),
                ("h".to_owned(), json!(location.h)),
                ("r".to_owned(), f32_value(location.r)),
                ("confidence".to_owned(), f32_value(location.loc_conf_lvl)),
            ]);
        }
    } else if let Some(cls) = meta.mtd::<AnalyticsClassificationMtd>(id) {
        let classes: Vec<_> = cls
            .iterate()
            .map(|(quark, level)| json!({"label": quark.as_str().as_str(), "confidence": f32_value(level)}))
            .collect();
        record.insert("classes".to_owned(), Value::Array(classes));
    } else if let Some(tracking) = meta.mtd::<AnalyticsTrackingMtd>(id) {
        let (track, first_seen, last_seen, lost) = tracking.info();
        record.extend([
            ("tracking-id".to_owned(), json!(track)),
            ("first-seen".to_owned(), json!(first_seen.nseconds())),
            ("last-seen".to_owned(), json!(last_seen.nseconds())),
            ("lost".to_owned(), json!(lost)),
        ]);
    } else if mtd_type == Some(SegmentationMtd::mtd_type()) {
        if let Some(info) = segmentation::info(meta, id) {
            record.extend([
                ("x".to_owned(), json!(info.x)),
                ("y".to_owned(), json!(info.y)),
                ("w".to_owned(), json!(info.width)),
                ("h".to_owned(), json!(info.height)),
                ("mask-size".to_owned(), json!(info.mask_size)),
                ("region-ids".to_owned(), json!(info.region_ids)),
            ]);
        }
    } else if mtd_type == Some(JsonMtd::mtd_type())
        && let Some(doc) = meta.mtd::<JsonMtd>(id).and_then(|mtd| mtd.document())
    {
        record.insert("schema".to_owned(), json!(doc.schema));
        match serde_json::from_str::<Value>(doc.json) {
            Ok(value) => record.insert("json".to_owned(), value),
            Err(_) => record.insert("text".to_owned(), json!(doc.json)),
        };
    }
}

/// Records every Mtd of `meta`, with relations and documents.
#[must_use]
pub fn relation_meta(meta: &gst::MetaRef<'_, AnalyticsRelationMeta>) -> Value {
    let count = u32::try_from(meta.len()).unwrap_or(u32::MAX);
    let mtds: Vec<Value> = (0..count)
        .filter_map(|id| {
            let mtd_type = meta.mtd::<AnalyticsAnyMtd>(id)?.mtd_type();
            let mut record = Map::new();
            record.insert("id".to_owned(), json!(id));
            record.insert("type".to_owned(), json!(type_name(mtd_type)));
            fields(meta, id, &mut record);
            let relations: Vec<Value> = (0..count)
                .filter(|&to| to != id)
                .filter_map(|to| {
                    let relation = meta.relation(id, to);
                    (relation != RelTypes::NONE)
                        .then(|| json!({"to": to, "type": relation_name(relation)}))
                })
                .collect();
            if !relations.is_empty() {
                record.insert("relations".to_owned(), Value::Array(relations));
            }
            Some(Value::Object(record))
        })
        .collect();
    json!({ "mtds": mtds })
}

#[cfg(test)]
mod tests {
    use gst::glib;
    use gst_analytics::prelude::*;

    use super::*;
    use crate::schema;

    #[test]
    fn records_every_mtd_with_relations_and_documents() {
        gst::init().expect("GStreamer initializes");
        let mut buffer = gst::Buffer::new();
        {
            let mut meta = crate::relation_meta_mut(buffer.make_mut()).expect("meta");
            let man = meta
                .add_od_mtd(glib::Quark::from_str("man"), 10, 10, 50, 80, 0.7)
                .expect("od")
                .id();
            let cls = meta
                .add_one_cls_mtd(0.25, glib::Quark::from_str("person_present=no"))
                .expect("cls")
                .id();
            let track = meta
                .add_tracking_mtd(7, gst::ClockTime::from_nseconds(40))
                .expect("tracking")
                .id();
            meta.set_relation(RelTypes::RELATE_TO, man, track)
                .expect("relation");
            let polygon = schema::Polygon {
                rings: vec![vec![0.5, 0.25, 1.0, 0.25, 1.0, 0.75]],
            };
            schema::add_polygon(&mut meta, man, &polygon).expect("polygon");
            crate::json::add(
                &mut meta,
                "example/detector/record",
                r#"{"scene":{"a":1e3}}"#,
            )
            .expect("document");
            segmentation::add(&mut meta, vec![0, 1], 2, 1, true).expect("mask");
            assert_eq!((man, cls, track), (0, 1, 2));
        }
        let meta = buffer.meta::<AnalyticsRelationMeta>().expect("meta");
        assert_eq!(
            relation_meta(&meta),
            json!({"mtds": [
                {"id": 0, "type": "object-detection", "label": "man", "x": 10, "y": 10,
                 "w": 50, "h": 80, "r": 0.0, "confidence": 0.7,
                 "relations": [{"to": 2, "type": "relate-to"}, {"to": 3, "type": "contain"}]},
                {"id": 1, "type": "classification",
                 "classes": [{"label": "person_present=no", "confidence": 0.25}]},
                {"id": 2, "type": "object-tracking", "tracking-id": 7, "first-seen": 40,
                 "last-seen": 40, "lost": false},
                {"id": 3, "type": "x-json", "schema": "x-polygon",
                 "json": {"rings": [[0.5, 0.25, 1.0, 0.25, 1.0, 0.75]]},
                 "relations": [{"to": 0, "type": "is-part-of"}]},
                {"id": 4, "type": "x-json", "schema": "example/detector/record",
                 "json": {"scene": {"a": 1000.0}}},
                {"id": 5, "type": "segmentation", "x": 0, "y": 0, "w": 2, "h": 1,
                 "mask-size": [2, 1], "region-ids": [0, 1]},
            ]})
        );
    }
}
