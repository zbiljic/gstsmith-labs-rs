use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_analytics::AnalyticsRelationMeta;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use super::observe::{self, Filter};
use super::state::{Change, Rules, Tracker};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "relationmetaevents",
        gst::DebugColorFlags::empty(),
        Some("Events from analytics relation metadata"),
    )
});

/// The event types `relationmetaevents` can emit.
#[glib::flags(name = "GstRelationMetaEventsKinds")]
pub enum EventKinds {
    #[flags_value(name = "A label became present", nick = "present")]
    PRESENT = 1 << 0,
    #[flags_value(name = "A label became absent", nick = "absent")]
    ABSENT = 1 << 1,
    #[flags_value(name = "A present label's count changed", nick = "count-changed")]
    COUNT_CHANGED = 1 << 2,
    #[flags_value(name = "A track entered", nick = "entered")]
    ENTERED = 1 << 3,
    #[flags_value(name = "A track left", nick = "left")]
    LEFT = 1 << 4,
    #[flags_value(name = "A frame classification changed", nick = "class-changed")]
    CLASS_CHANGED = 1 << 5,
}

const DEFAULT_STREAM_ID: &str = "stream";

#[derive(Clone, Debug)]
struct Settings {
    stream_id: String,
    labels: String,
    filter: Filter,
    rules: Rules,
    events: EventKinds,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            stream_id: DEFAULT_STREAM_ID.to_owned(),
            labels: String::new(),
            filter: Filter::default(),
            rules: Rules::default(),
            events: EventKinds::all(),
        }
    }
}

#[derive(Debug, Default)]
struct State {
    tracker: Tracker,
    /// The analyzed PTS of the last observation, to spot re-attached results.
    last_analyzed: Option<u64>,
}

#[derive(Default)]
pub struct RelationMetaEvents {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Change {
    fn kind(&self) -> (EventKinds, &'static str) {
        match self {
            Self::Present { .. } => (EventKinds::PRESENT, "present"),
            Self::Absent { .. } => (EventKinds::ABSENT, "absent"),
            Self::CountChanged { .. } => (EventKinds::COUNT_CHANGED, "count-changed"),
            Self::Entered { .. } => (EventKinds::ENTERED, "entered"),
            Self::Left { .. } => (EventKinds::LEFT, "left"),
            Self::ClassChanged { .. } => (EventKinds::CLASS_CHANGED, "class-changed"),
        }
    }

    fn fill(&self, builder: gst::structure::Builder) -> gst::structure::Builder {
        let builder = builder.field("type", self.kind().1);
        match self {
            Self::Present {
                label,
                count,
                score,
            }
            | Self::CountChanged {
                label,
                count,
                score,
            } => builder
                .field("label", label)
                .field("count", *count)
                .field("score", f64::from(*score)),
            Self::Absent { label } => builder.field("label", label).field("count", 0u32),
            Self::Entered {
                track,
                label,
                score,
            } => builder
                .field("track-id", *track)
                .field_if_some("label", label.clone())
                .field_if_some("score", score.map(f64::from)),
            Self::Left { track, label } => builder
                .field("track-id", *track)
                .field_if_some("label", label.clone()),
            Self::ClassChanged { label, score } => builder
                .field("label", label)
                .field("score", f64::from(*score)),
        }
    }
}

impl RelationMetaEvents {
    fn emit(
        &self,
        stream_id: &str,
        change: &Change,
        pts: Option<gst::ClockTime>,
        running_time: Option<gst::ClockTime>,
        analyzed_pts: Option<u64>,
    ) {
        let structure = change
            .fill(gst::Structure::builder("relationmeta-event").field("stream-id", stream_id))
            .field_if_some("pts", pts.map(gst::ClockTime::nseconds))
            .field_if_some("running-time", running_time.map(gst::ClockTime::nseconds))
            .field_if_some("analyzed-pts", analyzed_pts)
            .build();
        gst::debug!(CAT, imp = self, "{structure}");
        let element = self.obj();
        let message = gst::message::Element::builder(structure.clone())
            .src(&*element)
            .build();
        // Without a bus (no pipeline yet) there is nobody to tell.
        let _posted = element.post_message(message);
        let _forwarded = element
            .src_pad()
            .push_event(gst::event::CustomDownstream::new(structure));
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RelationMetaEvents {
    const NAME: &'static str = "GstSmithRelationMetaEvents";
    type Type = super::RelationMetaEvents;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for RelationMetaEvents {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            let time = |name: &'static str, blurb: &'static str| {
                glib::ParamSpecUInt64::builder(name)
                    .nick(name)
                    .blurb(blurb)
                    .default_value(0)
                    .mutable_ready()
                    .build()
            };
            let frames = |name: &'static str, blurb: &'static str| {
                glib::ParamSpecUInt::builder(name)
                    .nick(name)
                    .blurb(blurb)
                    .minimum(1)
                    .default_value(1)
                    .mutable_ready()
                    .build()
            };
            vec![
                glib::ParamSpecString::builder("stream-id")
                    .nick("Stream identifier")
                    .blurb("Identifier copied into every event")
                    .default_value(Some(DEFAULT_STREAM_ID))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecString::builder("labels")
                    .nick("Labels")
                    .blurb("Comma-separated labels to report; empty reports every label")
                    .default_value(Some(""))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecFloat::builder("min-score")
                    .nick("Minimum score")
                    .blurb("Objects scoring below this are ignored")
                    .minimum(0.0)
                    .maximum(1.0)
                    .default_value(0.0)
                    .mutable_ready()
                    .build(),
                frames(
                    "enter-min-frames",
                    "Consecutive analyzed frames before a label, count, track or class changes in",
                ),
                frames(
                    "leave-min-frames",
                    "Consecutive analyzed frames before a label or track is reported gone",
                ),
                time(
                    "enter-min-time",
                    "Running time in ns a change in must persist, alongside enter-min-frames",
                ),
                time(
                    "leave-min-time",
                    "Running time in ns an absence must persist, alongside leave-min-frames",
                ),
                glib::ParamSpecFlags::builder::<EventKinds>("events")
                    .nick("Events")
                    .blurb("Event types to emit")
                    .default_value(EventKinds::all())
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = lock(&self.settings);
        match pspec.name() {
            "stream-id" => {
                settings.stream_id = value
                    .get::<Option<String>>()
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| DEFAULT_STREAM_ID.to_owned());
            }
            "labels" => {
                settings.labels = value
                    .get::<Option<String>>()
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                settings.filter.labels = Filter::parse_labels(&settings.labels);
            }
            "min-score" => settings.filter.min_score = value.get().unwrap_or(0.0),
            "enter-min-frames" => settings.rules.enter.min_frames = value.get().unwrap_or(1),
            "leave-min-frames" => settings.rules.leave.min_frames = value.get().unwrap_or(1),
            "enter-min-time" => settings.rules.enter.min_time = value.get().unwrap_or(0),
            "leave-min-time" => settings.rules.leave.min_time = value.get().unwrap_or(0),
            "events" => settings.events = value.get().unwrap_or(EventKinds::all()),
            other => gst::error!(CAT, imp = self, "unknown property {other}"),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = lock(&self.settings);
        match pspec.name() {
            "stream-id" => settings.stream_id.to_value(),
            "labels" => settings.labels.to_value(),
            "min-score" => settings.filter.min_score.to_value(),
            "enter-min-frames" => settings.rules.enter.min_frames.to_value(),
            "leave-min-frames" => settings.rules.leave.min_frames.to_value(),
            "enter-min-time" => settings.rules.enter.min_time.to_value(),
            "leave-min-time" => settings.rules.leave.min_time.to_value(),
            "events" => settings.events.to_value(),
            other => {
                gst::error!(CAT, imp = self, "unknown property {other}");
                glib::Value::from_type(pspec.value_type())
            }
        }
    }
}

impl GstObjectImpl for RelationMetaEvents {}

impl ElementImpl for RelationMetaEvents {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Relation meta events",
                "Filter/Analyzer",
                "Emits presence, count, track and classification events from analytics relation metadata",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::new_any();
            [
                ("src", gst::PadDirection::Src),
                ("sink", gst::PadDirection::Sink),
            ]
            .into_iter()
            .filter_map(|(name, direction)| {
                gst::PadTemplate::new(name, direction, gst::PadPresence::Always, &caps).ok()
            })
            .collect()
        });
        TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for RelationMetaEvents {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = true;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        *lock(&self.state) = State::default();
        Ok(())
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        match event.view() {
            gst::EventView::StreamStart(_) | gst::EventView::FlushStop(_) => {
                *lock(&self.state) = State::default();
            }
            // Segments also arrive without a flush (concat, part changes):
            // keep presence, but running time may jump.
            gst::EventView::Segment(_) => lock(&self.state).tracker.restart_time(),
            _ => {}
        }
        self.parent_sink_event(event)
    }

    fn transform_ip_passthrough(
        &self,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Buffers without relation meta were not analyzed: no observation.
        let Some(meta) = buffer.meta::<AnalyticsRelationMeta>() else {
            return Ok(gst::FlowSuccess::Ok);
        };
        let settings = lock(&self.settings).clone();
        let observation = observe::observe(&meta, &settings.filter);
        let pts = buffer.pts();
        let running_time = self
            .obj()
            .segment()
            .downcast_ref::<gst::ClockTime>()
            .and_then(|segment| segment.to_running_time(pts));
        let changes = {
            let mut state = lock(&self.state);
            let repeat = observation.analyzed_pts.is_some()
                && observation.analyzed_pts == state.last_analyzed;
            state.last_analyzed = observation.analyzed_pts;
            state.tracker.observe(
                &observation,
                running_time.map(gst::ClockTime::nseconds),
                settings.rules,
                repeat,
            )
        };
        for change in changes
            .iter()
            .filter(|change| settings.events.contains(change.kind().0))
        {
            self.emit(
                &settings.stream_id,
                change,
                pts,
                running_time,
                observation.analyzed_pts,
            );
        }
        Ok(gst::FlowSuccess::Ok)
    }
}
