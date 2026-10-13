use std::num::NonZeroU64;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use atomic_refcell::{AtomicRefCell, AtomicRefMut};

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

struct Settings {
    period: AtomicU64,
    phase: AtomicU64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            period: AtomicU64::new(DEFAULT_PERIOD),
            phase: AtomicU64::new(DEFAULT_PHASE),
        }
    }
}

#[derive(Default)]
struct State {
    segment: Option<gst::FormattedSegment<gst::ClockTime>>,
    /// The next boundary may exceed the range of a u64 timestamp.
    next_boundary: Option<i128>,
}

#[derive(Default)]
pub struct PtsSampler {
    settings: Settings,
    state: AtomicRefCell<State>,
}

/// End of the period containing `running_time`, counted from `phase`.
fn next_boundary(running_time: u64, period: NonZeroU64, phase: u64) -> i128 {
    let period = i128::from(period.get());
    let offset = i128::from(running_time) - i128::from(phase);
    i128::from(running_time) + period - offset.rem_euclid(period)
}

impl PtsSampler {
    fn borrow_state(&self) -> Result<AtomicRefMut<'_, State>, gst::ErrorMessage> {
        self.state.try_borrow_mut().map_err(|err| {
            gst::error_msg!(
                gst::CoreError::Failed,
                ["Concurrent sampling state access: {err}"]
            )
        })
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
        match pspec.name() {
            "period" => self
                .settings
                .period
                .store(value.get().unwrap_or(DEFAULT_PERIOD), Ordering::Relaxed),
            "phase" => self
                .settings
                .phase
                .store(value.get().unwrap_or(DEFAULT_PHASE), Ordering::Relaxed),
            other => gst::error!(CAT, imp = self, "unknown property {other}"),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "period" => self.settings.period.load(Ordering::Relaxed).to_value(),
            "phase" => self.settings.phase.load(Ordering::Relaxed).to_value(),
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
        let obj = self.obj();
        let _guard = obj.sink_pad().stream_lock();
        *self.borrow_state()? = State::default();
        Ok(())
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if matches!(
            event.view(),
            gst::EventView::StreamStart(_)
                | gst::EventView::Segment(_)
                | gst::EventView::FlushStop(_)
        ) {
            // These events and buffers are serialized by the sink pad's stream lock.
            let mut state = match self.borrow_state() {
                Ok(state) => state,
                Err(err) => {
                    self.post_error_message(err);
                    return false;
                }
            };
            state.next_boundary = None;
            match event.view() {
                gst::EventView::Segment(segment) => {
                    state.segment = segment.segment().downcast_ref::<gst::ClockTime>().cloned();
                }
                gst::EventView::FlushStop(_) => state.segment = None,
                _ => (),
            }
        }
        self.parent_sink_event(event)
    }

    fn transform_ip_passthrough(
        &self,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let Some(period) = NonZeroU64::new(self.settings.period.load(Ordering::Relaxed)) else {
            return Ok(gst::FlowSuccess::Ok);
        };
        let Some(pts) = buffer.pts() else {
            return Ok(gst::FlowSuccess::Ok);
        };
        let obj = self.obj();
        let sink_pad = obj.sink_pad();
        // Pull-mode buffers hold the source pad's lock, unlike sink events.
        let _guard = (sink_pad.mode() != gst::PadMode::Push).then(|| sink_pad.stream_lock());
        let mut state = self.borrow_state().map_err(|err| {
            self.post_error_message(err);
            gst::FlowError::Error
        })?;
        // Buffers without a running time cannot be placed in a period.
        let Some(running_time) = state
            .segment
            .as_ref()
            .and_then(|segment| segment.to_running_time(pts))
        else {
            return Ok(gst::FlowSuccess::Ok);
        };
        let running_time = running_time.nseconds();
        if state
            .next_boundary
            .is_some_and(|next| i128::from(running_time) < next)
        {
            return Ok(gst_base::BASE_TRANSFORM_FLOW_DROPPED);
        }
        state.next_boundary = Some(next_boundary(
            running_time,
            period,
            self.settings.phase.load(Ordering::Relaxed),
        ));
        Ok(gst::FlowSuccess::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_match_period_windows_including_u64_limits() {
        for period in [1, 150, u64::MAX].into_iter().filter_map(NonZeroU64::new) {
            for phase in [0, 50, u64::MAX] {
                for time in [0, 49, 50, 149, 150, u64::MAX - 1, u64::MAX] {
                    let window =
                        (i128::from(time) - i128::from(phase)).div_euclid(i128::from(period.get()));
                    let boundary = next_boundary(time, period, phase);
                    assert_eq!(
                        boundary,
                        i128::from(phase) + (window + 1) * i128::from(period.get())
                    );
                    assert!(boundary > i128::from(time));
                    assert!(boundary <= i128::from(time) + i128::from(period.get()));
                }
            }
        }
    }
}
