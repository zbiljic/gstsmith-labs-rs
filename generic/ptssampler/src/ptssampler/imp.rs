use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

const DEFAULT_PERIOD: u64 = 1_000_000_000;
const DEFAULT_PHASE: u64 = 0;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "ptssampler",
        gst::DebugColorFlags::empty(),
        Some("Running-time period sampler"),
    )
});

#[derive(Debug, Clone, Copy)]
struct Settings {
    period: u64,
    phase: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            period: DEFAULT_PERIOD,
            phase: DEFAULT_PHASE,
        }
    }
}

#[derive(Default)]
pub struct PtsSampler {
    settings: Mutex<Settings>,
    /// Period index of the last buffer passed downstream in this segment.
    last_window: Mutex<Option<i128>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Index of the period containing `running_time`, counted from `phase`.
fn window(running_time: u64, settings: Settings) -> Option<i128> {
    if settings.period == 0 {
        return None;
    }
    let offset = i128::from(running_time) - i128::from(settings.phase);
    Some(offset.div_euclid(i128::from(settings.period)))
}

impl PtsSampler {
    fn reset(&self) {
        *lock(&self.last_window) = None;
    }

    fn running_time(&self, buffer: &gst::BufferRef) -> Option<gst::ClockTime> {
        let pts = buffer.pts()?;
        let segment = self.obj().segment();
        let segment = segment.downcast_ref::<gst::ClockTime>()?;
        segment.to_running_time(pts)
    }
}

#[glib::object_subclass]
impl ObjectSubclass for PtsSampler {
    const NAME: &'static str = "GstSmithPtsSampler";
    type Type = super::PtsSampler;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for PtsSampler {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt64::builder("period")
                    .nick("Period")
                    .blurb("Sampling period in nanoseconds of running time; 0 passes every buffer")
                    .default_value(DEFAULT_PERIOD)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("phase")
                    .nick("Phase")
                    .blurb("Running time in nanoseconds at which periods start")
                    .default_value(DEFAULT_PHASE)
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let mut settings = lock(&self.settings);
        match pspec.name() {
            "period" => settings.period = value.get().unwrap_or(DEFAULT_PERIOD),
            "phase" => settings.phase = value.get().unwrap_or(DEFAULT_PHASE),
            other => gst::error!(CAT, imp = self, "unknown property {other}"),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = lock(&self.settings);
        match pspec.name() {
            "period" => settings.period.to_value(),
            "phase" => settings.phase.to_value(),
            other => {
                gst::error!(CAT, imp = self, "unknown property {other}");
                glib::Value::from_type(pspec.value_type())
            }
        }
    }
}

impl GstObjectImpl for PtsSampler {}

impl ElementImpl for PtsSampler {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "PTS sampler",
                "Filter",
                "Passes the first buffer of every running-time period and drops the rest",
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

impl BaseTransformImpl for PtsSampler {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = true;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = true;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        self.reset();
        Ok(())
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if matches!(
            event.view(),
            gst::EventView::StreamStart(_)
                | gst::EventView::Segment(_)
                | gst::EventView::FlushStop(_)
        ) {
            self.reset();
        }
        self.parent_sink_event(event)
    }

    fn transform_ip_passthrough(
        &self,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = *lock(&self.settings);
        // Buffers without a running time cannot be placed in a period.
        let Some(window) = self
            .running_time(buffer)
            .and_then(|running_time| window(running_time.nseconds(), settings))
        else {
            return Ok(gst::FlowSuccess::Ok);
        };
        let mut last = lock(&self.last_window);
        if last.is_some_and(|last| window <= last) {
            return Ok(gst_base::BASE_TRANSFORM_FLOW_DROPPED);
        }
        *last = Some(window);
        Ok(gst::FlowSuccess::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(period_ms: u64, phase_ms: u64) -> Settings {
        Settings {
            period: period_ms * 1_000_000,
            phase: phase_ms * 1_000_000,
        }
    }

    #[test]
    fn windows_count_periods_from_the_phase() {
        let ms = 1_000_000;
        assert_eq!(window(0, settings(150, 0)), Some(0));
        assert_eq!(window(149 * ms, settings(150, 0)), Some(0));
        assert_eq!(window(150 * ms, settings(150, 0)), Some(1));
        assert_eq!(window(0, settings(150, 50)), Some(-1));
        assert_eq!(window(50 * ms, settings(150, 50)), Some(0));
        assert_eq!(window(123 * ms, settings(0, 0)), None);
    }
}
