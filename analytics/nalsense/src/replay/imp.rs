use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use crate::codec::{self, Codec};

const DEFAULT_MAX_BUFFER_FRAMES: u32 = 300;
const DEFAULT_MAX_BUFFER_BYTES: u64 = 16 * 1024 * 1024;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "nalsensereplay",
        gst::DebugColorFlags::empty(),
        Some("Bounded H.264/H.265 IDR replay gate for NALSense wake events"),
    )
});

#[derive(Debug, Clone, Copy)]
struct Settings {
    max_buffer_frames: u32,
    max_buffer_bytes: u64,
    start_active: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_buffer_frames: DEFAULT_MAX_BUFFER_FRAMES,
            max_buffer_bytes: DEFAULT_MAX_BUFFER_BYTES,
            start_active: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Dormant,
    WakeRequested,
    Active,
    SleepPending,
}

struct ReplayBatch {
    buffers: VecDeque<gst::Buffer>,
    replayed_frames: u64,
    reason: &'static str,
}

enum BufferAction {
    Drop,
    Push(gst::Buffer),
    Replay(ReplayBatch),
}

struct State {
    mode: Mode,
    buffers: VecDeque<gst::Buffer>,
    buffered_bytes: u64,
    replayed_frames: u64,
    replayed_bytes: u64,
    forced_wakes: u64,
    peak_buffered_frames: u64,
    peak_buffered_bytes: u64,
    codec: Option<Codec>,
    flow_error: Option<gst::FlowError>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            mode: Mode::Dormant,
            buffers: VecDeque::new(),
            buffered_bytes: 0,
            replayed_frames: 0,
            replayed_bytes: 0,
            forced_wakes: 0,
            peak_buffered_frames: 0,
            peak_buffered_bytes: 0,
            codec: None,
            flow_error: None,
        }
    }
}

impl State {
    fn reset_run(&mut self, start_active: bool) {
        self.mode = if start_active {
            Mode::Active
        } else {
            Mode::Dormant
        };
        self.clear_stream();
        self.codec = None;
        self.replayed_frames = 0;
        self.replayed_bytes = 0;
        self.forced_wakes = 0;
        self.peak_buffered_frames = 0;
        self.peak_buffered_bytes = 0;
    }

    fn invalidate_stream(&mut self) {
        self.mode = match self.mode {
            Mode::SleepPending => Mode::Dormant,
            mode @ (Mode::Dormant | Mode::WakeRequested | Mode::Active) => mode,
        };
        self.clear_stream();
    }

    fn clear_stream(&mut self) {
        self.buffers.clear();
        self.buffered_bytes = 0;
        self.flow_error = None;
    }

    fn set_codec(&mut self, codec: Codec) {
        self.invalidate_stream();
        self.codec = Some(codec);
    }

    fn clear_codec(&mut self) {
        self.invalidate_stream();
        self.codec = None;
    }

    fn is_active(&self) -> bool {
        matches!(self.mode, Mode::Active | Mode::SleepPending)
    }

    fn request_wake(&mut self, serialized: bool) -> Option<ReplayBatch> {
        match self.mode {
            Mode::Active => None,
            Mode::SleepPending => {
                self.mode = Mode::Active;
                None
            }
            Mode::Dormant | Mode::WakeRequested if serialized && !self.buffers.is_empty() => {
                Some(self.take_replay("activity-event"))
            }
            Mode::Dormant | Mode::WakeRequested => {
                self.mode = Mode::WakeRequested;
                None
            }
        }
    }

    fn request_sleep(&mut self) {
        self.mode = match self.mode {
            Mode::Active => Mode::SleepPending,
            Mode::WakeRequested => Mode::Dormant,
            mode @ (Mode::Dormant | Mode::SleepPending) => mode,
        };
    }

    fn handle_buffer(
        &mut self,
        buffer: gst::Buffer,
        is_idr: bool,
        settings: Settings,
    ) -> BufferAction {
        match self.mode {
            Mode::Active => BufferAction::Push(buffer),
            Mode::SleepPending if !is_idr => BufferAction::Push(buffer),
            Mode::SleepPending if Self::idr_exceeds_limit(&buffer, settings) => {
                self.mode = Mode::Active;
                self.forced_wakes = self.forced_wakes.saturating_add(1);
                BufferAction::Replay(Self::single_buffer_wake(buffer, "buffer-limit"))
            }
            Mode::SleepPending => {
                self.mode = Mode::Dormant;
                self.replace_with_idr(buffer);
                BufferAction::Drop
            }
            Mode::WakeRequested if !self.buffers.is_empty() => {
                let mut batch = self.take_replay("action");
                batch.buffers.push_back(buffer);
                BufferAction::Replay(batch)
            }
            Mode::WakeRequested if is_idr => {
                self.mode = Mode::Active;
                BufferAction::Replay(Self::single_buffer_wake(buffer, "action-next-idr"))
            }
            Mode::WakeRequested => BufferAction::Drop,
            Mode::Dormant if is_idr && Self::idr_exceeds_limit(&buffer, settings) => {
                self.mode = Mode::Active;
                self.forced_wakes = self.forced_wakes.saturating_add(1);
                BufferAction::Replay(Self::single_buffer_wake(buffer, "buffer-limit"))
            }
            Mode::Dormant if is_idr => {
                self.replace_with_idr(buffer);
                BufferAction::Drop
            }
            Mode::Dormant if self.buffers.is_empty() => BufferAction::Drop,
            Mode::Dormant => {
                let buffer_bytes = u64::try_from(buffer.size()).unwrap_or(u64::MAX);
                let frame_limit = usize::try_from(settings.max_buffer_frames).unwrap_or(usize::MAX);
                let bytes_after = self.buffered_bytes.checked_add(buffer_bytes);
                let over_frames = self.buffers.len() >= frame_limit;
                let over_bytes = bytes_after.is_none_or(|bytes| bytes > settings.max_buffer_bytes);
                if over_frames || over_bytes {
                    self.forced_wakes = self.forced_wakes.saturating_add(1);
                    let mut batch = self.take_replay("buffer-limit");
                    batch.buffers.push_back(buffer);
                    BufferAction::Replay(batch)
                } else {
                    self.buffered_bytes = bytes_after.unwrap_or(settings.max_buffer_bytes);
                    self.buffers.push_back(buffer);
                    self.update_peaks();
                    BufferAction::Drop
                }
            }
        }
    }

    fn replace_with_idr(&mut self, buffer: gst::Buffer) {
        self.buffers.clear();
        self.buffered_bytes = u64::try_from(buffer.size()).unwrap_or(u64::MAX);
        self.buffers.push_back(buffer);
        self.update_peaks();
    }

    fn update_peaks(&mut self) {
        let buffered_frames = u64::try_from(self.buffers.len()).unwrap_or(u64::MAX);
        self.peak_buffered_frames = self.peak_buffered_frames.max(buffered_frames);
        self.peak_buffered_bytes = self.peak_buffered_bytes.max(self.buffered_bytes);
    }

    fn idr_exceeds_limit(buffer: &gst::Buffer, settings: Settings) -> bool {
        u64::try_from(buffer.size()).map_or(true, |bytes| bytes > settings.max_buffer_bytes)
    }

    fn single_buffer_wake(buffer: gst::Buffer, reason: &'static str) -> ReplayBatch {
        let mut buffers = VecDeque::new();
        buffers.push_back(buffer);
        ReplayBatch {
            buffers,
            replayed_frames: 0,
            reason,
        }
    }

    fn take_replay(&mut self, reason: &'static str) -> ReplayBatch {
        self.mode = Mode::Active;
        let buffers = std::mem::take(&mut self.buffers);
        let replayed_frames = u64::try_from(buffers.len()).unwrap_or(u64::MAX);
        self.buffered_bytes = 0;
        ReplayBatch {
            buffers,
            replayed_frames,
            reason,
        }
    }
}

pub struct NalSenseReplay {
    settings: Mutex<Settings>,
    state: Mutex<State>,
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
}

#[glib::object_subclass]
impl ObjectSubclass for NalSenseReplay {
    const NAME: &'static str = "GstSmithNalSenseReplay";
    type Type = super::NalSenseReplay;
    type ParentType = gst::Element;

    #[expect(
        clippy::expect_used,
        reason = "fixed class pad templates are installed before element construction"
    )]
    fn with_class(class: &Self::Class) -> Self {
        let sinkpad = gst::Pad::builder_from_template(
            &class
                .pad_template("sink")
                .expect("NALSense replay sink template"),
        )
        .chain_function(|pad, parent, buffer| {
            Self::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |replay| replay.sink_chain(pad, buffer),
            )
        })
        .event_function(|pad, parent, event| {
            Self::catch_panic_pad_function(parent, || false, |replay| replay.sink_event(pad, event))
        })
        .query_function(|pad, parent, query| {
            Self::catch_panic_pad_function(parent, || false, |replay| replay.sink_query(pad, query))
        })
        .flags(
            gst::PadFlags::PROXY_CAPS
                | gst::PadFlags::PROXY_ALLOCATION
                | gst::PadFlags::PROXY_SCHEDULING,
        )
        .build();

        let srcpad = gst::Pad::builder_from_template(
            &class
                .pad_template("src")
                .expect("NALSense replay source template"),
        )
        .event_function(|pad, parent, event| {
            Self::catch_panic_pad_function(parent, || false, |replay| replay.src_event(pad, event))
        })
        .query_function(|pad, parent, query| {
            Self::catch_panic_pad_function(parent, || false, |replay| replay.src_query(pad, query))
        })
        .flags(
            gst::PadFlags::PROXY_CAPS
                | gst::PadFlags::PROXY_ALLOCATION
                | gst::PadFlags::PROXY_SCHEDULING,
        )
        .build();

        Self {
            settings: Mutex::new(Settings::default()),
            state: Mutex::new(State::default()),
            srcpad,
            sinkpad,
        }
    }
}

impl ObjectImpl for NalSenseReplay {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("max-buffer-frames")
                    .nick("Maximum buffered frames")
                    .blurb("Maximum complete IDR-prefix access units retained while dormant")
                    .minimum(1)
                    .maximum(u32::MAX)
                    .default_value(DEFAULT_MAX_BUFFER_FRAMES)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("max-buffer-bytes")
                    .nick("Maximum buffered bytes")
                    .blurb("Maximum H.264 access-unit bytes retained while dormant")
                    .minimum(1)
                    .maximum(u64::MAX)
                    .default_value(DEFAULT_MAX_BUFFER_BYTES)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("start-active")
                    .nick("Start active")
                    .blurb("Forward immediately instead of waiting for a wake request")
                    .default_value(false)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("active")
                    .nick("Active")
                    .blurb("Whether buffers are currently forwarded downstream")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("buffered-frames")
                    .nick("Buffered frames")
                    .blurb("Access units currently retained from the latest IDR")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("buffered-bytes")
                    .nick("Buffered bytes")
                    .blurb("Bytes currently retained from the latest IDR")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("replayed-frames")
                    .nick("Replayed frames")
                    .blurb("Retained access units replayed during the current run")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("replayed-bytes")
                    .nick("Replayed bytes")
                    .blurb("Retained compressed bytes replayed during the current run")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("forced-wakes")
                    .nick("Forced wakes")
                    .blurb("Safety wakes caused by a configured buffer limit")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("peak-buffered-frames")
                    .nick("Peak buffered frames")
                    .blurb("Maximum retained access units during the current run")
                    .read_only()
                    .build(),
                glib::ParamSpecUInt64::builder("peak-buffered-bytes")
                    .nick("Peak buffered bytes")
                    .blurb("Maximum retained compressed bytes during the current run")
                    .read_only()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
            vec![
                glib::subclass::Signal::builder("wake")
                    .action()
                    .class_handler(|args| {
                        if let Some(element) = args
                            .first()
                            .and_then(|value| value.get::<super::NalSenseReplay>().ok())
                        {
                            element.imp().request_wake();
                        }
                        None
                    })
                    .build(),
                glib::subclass::Signal::builder("sleep")
                    .action()
                    .class_handler(|args| {
                        if let Some(element) = args
                            .first()
                            .and_then(|value| value.get::<super::NalSenseReplay>().ok())
                        {
                            element.imp().request_sleep();
                        }
                        None
                    })
                    .build(),
            ]
        });
        SIGNALS.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        let Ok(mut settings) = self.settings.lock() else {
            gst::error!(CAT, imp = self, "NALSense replay settings lock is poisoned");
            return;
        };
        match pspec.name() {
            "max-buffer-frames" => {
                if let Ok(frames) = value.get() {
                    settings.max_buffer_frames = frames;
                }
            }
            "max-buffer-bytes" => {
                if let Ok(bytes) = value.get() {
                    settings.max_buffer_bytes = bytes;
                }
            }
            "start-active" => {
                if let Ok(active) = value.get() {
                    settings.start_active = active;
                }
            }
            _ => gst::warning!(CAT, imp = self, "unexpected property {}", pspec.name()),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "max-buffer-frames" => self
                .settings
                .lock()
                .map_or(DEFAULT_MAX_BUFFER_FRAMES, |value| value.max_buffer_frames)
                .to_value(),
            "max-buffer-bytes" => self
                .settings
                .lock()
                .map_or(DEFAULT_MAX_BUFFER_BYTES, |value| value.max_buffer_bytes)
                .to_value(),
            "start-active" => self
                .settings
                .lock()
                .is_ok_and(|value| value.start_active)
                .to_value(),
            "active" => self
                .state
                .lock()
                .is_ok_and(|value| value.is_active())
                .to_value(),
            "buffered-frames" => self
                .state
                .lock()
                .map_or(0, |value| {
                    u64::try_from(value.buffers.len()).unwrap_or(u64::MAX)
                })
                .to_value(),
            "buffered-bytes" => self
                .state
                .lock()
                .map_or(0, |value| value.buffered_bytes)
                .to_value(),
            "replayed-frames" => self
                .state
                .lock()
                .map_or(0, |value| value.replayed_frames)
                .to_value(),
            "replayed-bytes" => self
                .state
                .lock()
                .map_or(0, |value| value.replayed_bytes)
                .to_value(),
            "forced-wakes" => self
                .state
                .lock()
                .map_or(0, |value| value.forced_wakes)
                .to_value(),
            "peak-buffered-frames" => self
                .state
                .lock()
                .map_or(0, |value| value.peak_buffered_frames)
                .to_value(),
            "peak-buffered-bytes" => self
                .state
                .lock()
                .map_or(0, |value| value.peak_buffered_bytes)
                .to_value(),
            _ => pspec.default_value().clone(),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "fixed pads are added once during construction and duplicate names are impossible"
    )]
    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        obj.add_pad(&self.sinkpad)
            .expect("adding NALSense replay sink pad");
        obj.add_pad(&self.srcpad)
            .expect("adding NALSense replay source pad");
    }
}

impl GstObjectImpl for NalSenseReplay {}

impl ElementImpl for NalSenseReplay {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "NALSense H.264/H.265 Replay Gate",
                "Filter/Analysis/Video",
                "Buffers from an IDR and replays a decodable H.264/H.265 prefix on activity wake",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&METADATA)
    }

    #[expect(clippy::expect_used, reason = "fixed static pad templates are valid")]
    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = codec::parsed_au_caps();
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .expect("static NALSense replay sink pad"),
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .expect("static NALSense replay source pad"),
            ]
        });
        TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::ReadyToPaused {
            self.reset_run_state()?;
        }
        let result = self.parent_change_state(transition)?;
        if transition == gst::StateChange::PausedToReady {
            self.reset_run_state()?;
        }
        Ok(result)
    }
}

impl NalSenseReplay {
    fn reset_run_state(&self) -> Result<(), gst::StateChangeError> {
        let start_active = self
            .settings
            .lock()
            .map_err(|_poisoned| gst::StateChangeError)?
            .start_active;
        self.state
            .lock()
            .map_err(|_poisoned| gst::StateChangeError)?
            .reset_run(start_active);
        Ok(())
    }

    fn invalidate_stream_state(&self, clear_codec: bool) -> Result<(), gst::StateChangeError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_poisoned| gst::StateChangeError)?;
        if clear_codec {
            state.clear_codec();
        } else {
            state.invalidate_stream();
        }
        Ok(())
    }

    fn set_codec(&self, codec: Codec) -> Result<(), gst::StateChangeError> {
        self.state
            .lock()
            .map_err(|_poisoned| gst::StateChangeError)?
            .set_codec(codec);
        Ok(())
    }

    fn request_wake(&self) {
        match self.state.lock() {
            Ok(mut state) => {
                let _batch_deferred_to_streaming_thread = state.request_wake(false);
            }
            Err(_poisoned) => {
                gst::error!(CAT, imp = self, "NALSense replay state lock is poisoned");
            }
        }
    }

    fn request_sleep(&self) {
        match self.state.lock() {
            Ok(mut state) => state.request_sleep(),
            Err(_poisoned) => {
                gst::error!(CAT, imp = self, "NALSense replay state lock is poisoned");
            }
        }
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let flags = buffer.flags();
        let delta_unit = flags.contains(gst::BufferFlags::DELTA_UNIT);
        let discontinuity = flags.contains(gst::BufferFlags::DISCONT);
        let action = {
            let mut state = self.state.lock().map_err(|_poisoned| {
                gst::element_imp_error!(
                    self,
                    gst::LibraryError::Failed,
                    ["NALSense replay state lock is poisoned"]
                );
                gst::FlowError::Error
            })?;
            if discontinuity {
                state.invalidate_stream();
            }
            if let Some(error) = state.flow_error {
                return Err(error);
            }

            if state.mode == Mode::Active {
                BufferAction::Push(buffer)
            } else {
                let is_idr = if delta_unit {
                    false
                } else {
                    let codec = state.codec.ok_or(gst::FlowError::NotNegotiated)?;
                    let mapped = buffer.map_readable().map_err(|error| {
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::Read,
                            ["Failed to map a replay compressed-video access unit: {error}"]
                        );
                        gst::FlowError::Error
                    })?;
                    codec.is_idr_candidate(mapped.as_slice()).map_err(|error| {
                        gst::element_imp_error!(
                            self,
                            gst::StreamError::Format,
                            ["Invalid replay {} access unit: {error}", codec.name()]
                        );
                        gst::FlowError::Error
                    })?
                };
                let settings = *self.settings.lock().map_err(|_poisoned| {
                    gst::element_imp_error!(
                        self,
                        gst::LibraryError::Failed,
                        ["NALSense replay settings lock is poisoned"]
                    );
                    gst::FlowError::Error
                })?;
                state.handle_buffer(buffer, is_idr, settings)
            }
        };

        match action {
            BufferAction::Drop => Ok(gst::FlowSuccess::Ok),
            BufferAction::Push(buffer) => self.srcpad.push(buffer),
            BufferAction::Replay(batch) => self.push_replay(batch),
        }
    }

    fn sink_event(&self, _pad: &gst::Pad, event: gst::Event) -> bool {
        let stream_state_result = match event.view() {
            gst::EventView::StreamStart(_) => self.invalidate_stream_state(true),
            gst::EventView::Caps(caps_event) => caps_event
                .caps()
                .structure(0)
                .and_then(|structure| Codec::from_media_type(structure.name()))
                .ok_or(gst::StateChangeError)
                .and_then(|codec| self.set_codec(codec)),
            gst::EventView::Segment(_) | gst::EventView::FlushStop(_) => {
                self.invalidate_stream_state(false)
            }
            _ => Ok(()),
        };
        if stream_state_result.is_err() {
            gst::error!(
                CAT,
                imp = self,
                "failed to configure NALSense replay stream state"
            );
            return false;
        }

        let wake = match event.view() {
            gst::EventView::CustomDownstream(custom) => {
                custom.structure().is_some_and(|structure| {
                    structure.name() == "nalsense-activity"
                        && structure.get::<String>("type").as_deref() == Ok("activity-start")
                })
            }
            _ => false,
        };
        if wake {
            let batch = match self.state.lock() {
                Ok(mut state) => state.request_wake(true),
                Err(_poisoned) => {
                    gst::error!(CAT, imp = self, "NALSense replay state lock is poisoned");
                    return false;
                }
            };
            if let Some(batch) = batch
                && self.push_replay(batch).is_err()
            {
                return false;
            }
        }
        self.srcpad.push_event(event)
    }

    fn sink_query(&self, _pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        self.srcpad.peer_query(query)
    }

    fn src_event(&self, _pad: &gst::Pad, event: gst::Event) -> bool {
        self.sinkpad.push_event(event)
    }

    fn src_query(&self, _pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
        self.sinkpad.peer_query(query)
    }

    fn push_replay(&self, mut batch: ReplayBatch) -> Result<gst::FlowSuccess, gst::FlowError> {
        if let Some(first) = batch.buffers.front_mut() {
            let flags = first.flags();
            first
                .make_mut()
                .set_flags(flags | gst::BufferFlags::DISCONT);
        }
        let mut replayed_frames = 0_u64;
        let mut replayed_bytes = 0_u64;
        let reason = batch.reason;
        let mut result = Ok(gst::FlowSuccess::Ok);
        for buffer in batch.buffers {
            let bytes = u64::try_from(buffer.size()).unwrap_or(u64::MAX);
            if let Err(error) = self.srcpad.push(buffer) {
                result = Err(error);
                break;
            }
            if replayed_frames < batch.replayed_frames {
                replayed_frames += 1;
                replayed_bytes = replayed_bytes.saturating_add(bytes);
            }
        }
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_poisoned| gst::FlowError::Error)?;
            state.replayed_frames = state.replayed_frames.saturating_add(replayed_frames);
            state.replayed_bytes = state.replayed_bytes.saturating_add(replayed_bytes);
            if let Err(error) = result {
                state.invalidate_stream();
                if state.mode == Mode::Active {
                    state.mode = Mode::WakeRequested;
                }
                // Event handlers return only bool; retain the flow error for the next buffer.
                state.flow_error = Some(error);
            }
        }
        result?;
        self.post_replay_message(reason, replayed_frames, replayed_bytes);
        Ok(gst::FlowSuccess::Ok)
    }

    fn post_replay_message(&self, reason: &str, replayed_frames: u64, replayed_bytes: u64) {
        let structure = gst::Structure::builder("nalsense-replay")
            .field("type", "wake")
            .field("reason", reason)
            .field("replayed-frames", replayed_frames)
            .field("replayed-bytes", replayed_bytes)
            .build();
        let message = gst::message::Element::builder(structure)
            .src(&*self.obj())
            .build();
        let _posted = self.obj().post_message(message);
    }
}
