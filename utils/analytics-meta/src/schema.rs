//! Generic, model-agnostic `x-*` documents for what `GStreamer` 1.28 has no
//! Mtd type for. Any producer can write them, and a consumer that reads only
//! these and the standard Mtds works with any producer.
//!
//! | Schema | Attached to | Meaning |
//! | --- | --- | --- |
//! | `x-polygon` | an `ODMtd`, `CONTAIN`/`IS_PART_OF` | outline rings, normalized 0..1 |
//! | `x-relation` | its own node, subject `RELATE_TO` node `RELATE_TO` object | a predicate |
//! | `x-caption` | the frame | a scene description |
//! | `x-text` | an `ODMtd`, `CONTAIN`/`IS_PART_OF` | recognized text |
//! | `x-provenance` | the frame | which frame a result describes |
//!
//! No schema stores Mtd ids: a transform into a fresh buffer renumbers them,
//! while relations are copied through the adjacency matrix. Frame documents
//! are found by schema, not by the absence of a parent.
//!
//! Writers add to existing relation cells instead of replacing them, because
//! upstream's `set_relation` assigns a cell. Per-object documents normally
//! hang under an `ODMtd`, but any Mtd may contain them. Readers are keyed by
//! the containing Mtd: a consumer iterates its objects and asks for their
//! documents, so there is no reverse index.

use gst::glib;
use gst_analytics::{AnalyticsAnyMtd, AnalyticsMetaRefExt as _, AnalyticsRelationMeta, RelTypes};
use serde::{Deserialize, Serialize};

use crate::json::{self, JsonMtd, JsonMtdExt as _};

pub const POLYGON: &str = "x-polygon";
pub const RELATION: &str = "x-relation";
pub const CAPTION: &str = "x-caption";
pub const TEXT: &str = "x-text";
pub const PROVENANCE: &str = "x-provenance";

type MetaMut<'a, 'b> = &'a mut gst::MetaRefMut<'b, AnalyticsRelationMeta, gst::meta::Standalone>;

/// Outline polygons of one object. Each ring is a flat `[x, y, x, y, ...]`
/// list in normalized frame coordinates (0..1), because a copied document is
/// not rescaled the way an `ODMtd` is.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Polygon {
    pub rings: Vec<Vec<f32>>,
}

/// The predicate of one relation, such as `holding`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub predicate: String,
}

/// A scene description for the frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caption {
    pub text: String,
}

/// Recognized text of one `text-line` object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Text {
    pub string: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

/// Which frame a result describes, when that is not the frame it is
/// attached to (for example an analysis that finished frames later).
/// Unknown times are omitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pts: Option<u64>,
    #[serde(
        default,
        rename = "running-time",
        skip_serializing_if = "Option::is_none"
    )]
    pub running_time: Option<u64>,
    pub producer: String,
}

/// A relation found through an `x-relation` node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelationEdge {
    pub subject: u32,
    pub object: u32,
    pub node: u32,
    pub predicate: String,
}

/// Checks ids before anything is written, so a failed call leaves no orphan
/// document behind.
fn check_ids(meta: &AnalyticsRelationMeta, ids: &[u32]) -> Result<(), glib::BoolError> {
    match ids
        .iter()
        .find(|&&id| usize::try_from(id).ok().is_none_or(|id| id >= meta.len()))
    {
        Some(id) => Err(glib::bool_error!("no Mtd with id {id}")),
        None => Ok(()),
    }
}

/// Adds `relation` to the cell `first → second`, keeping what is there.
fn relate(
    meta: MetaMut<'_, '_>,
    relation: RelTypes,
    first: u32,
    second: u32,
) -> Result<(), glib::BoolError> {
    let existing = meta.relation(first, second);
    meta.set_relation(existing | relation, first, second)
}

fn attach_to_object(meta: MetaMut<'_, '_>, object: u32, doc: u32) -> Result<(), glib::BoolError> {
    relate(meta, RelTypes::CONTAIN, object, doc)?;
    relate(meta, RelTypes::IS_PART_OF, doc, object)
}

/// How far outside 0..1 a polygon coordinate may stray from float rounding
/// before it is rejected instead of clamped.
pub const POLYGON_TOLERANCE: f32 = 1e-3;

/// Attaches an `x-polygon` under the Mtd `object`, normally an `ODMtd`.
/// Coordinates within [`POLYGON_TOLERANCE`] outside 0..1 are clamped.
pub fn add_polygon(
    meta: MetaMut<'_, '_>,
    object: u32,
    polygon: &Polygon,
) -> Result<u32, glib::BoolError> {
    let range = -POLYGON_TOLERANCE..=1.0 + POLYGON_TOLERANCE;
    let valid = !polygon.rings.is_empty()
        && polygon.rings.iter().all(|ring| {
            ring.len() >= 6
                && ring.len() % 2 == 0
                && ring.iter().all(|v| v.is_finite() && range.contains(v))
        });
    if !valid {
        return Err(glib::bool_error!(
            "x-polygon rings need at least 3 points in 0..1"
        ));
    }
    check_ids(meta, &[object])?;
    let clamped = Polygon {
        rings: polygon
            .rings
            .iter()
            .map(|ring| ring.iter().map(|v| v.clamp(0.0, 1.0)).collect())
            .collect(),
    };
    let doc = json::add_value(meta, POLYGON, &clamped)?;
    attach_to_object(meta, object, doc)?;
    Ok(doc)
}

/// Relates `subject` to `object` with `predicate`: a direct `RELATE_TO` for
/// standard consumers, plus an `x-relation` node linked subject → node →
/// object. Returns the node id.
pub fn add_relation(
    meta: MetaMut<'_, '_>,
    subject: u32,
    object: u32,
    predicate: &str,
) -> Result<u32, glib::BoolError> {
    if predicate.is_empty() {
        return Err(glib::bool_error!("x-relation needs a predicate"));
    }
    check_ids(meta, &[subject, object])?;
    // The node first: once it exists, setting edges cannot fail.
    let node = json::add_value(
        meta,
        RELATION,
        &Relation {
            predicate: predicate.to_owned(),
        },
    )?;
    relate(meta, RelTypes::RELATE_TO, subject, object)?;
    relate(meta, RelTypes::RELATE_TO, subject, node)?;
    relate(meta, RelTypes::RELATE_TO, node, object)?;
    Ok(node)
}

/// Attaches any document under the Mtd `object` with the per-object
/// convention (`CONTAIN` from the object, `IS_PART_OF` back), for
/// producer-specific schemas such as `example/detector/object`.
pub fn add_document_under<T: Serialize + ?Sized>(
    meta: MetaMut<'_, '_>,
    object: u32,
    schema: &str,
    value: &T,
) -> Result<u32, glib::BoolError> {
    check_ids(meta, &[object])?;
    let doc = json::add_value(meta, schema, value)?;
    attach_to_object(meta, object, doc)?;
    Ok(doc)
}

/// Attaches a frame `x-caption`.
pub fn add_caption(meta: MetaMut<'_, '_>, text: &str) -> Result<u32, glib::BoolError> {
    json::add_value(
        meta,
        CAPTION,
        &Caption {
            text: text.to_owned(),
        },
    )
}

/// Attaches an `x-text` under the Mtd `object`, normally an `ODMtd`
/// labelled `text-line`.
pub fn add_text(meta: MetaMut<'_, '_>, object: u32, text: &Text) -> Result<u32, glib::BoolError> {
    check_ids(meta, &[object])?;
    let doc = json::add_value(meta, TEXT, text)?;
    attach_to_object(meta, object, doc)?;
    Ok(doc)
}

/// Attaches a frame `x-provenance`.
pub fn add_provenance(
    meta: MetaMut<'_, '_>,
    provenance: &Provenance,
) -> Result<u32, glib::BoolError> {
    json::add_value(meta, PROVENANCE, provenance)
}

fn parse<T: for<'de> Deserialize<'de>>(json: &str) -> Option<T> {
    serde_json::from_str(json).ok()
}

/// Documents with `schema` that `object` contains.
fn contained<'a, T: for<'de> Deserialize<'de>>(
    meta: &'a gst::MetaRef<'a, AnalyticsRelationMeta>,
    object: u32,
    schema: &'a str,
) -> impl Iterator<Item = T> + 'a {
    meta.iter_direct_related::<JsonMtd>(object, RelTypes::CONTAIN)
        .filter_map(|mtd| mtd.document())
        .filter(move |doc| doc.schema == schema)
        .filter_map(|doc| parse(doc.json))
}

/// The `x-polygon` documents of `object`. Malformed documents are skipped.
#[must_use]
pub fn polygons<'a>(
    meta: &'a gst::MetaRef<'a, AnalyticsRelationMeta>,
    object: u32,
) -> Vec<Polygon> {
    contained(meta, object, POLYGON).collect()
}

/// The `x-text` of `object`, if any.
#[must_use]
pub fn text<'a>(meta: &'a gst::MetaRef<'a, AnalyticsRelationMeta>, object: u32) -> Option<Text> {
    // 1.30: also read upstream `TextMtd` here.
    contained(meta, object, TEXT).next()
}

/// The first frame `x-caption`.
#[must_use]
pub fn caption<'a>(meta: &'a gst::MetaRef<'a, AnalyticsRelationMeta>) -> Option<Caption> {
    json::documents_with_schema(meta, CAPTION).find_map(|doc| parse(doc.json))
}

/// The first frame `x-provenance`.
#[must_use]
pub fn provenance<'a>(meta: &'a gst::MetaRef<'a, AnalyticsRelationMeta>) -> Option<Provenance> {
    json::documents_with_schema(meta, PROVENANCE).find_map(|doc| parse(doc.json))
}

/// Every relation with a predicate. A node without exactly one subject and
/// one object is skipped.
#[must_use]
pub fn relations<'a>(meta: &'a gst::MetaRef<'a, AnalyticsRelationMeta>) -> Vec<RelationEdge> {
    let count = u32::try_from(meta.len()).unwrap_or(u32::MAX);
    json::documents_with_schema(meta, RELATION)
        .filter_map(|doc| {
            let relation: Relation = parse(doc.json)?;
            let node = doc.id;
            let mut subjects = (0..count)
                .filter(|&id| id != node && meta.relation(id, node).contains(RelTypes::RELATE_TO));
            let mut objects = meta
                .iter_direct_related::<AnalyticsAnyMtd>(node, RelTypes::RELATE_TO)
                .map(|mtd| mtd.id());
            let (Some(subject), None, Some(object), None) = (
                subjects.next(),
                subjects.next(),
                objects.next(),
                objects.next(),
            ) else {
                return None;
            };
            Some(RelationEdge {
                subject,
                object,
                node,
                predicate: relation.predicate,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gst_analytics::prelude::*;

    fn square() -> Polygon {
        Polygon {
            rings: vec![vec![0.1, 0.1, 0.5, 0.1, 0.5, 0.5, 0.1, 0.5]],
        }
    }

    #[test]
    fn documents_round_trip_with_their_relations() {
        gst::init().expect("GStreamer initializes");
        let mut buffer = gst::Buffer::new();
        let (man, cup, node) = {
            let mut meta = crate::relation_meta_mut(buffer.make_mut()).expect("meta");
            let man = meta
                .add_od_mtd(glib::Quark::from_str("man"), 10, 10, 50, 80, 0.7)
                .expect("od")
                .id();
            let cup = meta
                .add_od_mtd(glib::Quark::from_str("cup"), 40, 40, 10, 10, 0.6)
                .expect("od")
                .id();
            add_polygon(&mut meta, man, &square()).expect("polygon");
            let line = meta
                .add_od_mtd(glib::Quark::from_str("text-line"), 0, 0, 20, 5, 0.9)
                .expect("od")
                .id();
            let text = Text {
                string: "05:21:35".to_owned(),
                score: Some(0.75),
            };
            add_text(&mut meta, line, &text).expect("text");
            let node = add_relation(&mut meta, man, cup, "holding").expect("relation");
            add_caption(&mut meta, "a man holding a cup").expect("caption");
            add_provenance(
                &mut meta,
                &Provenance {
                    pts: Some(40),
                    running_time: None,
                    producer: "test".to_owned(),
                },
            )
            .expect("provenance");
            (man, cup, node)
        };
        let meta = buffer.meta::<AnalyticsRelationMeta>().expect("meta");
        assert_eq!(polygons(&meta, man), vec![square()]);
        assert_eq!(polygons(&meta, cup), Vec::<Polygon>::new());
        assert_eq!(
            text(&meta, 3).map(|t| t.string),
            Some("05:21:35".to_owned())
        );
        assert_eq!(text(&meta, man), None);
        assert_eq!(
            relations(&meta),
            vec![RelationEdge {
                subject: man,
                object: cup,
                node,
                predicate: "holding".to_owned(),
            }]
        );
        assert_eq!(meta.relation(man, cup), RelTypes::RELATE_TO);
        assert_eq!(
            caption(&meta).map(|c| c.text),
            Some("a man holding a cup".to_owned())
        );
        assert_eq!(provenance(&meta).and_then(|p| p.pts), Some(40));
        assert_eq!(
            json::documents_with_schema(&meta, PROVENANCE)
                .map(|doc| doc.json)
                .next(),
            Some(r#"{"pts":40,"producer":"test"}"#)
        );
    }

    #[test]
    fn writers_reject_invalid_documents() {
        gst::init().expect("GStreamer initializes");
        let mut buffer = gst::Buffer::new();
        let mut meta = crate::relation_meta_mut(buffer.make_mut()).expect("meta");
        let od = meta
            .add_od_mtd(glib::Quark::from_str("x"), 0, 0, 1, 1, 0.5)
            .expect("od")
            .id();
        for rings in [
            vec![],
            vec![vec![0.1, 0.1, 0.2, 0.2]],
            vec![vec![0.1, 0.1, 0.2, 0.2, 0.3]],
            vec![vec![0.1, 0.1, 0.2, 0.2, 0.3, 1.5]],
            vec![vec![0.1, 0.1, 0.2, 0.2, 0.3, f32::NAN]],
        ] {
            let polygon = Polygon { rings };
            assert!(
                add_polygon(&mut meta, od, &polygon).err().is_some(),
                "{polygon:?} accepted"
            );
        }
        assert!(add_relation(&mut meta, od, od, "").err().is_some());
        let tiny_overshoot = Polygon {
            rings: vec![vec![-0.0005, 0.1, 1.0005, 0.1, 0.5, 0.5]],
        };
        let doc = add_polygon(&mut meta, od, &tiny_overshoot).expect("clamped");
        assert!(add_polygon(&mut meta, 99, &square()).err().is_some());
        assert!(
            add_document_under(&mut meta, 99, "example/detector/object", &1)
                .err()
                .is_some()
        );
        assert!(add_relation(&mut meta, od, 99, "on").err().is_some());
        assert_eq!(meta.len(), 2, "a failed call left a document behind");
        let meta = buffer.meta::<AnalyticsRelationMeta>().expect("meta");
        assert_eq!(
            json::get(&meta, doc).map(|d| d.json),
            Some(r#"{"rings":[[0.0,0.1,1.0,0.1,0.5,0.5]]}"#)
        );
    }
}

#[cfg(test)]
mod relate_tests {
    use super::*;
    use gst_analytics::prelude::*;

    #[test]
    fn writers_keep_existing_edges_in_a_cell() {
        gst::init().expect("GStreamer initializes");
        let mut buffer = gst::Buffer::new();
        let mut meta = crate::relation_meta_mut(buffer.make_mut()).expect("meta");
        let group = meta
            .add_od_mtd(glib::Quark::from_str("group"), 0, 0, 9, 9, 0.5)
            .expect("od")
            .id();
        let member = meta
            .add_od_mtd(glib::Quark::from_str("man"), 1, 1, 2, 2, 0.5)
            .expect("od")
            .id();
        meta.set_relation(RelTypes::CONTAIN, group, member)
            .expect("producer edge");
        add_relation(&mut meta, group, member, "near").expect("relation");
        let meta = buffer.meta::<AnalyticsRelationMeta>().expect("meta");
        assert_eq!(
            meta.relation(group, member),
            RelTypes::CONTAIN | RelTypes::RELATE_TO
        );
        assert_eq!(relations(&meta).len(), 1);
    }
}
