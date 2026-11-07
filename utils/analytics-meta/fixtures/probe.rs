//! Shared source of the identity fixture element, compiled into each fixture
//! plugin under its own element and type name.

use std::sync::Mutex;

use analytics_meta::{JsonMtd, json};
use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_analytics::{AnalyticsMtd as _, AnalyticsRelationMeta};
use gst_base::subclass::prelude::*;

glib::wrapper! {
    pub struct Probe(ObjectSubclass<imp::Probe>)
        @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        super::ELEMENT,
        gst::Rank::NONE,
        Probe::static_type(),
    )
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Probe {
        write_schema: Mutex<Option<String>>,
        seen: Mutex<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Probe {
        const NAME: &'static str = super::super::TYPE_NAME;
        type Type = super::Probe;
        type ParentType = gst_base::BaseTransform;
    }

    impl ObjectImpl for Probe {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: std::sync::LazyLock<Vec<glib::ParamSpec>> =
                std::sync::LazyLock::new(|| {
                    vec![
                        glib::ParamSpecString::builder("write-schema").build(),
                        glib::ParamSpecString::builder("seen").read_only().build(),
                        glib::ParamSpecUInt64::builder("mtd-type")
                            .read_only()
                            .build(),
                        glib::ParamSpecUInt64::builder("local-impl")
                            .read_only()
                            .build(),
                    ]
                });
            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            if pspec.name() == "write-schema" {
                *lock(&self.write_schema) = value.get().unwrap_or_default();
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "write-schema" => lock(&self.write_schema).to_value(),
                "seen" => lock(&self.seen).to_value(),
                "mtd-type" => (JsonMtd::mtd_type() as u64).to_value(),
                _ => (json::local_impl_address() as u64).to_value(),
            }
        }
    }

    impl GstObjectImpl for Probe {}

    impl ElementImpl for Probe {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: std::sync::LazyLock<gst::subclass::ElementMetadata> =
                std::sync::LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "JsonMtd identity probe",
                        "Testing",
                        "Writes and reads JsonMtd documents across plugins",
                        "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
                    )
                });
            Some(&*METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: std::sync::LazyLock<Vec<gst::PadTemplate>> =
                std::sync::LazyLock::new(|| {
                    let caps = gst::Caps::new_any();
                    [gst::PadDirection::Src, gst::PadDirection::Sink]
                        .into_iter()
                        .filter_map(|direction| {
                            let name = if direction == gst::PadDirection::Src {
                                "src"
                            } else {
                                "sink"
                            };
                            gst::PadTemplate::new(name, direction, gst::PadPresence::Always, &caps)
                                .ok()
                        })
                        .collect()
                });
            TEMPLATES.as_ref()
        }
    }

    impl BaseTransformImpl for Probe {
        const MODE: gst_base::subclass::BaseTransformMode =
            gst_base::subclass::BaseTransformMode::AlwaysInPlace;
        // Never passthrough: the probe must see (and may write to) every buffer.
        const PASSTHROUGH_ON_SAME_CAPS: bool = false;
        const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

        fn transform_ip(
            &self,
            buffer: &mut gst::BufferRef,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let schema = lock(&self.write_schema).clone();
            if let Some(schema) = schema {
                let exists = buffer.meta::<AnalyticsRelationMeta>().is_some();
                let mut meta = if exists {
                    buffer
                        .meta_mut::<AnalyticsRelationMeta>()
                        .ok_or(gst::FlowError::Error)?
                } else {
                    AnalyticsRelationMeta::add(buffer)
                };
                let body = format!(r#"{{"from":"{}"}}"#, super::super::ELEMENT);
                json::add(&mut meta, &schema, &body).map_err(|err| {
                    gst::element_imp_error!(self, gst::StreamError::Failed, ["{err}"]);
                    gst::FlowError::Error
                })?;
            }
            let seen = buffer
                .meta::<AnalyticsRelationMeta>()
                .map(|meta| {
                    json::documents(&meta)
                        .map(|doc| format!("{}={}", doc.schema, doc.json))
                        .collect::<Vec<_>>()
                        .join(";")
                })
                .unwrap_or_default();
            *lock(&self.seen) = seen;
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
