use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use crate::record;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "jsontap",
        gst::DebugColorFlags::empty(),
        Some("JSON lines tap"),
    )
});

#[derive(Debug, Default)]
struct Settings {
    location: Option<String>,
}

struct State {
    writer: BufWriter<File>,
    seq: u64,
}

impl State {
    fn write(&mut self, value: &impl serde::Serialize) -> Result<(), gst::ErrorMessage> {
        // shortcut: file writes can block streaming, use an async writer if live isolation is needed.
        serde_json::to_writer(&mut self.writer, value)
            .map_err(std::io::Error::from)
            .and_then(|()| self.writer.write_all(b"\n"))
            .map_err(|err| gst::error_msg!(gst::ResourceError::Write, ["writing record: {err}"]))
    }
}

#[derive(Default)]
pub struct JsonTap {
    settings: Mutex<Settings>,
    state: Mutex<Option<State>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl JsonTap {
    fn write(&self, value: &impl serde::Serialize) -> Result<(), gst::ErrorMessage> {
        let mut state = lock(&self.state);
        let Some(state) = state.as_mut() else {
            return Err(gst::error_msg!(
                gst::CoreError::StateChange,
                ["jsontap is not started"]
            ));
        };
        state.write(value)
    }

    fn flush(&self) -> Result<(), gst::ErrorMessage> {
        if let Some(state) = lock(&self.state).as_mut() {
            state.writer.flush().map_err(|err| {
                gst::error_msg!(gst::ResourceError::Write, ["flushing records: {err}"])
            })?;
        }
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for JsonTap {
    const NAME: &'static str = "GstSmithJsonTap";
    type Type = super::JsonTap;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for JsonTap {
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

impl GstObjectImpl for JsonTap {}

impl ElementImpl for JsonTap {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "JSON tap",
                "Generic/Debug",
                "Passes data through unchanged and records every buffer, caps change, and EOS as JSON lines",
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

impl BaseTransformImpl for JsonTap {
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
        *lock(&self.state) = Some(State {
            writer: BufWriter::new(file),
            seq: 0,
        });
        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        let result = self.flush();
        *lock(&self.state) = None;
        result
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        self.write(&record::caps(incaps)).map_err(|err| {
            let log_error = gst::loggable_error!(CAT, "{err:?}");
            self.post_error_message(err);
            log_error
        })?;
        self.parent_set_caps(incaps, outcaps)
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if let gst::EventView::Eos(_) = event.view()
            && let Err(err) = self.write(&record::eos()).and_then(|()| self.flush())
        {
            self.post_error_message(err);
        }
        self.parent_sink_event(event)
    }

    fn transform_ip_passthrough(
        &self,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // Inspect metas before locking: GStreamer value serializers can call plugin code.
        let mut record = record::buffer(buffer);
        let result = {
            let mut state = lock(&self.state);
            let Some(state) = state.as_mut() else {
                return Err(gst::FlowError::Flushing);
            };
            record.seq = state.seq;
            state.seq = state.seq.saturating_add(1);
            state.write(&record)
        };
        result.map_err(|err| {
            self.post_error_message(err);
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
        super::super::JsonTap,
        gst::Pipeline,
        tempfile::NamedTempFile,
    ) {
        gst::init().expect("GStreamer should initialize");
        let file = tempfile::NamedTempFile::new().expect("temporary file");
        let tap: super::super::JsonTap = glib::Object::new();
        let pipeline = gst::Pipeline::new();
        pipeline.add(&tap).expect("add tap");
        *lock(&tap.imp().state) = Some(State {
            writer: BufWriter::with_capacity(
                capacity,
                File::open(file.path()).expect("read-only file"),
            ),
            seq: 0,
        });
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
    fn caps_write_failure_posts_a_resource_error() {
        let (tap, pipeline, _file) = tap_with_read_only_writer(1);
        let caps = gst::Caps::builder("application/x-test").build();
        tap.imp()
            .set_caps(&caps, &caps)
            .expect_err("read-only file rejects caps record");
        assert_resource_write_error(&pipeline);
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
        assert!(lock(&tap.imp().state).is_none());
    }
}
