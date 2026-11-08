//! Records the relation meta of every buffer as one JSON line.
//!
//! ```json
//! {"pts":40000000,"running-time":40000000,"mtds":[{"id":0,"type":"object-detection",...}]}
//! ```
//!
//! `mtds` comes from `analytics_meta::record` and is `null` when the buffer
//! has no relation meta, so a frame nobody analyzed stays distinct from one
//! analyzed with nothing found (`[]`).

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_analytics::AnalyticsRelationMeta;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use serde::Serialize;
use serde_json::Value;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "relationmetatap",
        gst::DebugColorFlags::empty(),
        Some("Relation meta JSON lines tap"),
    )
});

#[derive(Serialize)]
struct Line {
    pts: Option<u64>,
    #[serde(rename = "running-time")]
    running_time: Option<u64>,
    mtds: Value,
}

#[derive(Debug, Default)]
struct Settings {
    location: Option<String>,
}

#[derive(Default)]
pub struct RelationMetaTap {
    settings: Mutex<Settings>,
    writer: Mutex<Option<BufWriter<File>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn write_error(err: &std::io::Error) -> gst::ErrorMessage {
    gst::error_msg!(gst::ResourceError::Write, ["writing records: {err}"])
}

impl RelationMetaTap {
    fn record(&self, buffer: &gst::BufferRef) -> Line {
        let pts = buffer.pts();
        let running_time = pts.and_then(|pts| {
            self.obj()
                .segment()
                .downcast_ref::<gst::ClockTime>()
                .and_then(|segment| segment.to_running_time(pts))
        });
        let mtds = buffer
            .meta::<AnalyticsRelationMeta>()
            .map(|meta| analytics_meta::record::relation_meta(&meta))
            .and_then(|record| match record {
                Value::Object(mut record) => record.remove("mtds"),
                _ => None,
            })
            .unwrap_or(Value::Null);
        Line {
            pts: pts.map(gst::ClockTime::nseconds),
            running_time: running_time.map(gst::ClockTime::nseconds),
            mtds,
        }
    }

    fn flush(&self) -> Result<(), gst::ErrorMessage> {
        match lock(&self.writer).as_mut() {
            Some(writer) => writer.flush().map_err(|err| write_error(&err)),
            None => Ok(()),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RelationMetaTap {
    const NAME: &'static str = "GstSmithRelationMetaTap";
    type Type = super::RelationMetaTap;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for RelationMetaTap {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("location")
                    .nick("Location")
                    .blurb("Path of the JSON lines file to write (truncated on start)")
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        if pspec.name() == "location" {
            lock(&self.settings).location = value.get::<Option<String>>().ok().flatten();
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "location" => lock(&self.settings).location.to_value(),
            other => {
                gst::error!(CAT, imp = self, "unknown property {other}");
                glib::Value::from_type(pspec.value_type())
            }
        }
    }
}

impl GstObjectImpl for RelationMetaTap {}

impl ElementImpl for RelationMetaTap {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Relation meta tap",
                "Generic/Debug",
                "Passes data through unchanged and records each buffer's analytics relation meta as JSON lines",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
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
        PAD_TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for RelationMetaTap {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = true;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let Some(location) = lock(&self.settings).location.clone() else {
            return Err(gst::error_msg!(
                gst::ResourceError::NotFound,
                ["the location property is required"]
            ));
        };
        let file = File::create(&location).map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::OpenWrite,
                ["creating {location}: {err}"]
            )
        })?;
        *lock(&self.writer) = Some(BufWriter::new(file));
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let result = self.flush();
        *lock(&self.writer) = None;
        result
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if let gst::EventView::Eos(_) = event.view()
            && let Err(err) = self.flush()
        {
            self.post_error_message(err);
        }
        self.parent_sink_event(event)
    }

    fn transform_ip_passthrough(
        &self,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let line = self.record(buffer);
        let result = {
            let mut writer = lock(&self.writer);
            let Some(writer) = writer.as_mut() else {
                return Err(gst::FlowError::Flushing);
            };
            // Blocking writes on the streaming thread, as in `jsontap`: this
            // is a development tool.
            serde_json::to_writer(&mut *writer, &line)
                .map_err(std::io::Error::from)
                .and_then(|()| writer.write_all(b"\n"))
        };
        result.map_err(|err| {
            self.post_error_message(write_error(&err));
            gst::FlowError::Error
        })?;
        Ok(gst::FlowSuccess::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tap_with_read_only_writer(
        capacity: usize,
    ) -> (
        super::super::RelationMetaTap,
        gst::Pipeline,
        tempfile::NamedTempFile,
    ) {
        gst::init().expect("GStreamer should initialize");
        let file = tempfile::NamedTempFile::new().expect("temporary file");
        let tap: super::super::RelationMetaTap = glib::Object::new();
        let pipeline = gst::Pipeline::new();
        pipeline.add(&tap).expect("add tap");
        *lock(&tap.imp().writer) = Some(BufWriter::with_capacity(
            capacity,
            File::open(file.path()).expect("read-only file"),
        ));
        (tap, pipeline, file)
    }

    fn assert_resource_write_error(pipeline: &gst::Pipeline) {
        let message = pipeline
            .bus()
            .expect("pipeline bus")
            .pop_filtered(&[gst::MessageType::Error])
            .expect("write failure is posted on the bus");
        let gst::MessageView::Error(error) = message.view() else {
            panic!("expected an error message");
        };
        assert_eq!(
            error.error().kind::<gst::ResourceError>(),
            Some(gst::ResourceError::Write)
        );
    }

    #[test]
    fn buffer_write_failure_posts_a_resource_error() {
        let (tap, pipeline, _file) = tap_with_read_only_writer(1);
        assert_eq!(
            tap.imp().transform_ip_passthrough(&gst::Buffer::new()),
            Err(gst::FlowError::Error),
        );
        assert_resource_write_error(&pipeline);
    }

    #[test]
    fn stop_reports_flush_failure_and_closes_the_writer() {
        let (tap, pipeline, _file) = tap_with_read_only_writer(8192);
        tap.imp()
            .transform_ip_passthrough(&gst::Buffer::new())
            .expect("record is buffered");
        let error = tap.imp().stop().expect_err("read-only file rejects flush");
        tap.imp().post_error_message(error);
        assert_resource_write_error(&pipeline);
        assert!(lock(&tap.imp().writer).is_none());
    }
}
