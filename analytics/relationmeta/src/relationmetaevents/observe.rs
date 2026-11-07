//! What one analyzed frame shows, read from standard Mtds and `x-*`
//! documents.

use std::collections::{BTreeMap, BTreeSet};

use analytics_meta::schema;
use gst_analytics::prelude::*;
use gst_analytics::{
    AnalyticsClassificationMtd, AnalyticsODMtd, AnalyticsRelationMeta, AnalyticsTrackingMtd,
    RelTypes,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LabelSeen {
    pub count: u32,
    /// The best score among this label's objects.
    pub score: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackSeen {
    pub label: Option<String>,
    pub score: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClassSeen {
    pub label: String,
    pub score: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Observation {
    pub labels: BTreeMap<String, LabelSeen>,
    pub tracks: BTreeMap<u64, TrackSeen>,
    /// Frame-level classifications keyed by their sorted label set.
    pub classes: BTreeMap<String, ClassSeen>,
    /// The analyzed frame's PTS, from `x-provenance`.
    pub analyzed_pts: Option<u64>,
}

/// Which objects count: an optional label allowlist and a minimum score.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    pub labels: BTreeSet<String>,
    pub min_score: f32,
}

impl Filter {
    /// Parses a comma-separated allowlist; empty entries are ignored.
    pub fn parse_labels(list: &str) -> BTreeSet<String> {
        list.split(',')
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_owned)
            .collect()
    }

    fn accepts(&self, label: Option<&str>, score: f32) -> bool {
        score >= self.min_score
            && (self.labels.is_empty() || label.is_some_and(|label| self.labels.contains(label)))
    }
}

fn related(meta: &AnalyticsRelationMeta, a: u32, b: u32) -> bool {
    meta.relation(a, b) != RelTypes::NONE || meta.relation(b, a) != RelTypes::NONE
}

pub fn observe(meta: &gst::MetaRef<'_, AnalyticsRelationMeta>, filter: &Filter) -> Observation {
    let mut observation = Observation::default();
    let mut objects = Vec::new();
    for od in meta.iter::<AnalyticsODMtd>() {
        let label = od.obj_type().map(|quark| quark.as_str().to_string());
        let score = od.confidence_level();
        objects.push((od.id(), label.clone(), score));
        if let Some(label) = label
            && filter.accepts(Some(&label), score)
        {
            let seen = observation.labels.entry(label).or_insert(LabelSeen {
                count: 0,
                score: f32::MIN,
            });
            seen.count = seen.count.saturating_add(1);
            seen.score = seen.score.max(score);
        }
    }
    for track in meta.iter::<AnalyticsTrackingMtd>() {
        let (id, _first, _last, lost) = track.info();
        if lost {
            continue;
        }
        let object = objects
            .iter()
            .find(|(object, _, _)| related(meta, track.id(), *object));
        let (label, score) = object.map_or((None, None), |(_, label, score)| {
            (label.clone(), Some(*score))
        });
        if filter.accepts(label.as_deref(), score.unwrap_or(f32::MAX)) {
            observation.tracks.insert(id, TrackSeen { label, score });
        }
    }
    let count = u32::try_from(meta.len()).unwrap_or(u32::MAX);
    for cls in meta.iter::<AnalyticsClassificationMtd>() {
        let frame_level =
            (0..count).all(|other| other == cls.id() || !related(meta, cls.id(), other));
        let classes: Vec<(String, f32)> = cls
            .iterate()
            .map(|(quark, level)| (quark.as_str().to_string(), level))
            .collect();
        let Some((label, score)) = classes.iter().max_by(|a, b| a.1.total_cmp(&b.1)).cloned()
        else {
            continue;
        };
        if frame_level {
            let mut keys: Vec<&str> = classes.iter().map(|(label, _)| label.as_str()).collect();
            keys.sort_unstable();
            observation
                .classes
                .insert(keys.join("|"), ClassSeen { label, score });
        }
    }
    observation.analyzed_pts = schema::provenance(meta).and_then(|provenance| provenance.pts);
    observation
}

#[cfg(test)]
mod tests {
    use gst::glib;

    use super::*;

    #[test]
    fn observes_labels_tracks_classes_and_provenance() {
        gst::init().expect("GStreamer initializes");
        let mut buffer = gst::Buffer::new();
        {
            let mut meta = analytics_meta::relation_meta_mut(buffer.make_mut()).expect("meta");
            let man = meta
                .add_od_mtd(glib::Quark::from_str("man"), 0, 0, 9, 9, 0.7)
                .expect("od")
                .id();
            meta.add_od_mtd(glib::Quark::from_str("man"), 5, 5, 9, 9, 0.4)
                .expect("od");
            meta.add_od_mtd(glib::Quark::from_str("cup"), 5, 5, 2, 2, 0.1)
                .expect("od");
            let track = meta
                .add_tracking_mtd(3, gst::ClockTime::ZERO)
                .expect("track")
                .id();
            meta.set_relation(RelTypes::RELATE_TO, man, track)
                .expect("relation");
            meta.add_cls_mtd(
                &[0.3, 0.7],
                &[
                    glib::Quark::from_str("a=yes"),
                    glib::Quark::from_str("a=no"),
                ],
            )
            .expect("cls");
            let attached = meta
                .add_one_cls_mtd(0.9, glib::Quark::from_str("adult"))
                .expect("cls")
                .id();
            meta.set_relation(RelTypes::CONTAIN, man, attached)
                .expect("relation");
            schema::add_provenance(
                &mut meta,
                &schema::Provenance {
                    pts: Some(40),
                    running_time: None,
                    producer: "test".to_owned(),
                },
            )
            .expect("provenance");
        }
        let meta = buffer.meta::<AnalyticsRelationMeta>().expect("meta");
        let filter = Filter {
            labels: BTreeSet::new(),
            min_score: 0.2,
        };
        let observation = observe(&meta, &filter);
        assert_eq!(
            observation.labels,
            BTreeMap::from([(
                "man".to_owned(),
                LabelSeen {
                    count: 2,
                    score: 0.7
                }
            )])
        );
        assert_eq!(
            observation.tracks,
            BTreeMap::from([(
                3,
                TrackSeen {
                    label: Some("man".to_owned()),
                    score: Some(0.7)
                }
            )])
        );
        assert_eq!(
            observation.classes,
            BTreeMap::from([(
                "a=no|a=yes".to_owned(),
                ClassSeen {
                    label: "a=no".to_owned(),
                    score: 0.7
                }
            )])
        );
        assert_eq!(observation.analyzed_pts, Some(40));

        let only_cups = Filter {
            labels: Filter::parse_labels(" cup, ,"),
            min_score: 0.0,
        };
        let observation = observe(&meta, &only_cups);
        assert_eq!(observation.labels.keys().collect::<Vec<_>>(), ["cup"]);
        assert!(observation.tracks.is_empty(), "the man track is filtered");
    }
}
