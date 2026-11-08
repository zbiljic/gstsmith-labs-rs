//! What to draw for one frame, read from standard Mtds and `x-*` documents
//! of any producer. Cactus-specific documents are never read.

use analytics_meta::{schema, segmentation};
use gst_analytics::prelude::*;
use gst_analytics::{AnalyticsODMtd, AnalyticsRelationMeta, AnalyticsTrackingMtd, RelTypes};

use super::layout::Rect;

/// The label Mirage-style producers give recognized text lines.
pub const TEXT_LINE: &str = "text-line";

#[derive(Clone, Debug, PartialEq)]
pub struct Object {
    pub id: u32,
    pub label: Option<String>,
    pub score: f32,
    /// The box in frame pixels.
    pub rect: Rect,
    pub track: Option<u64>,
    /// Outline rings in frame pixels.
    pub polygons: Vec<Vec<(f64, f64)>>,
    /// Recognized text from `x-text`.
    pub text: Option<String>,
}

impl Object {
    /// A text line carries recognized text instead of being an object.
    #[must_use]
    pub fn is_text_line(&self) -> bool {
        self.text.is_some() || self.label.as_deref() == Some(TEXT_LINE)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arrow {
    /// Indices into `Scene::objects`.
    pub subject: usize,
    pub object: usize,
    pub predicate: String,
}

/// A binary or labelled mask drawn over `rect`.
#[derive(Clone, Debug, PartialEq)]
pub struct Mask {
    pub rect: Rect,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub pixels: Vec<u8>,
    /// Index into `Scene::objects` of the object it belongs to, if any.
    pub object: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    pub objects: Vec<Object>,
    pub arrows: Vec<Arrow>,
    pub masks: Vec<Mask>,
    pub caption: Option<String>,
}

impl Scene {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
            && self.arrows.is_empty()
            && self.masks.is_empty()
            && self.caption.is_none()
    }
}

fn related(meta: &AnalyticsRelationMeta, a: u32, b: u32) -> bool {
    meta.relation(a, b) != RelTypes::NONE || meta.relation(b, a) != RelTypes::NONE
}

fn read_mask(
    meta: &gst::MetaRef<'_, AnalyticsRelationMeta>,
    id: u32,
    objects: &[Object],
) -> Option<Mask> {
    let info = segmentation::info(meta, id)?;
    let buffer = segmentation::mask(meta, id)?;
    let video = buffer.meta::<gst_video::VideoMeta>()?;
    if video.format() != gst_video::VideoFormat::Gray8 {
        return None;
    }
    let width = usize::try_from(video.width()).ok()?;
    let height = usize::try_from(video.height()).ok()?;
    let stride = usize::try_from(*video.stride().first()?).ok()?;
    let offset = *video.offset().first()?;
    let map = buffer.map_readable().ok()?;
    let end = offset.checked_add(stride.checked_mul(height)?)?;
    let pixels = map.as_slice().get(offset..end)?.to_vec();
    Some(Mask {
        rect: Rect {
            x: f64::from(info.x),
            y: f64::from(info.y),
            w: f64::from(info.width),
            h: f64::from(info.height),
        },
        width,
        height,
        stride,
        pixels,
        object: objects
            .iter()
            .position(|object| related(meta, id, object.id)),
    })
}

/// Reads the scene of a `width` × `height` frame.
#[must_use]
pub fn read(meta: &gst::MetaRef<'_, AnalyticsRelationMeta>, width: u32, height: u32) -> Scene {
    let (fw, fh) = (f64::from(width), f64::from(height));
    let tracks: Vec<(u32, u64)> = meta
        .iter::<AnalyticsTrackingMtd>()
        .map(|track| (track.id(), track.info().0))
        .collect();
    let mut objects = Vec::new();
    for od in meta.iter::<AnalyticsODMtd>() {
        let Ok(location) = od.location() else {
            continue;
        };
        let id = od.id();
        let polygons = schema::polygons(meta, id)
            .into_iter()
            .flat_map(|polygon| polygon.rings)
            .map(|ring| {
                ring.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|[x, y]| (f64::from(*x) * fw, f64::from(*y) * fh))
                    .collect()
            })
            .collect();
        objects.push(Object {
            id,
            label: od.obj_type().map(|quark| quark.as_str().to_string()),
            score: od.confidence_level(),
            rect: Rect {
                x: f64::from(location.x),
                y: f64::from(location.y),
                w: f64::from(location.w),
                h: f64::from(location.h),
            },
            track: tracks
                .iter()
                .find(|(track, _)| related(meta, id, *track))
                .map(|(_, track_id)| *track_id),
            polygons,
            text: schema::text(meta, id).map(|text| text.string),
        });
    }
    let index = |id: u32| objects.iter().position(|object| object.id == id);
    let arrows = schema::relations(meta)
        .into_iter()
        .filter_map(|edge| {
            Some(Arrow {
                subject: index(edge.subject)?,
                object: index(edge.object)?,
                predicate: edge.predicate.replace('_', " "),
            })
        })
        .collect();
    let masks = meta
        .iter::<segmentation::SegmentationMtd>()
        .filter_map(|mask| read_mask(meta, mask.id(), &objects))
        .collect();
    Scene {
        objects,
        arrows,
        masks,
        caption: schema::caption(meta).map(|caption| caption.text),
    }
}

#[cfg(test)]
mod tests {
    use gst::glib;

    use super::*;

    #[test]
    fn reads_objects_documents_tracks_masks_and_arrows() {
        gst::init().expect("GStreamer initializes");
        let mut buffer = gst::Buffer::new();
        {
            let mut meta = analytics_meta::relation_meta_mut(buffer.make_mut()).expect("meta");
            let man = meta
                .add_od_mtd(glib::Quark::from_str("man"), 10, 20, 30, 40, 0.7)
                .expect("od")
                .id();
            let cup = meta
                .add_od_mtd(glib::Quark::from_str("cup"), 50, 60, 5, 5, 0.6)
                .expect("od")
                .id();
            let line = meta
                .add_od_mtd(glib::Quark::from_str(TEXT_LINE), 0, 0, 20, 5, 0.9)
                .expect("od")
                .id();
            schema::add_polygon(
                &mut meta,
                man,
                &schema::Polygon {
                    rings: vec![vec![0.0, 0.0, 0.5, 0.0, 0.5, 0.25]],
                },
            )
            .expect("polygon");
            schema::add_relation(&mut meta, man, cup, "holding_up").expect("relation");
            schema::add_text(
                &mut meta,
                line,
                &schema::Text {
                    string: "EXIT".to_owned(),
                    score: None,
                },
            )
            .expect("text");
            schema::add_caption(&mut meta, "a man").expect("caption");
            let track = meta
                .add_tracking_mtd(9, gst::ClockTime::ZERO)
                .expect("track")
                .id();
            meta.set_relation(RelTypes::RELATE_TO, man, track)
                .expect("relation");
            let mask = segmentation::add(&mut meta, vec![0, 1, 1, 0], 2, 2, false).expect("mask");
            meta.set_relation(RelTypes::RELATE_TO, cup, mask)
                .expect("relation");
        }
        let meta = buffer.meta::<AnalyticsRelationMeta>().expect("meta");
        let scene = read(&meta, 200, 100);
        let [man, cup, line] = scene.objects.as_slice() else {
            panic!("objects: {:?}", scene.objects);
        };
        assert_eq!(man.label.as_deref(), Some("man"));
        assert_eq!(
            man.rect,
            Rect {
                x: 10.0,
                y: 20.0,
                w: 30.0,
                h: 40.0
            }
        );
        assert_eq!(man.track, Some(9));
        assert_eq!(
            man.polygons,
            [vec![(0.0, 0.0), (100.0, 0.0), (100.0, 25.0)]]
        );
        assert!(!man.is_text_line() && line.is_text_line());
        assert_eq!(line.text.as_deref(), Some("EXIT"));
        assert_eq!(cup.track, None);
        assert_eq!(
            scene.arrows,
            [Arrow {
                subject: 0,
                object: 1,
                predicate: "holding up".to_owned()
            }]
        );
        let [mask] = scene.masks.as_slice() else {
            panic!("masks: {:?}", scene.masks);
        };
        assert_eq!((mask.width, mask.height, mask.object), (2, 2, Some(1)));
        assert_eq!(mask.pixels, [0, 1, 1, 0]);
        assert_eq!(scene.caption.as_deref(), Some("a man"));
    }
}
