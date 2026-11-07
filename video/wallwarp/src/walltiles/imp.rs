use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use wall_geometry::{Layout, Screen};

use crate::wallwarp::Interpolation;

const DEFAULT_BACKGROUND: u32 = 0xff00_0000;

/// Frames each display's queue holds at most.
const QUEUE_FRAMES: u32 = 3;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "walltiles",
        gst::DebugColorFlags::empty(),
        Some("Video wall displays from one layout"),
    )
});

struct Settings {
    layout: Option<Layout>,
    location: Option<String>,
    interpolation: Interpolation,
    background: u32,
    gl: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            layout: None,
            location: None,
            interpolation: Interpolation::default(),
            background: DEFAULT_BACKGROUND,
            gl: false,
        }
    }
}

/// What every display shares: the conversion or upload before the `tee`.
struct Head {
    tee: gst::Element,
    /// Whether the displays warp with `glwallwarp`.
    gl: bool,
}

/// One display's `queue ! warp ! capsfilter`, behind its source pad.
#[derive(Clone)]
struct Branch {
    id: String,
    /// The `tee` pad feeding it, once linked.
    feed: Option<gst::Pad>,
    queue: gst::Element,
    warp: gst::Element,
    /// Gives the display its size in one step.
    size: gst::Element,
    /// Its source pad, once added.
    pad: Option<gst::GhostPad>,
}

#[derive(Default)]
pub struct WallTiles {
    settings: Mutex<Settings>,
    head: Mutex<Option<Head>>,
    branches: Mutex<Vec<Branch>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The display's size, in system memory on the CPU and in any memory on
/// the GPU, whose warp gives what downstream asks for.
fn size_caps(screen: &Screen, gl: bool) -> gst::Caps {
    let features = if gl {
        gst::CapsFeatures::new_any()
    } else {
        gst::CapsFeatures::new_empty()
    };
    let mut caps = gst::Caps::builder("video/x-raw")
        .field(
            "width",
            i32::try_from(screen.output.width).unwrap_or(i32::MAX),
        )
        .field(
            "height",
            i32::try_from(screen.output.height).unwrap_or(i32::MAX),
        )
        .build();
    if let Some(caps) = caps.get_mut() {
        caps.set_features(0, Some(features));
    }
    caps
}

fn make(factory: &str, name: Option<String>) -> Result<gst::Element, String> {
    let builder = gst::ElementFactory::make(factory);
    match name {
        Some(name) => builder.name(name),
        None => builder,
    }
    .build()
    .map_err(|_err| format!("no {factory} element"))
}

impl WallTiles {
    /// The `tee` feeding the displays, made with the conversion before it
    /// the first time a display or the stream needs it. Choosing between
    /// CPU and GPU ends there.
    fn tee(&self) -> Result<gst::Element, String> {
        // No lock is held while the bin changes: its signals and messages
        // may call back into the element.
        if let Some(head) = lock(&self.head).as_ref() {
            return Ok(head.tee.clone());
        }
        let gl = lock(&self.settings).gl;
        // The CPU warp takes RGBA; the GPU one converts as it uploads.
        let first = if gl {
            make("glupload", Some("upload".to_owned()))
                .map_err(|err| format!("{err}; is the GStreamer OpenGL plugin installed?"))?
        } else {
            make("videoconvert", Some("convert".to_owned()))?
        };
        let tee = gst::ElementFactory::make("tee")
            .name("tee")
            .property("allow-not-linked", true)
            .build()
            .map_err(|_err| "no tee element".to_owned())?;
        let obj = self.obj();
        if let Err(err) = obj.add_many([&first, &tee]) {
            // Another thread made the head meanwhile.
            return lock(&self.head)
                .as_ref()
                .map(|head| head.tee.clone())
                .ok_or_else(|| err.to_string());
        }
        if let Err(err) = self.link_head(&first, &tee) {
            self.discard(&[&first, &tee]);
            return Err(err);
        }
        *lock(&self.head) = Some(Head {
            tee: tee.clone(),
            gl,
        });
        for element in [&tee, &first] {
            element
                .sync_state_with_parent()
                .map_err(|err| err.to_string())?;
        }
        Ok(tee)
    }

    /// Link `first ! tee` behind the sink pad.
    fn link_head(&self, first: &gst::Element, tee: &gst::Element) -> Result<(), String> {
        let obj = self.obj();
        first.link(tee).map_err(|err| err.to_string())?;
        let target = first.static_pad("sink").ok_or("no sink pad")?;
        obj.static_pad("sink")
            .and_then(|pad| pad.downcast::<gst::GhostPad>().ok())
            .ok_or("no sink ghost pad")?
            .set_target(Some(&target))
            .map_err(|err| err.to_string())?;
        // Warps take normalized source positions: a source of another shape
        // is stretched to the canvas.
        let weak = obj.downgrade();
        target.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data
                && let gst::EventView::Caps(caps) = event.view()
                && let Ok(video) = gst_video::VideoInfo::from_caps(caps.caps())
                && let Some(obj) = weak.upgrade()
            {
                obj.imp().check_source(video.width(), video.height());
            }
            gst::PadProbeReturn::Ok
        });
        Ok(())
    }

    /// Stop `elements` and take them out of the bin, if they are still in
    /// it: while the bin is disposed, it has removed them already.
    fn discard(&self, elements: &[&gst::Element]) {
        let obj = self.obj();
        for element in elements {
            if let Err(err) = element.set_state(gst::State::Null) {
                gst::warning!(CAT, imp = self, "stopping {}: {err}", element.name());
            }
            if element.parent().as_ref() == Some(obj.upcast_ref::<gst::Object>())
                && let Err(err) = obj.remove(*element)
            {
                gst::warning!(CAT, imp = self, "removing {}: {err}", element.name());
            }
        }
    }

    /// Warn when the source's shape differs from the canvas's.
    fn check_source(&self, width: u32, height: u32) {
        let settings = lock(&self.settings);
        if let Some(layout) = &settings.layout
            && let Err(err) = layout.check_source_size(width, height)
        {
            gst::element_imp_warning!(
                self,
                gst::StreamError::Format,
                ["{err}; the source is stretched to the canvas"]
            );
        }
    }

    /// Take a new layout and apply it to the displays already playing.
    fn set_layout(&self, layout: &Layout, location: Option<String>) {
        {
            let mut settings = lock(&self.settings);
            settings.layout = Some(layout.clone());
            settings.location = location;
        }
        let gl = lock(&self.head).as_ref().is_some_and(|head| head.gl);
        for branch in lock(&self.branches).iter() {
            match layout.screen(&branch.id) {
                Some(screen) => {
                    branch
                        .warp
                        .set_property("quad", layout.source_quad(screen).to_string());
                    branch.warp.set_property("mask", screen.mask.to_string());
                    branch.size.set_property("caps", size_caps(screen, gl));
                }
                None => gst::warning!(
                    CAT,
                    imp = self,
                    "display {:?} is not in the new layout; it keeps its last place",
                    branch.id
                ),
            }
        }
    }

    fn set_layout_text(&self, text: Option<&str>, location: Option<String>) {
        let Some(text) = text else {
            return;
        };
        match Layout::from_json(text) {
            Ok(layout) => self.set_layout(&layout, location),
            Err(err) => gst::warning!(
                CAT,
                imp = self,
                "invalid layout: {err}; keeping the current one"
            ),
        }
    }

    fn set_location(&self, location: Option<String>) {
        let Some(location) = location else {
            return;
        };
        match std::fs::read_to_string(&location) {
            Ok(text) => self.set_layout_text(Some(&text), Some(location)),
            Err(err) => gst::warning!(
                CAT,
                imp = self,
                "reading layout {location:?}: {err}; keeping the current one"
            ),
        }
    }

    /// Apply the shared warp settings to every display.
    fn update_warps(&self) {
        let (interpolation, background) = {
            let settings = lock(&self.settings);
            (settings.interpolation, settings.background)
        };
        for branch in lock(&self.branches).iter() {
            branch.warp.set_property("interpolation", interpolation);
            branch.warp.set_property("background", background);
        }
    }

    /// Add the branch of display `id` behind a new source pad. The display
    /// is reserved first, so two requests for it cannot both succeed, and
    /// no lock is held while the bin changes, whose signals and messages may
    /// call back into the element. A failed request leaves nothing behind.
    fn add_branch(&self, template: &gst::PadTemplate, id: &str) -> Result<gst::Pad, String> {
        let (layout, interpolation, background) = {
            let settings = lock(&self.settings);
            let layout = settings
                .layout
                .clone()
                .ok_or("set a layout before requesting display pads")?;
            (layout, settings.interpolation, settings.background)
        };
        let screen = layout
            .screen(id)
            .ok_or_else(|| format!("the layout has no display {id:?}"))?;
        if lock(&self.branches).iter().any(|branch| branch.id == id) {
            return Err(format!("display {id:?} already has a pad"));
        }
        let tee = self.tee()?;
        let gl = lock(&self.head).as_ref().is_some_and(|head| head.gl);
        let factory = if gl { "glwallwarp" } else { "wallwarp" };
        // A few frames at most: a live wall wants its displays current.
        let queue = gst::ElementFactory::make("queue")
            .name(format!("queue-{id}"))
            .property("max-size-buffers", QUEUE_FRAMES)
            .property("max-size-bytes", 0_u32)
            .property("max-size-time", 0_u64)
            .build()
            .map_err(|_err| "no queue element".to_owned())?;
        let warp = make(factory, Some(format!("warp-{id}")))?;
        warp.set_property("quad", layout.source_quad(screen).to_string());
        warp.set_property("mask", screen.mask.to_string());
        warp.set_property("interpolation", interpolation);
        warp.set_property("background", background);
        let size = gst::ElementFactory::make("capsfilter")
            .name(format!("size-{id}"))
            .property("caps", size_caps(screen, gl))
            // A frame of the old size may be on its way when the size
            // changes; without this it would stop the stream.
            .property_from_str("caps-change-mode", "delayed")
            .build()
            .map_err(|_err| "no capsfilter element".to_owned())?;
        let mut branch = Branch {
            id: id.to_owned(),
            feed: None,
            queue,
            warp,
            size,
            pad: None,
        };
        {
            let mut branches = lock(&self.branches);
            if branches.iter().any(|branch| branch.id == id) {
                return Err(format!("display {id:?} already has a pad"));
            }
            branches.push(branch.clone());
        }
        let linked = self
            .obj()
            .add_many([&branch.queue, &branch.warp, &branch.size])
            .map_err(|err| err.to_string())
            .and_then(|()| self.link_branch(template, &tee, &mut branch));
        let mut branches = lock(&self.branches);
        let reserved = branches.iter().position(|reserved| reserved.id == id);
        match (linked, reserved) {
            (Ok(()), Some(index)) => {
                let pad = branch.pad.clone();
                if let Some(reserved) = branches.get_mut(index) {
                    *reserved = branch;
                }
                pad.map(gst::GhostPad::upcast)
                    .ok_or_else(|| "no pad".to_owned())
            }
            (result, reserved) => {
                if let Some(index) = reserved {
                    branches.swap_remove(index);
                }
                drop(branches);
                self.remove_branch(&branch);
                Err(result
                    .err()
                    .unwrap_or_else(|| "the display was taken out".to_owned()))
            }
        }
    }

    /// Link a new branch from its source pad back to the `tee`, so frames
    /// find it ready, and start it; what was made is kept in `branch` for a
    /// rollback.
    fn link_branch(
        &self,
        template: &gst::PadTemplate,
        tee: &gst::Element,
        branch: &mut Branch,
    ) -> Result<(), String> {
        let obj = self.obj();
        let chain = [&branch.queue, &branch.warp, &branch.size];
        gst::Element::link_many(chain).map_err(|err| err.to_string())?;
        let target = branch
            .size
            .static_pad("src")
            .ok_or("no capsfilter src pad")?;
        let pad = gst::GhostPad::builder_from_template_with_target(template, &target)
            .map_err(|err| err.to_string())?
            .name(format!("src_{}", branch.id))
            .build();
        if obj.current_state() > gst::State::Null {
            pad.set_active(true).map_err(|err| err.to_string())?;
        }
        obj.add_pad(&pad).map_err(|err| err.to_string())?;
        branch.pad = Some(pad);
        for element in [&branch.size, &branch.warp, &branch.queue] {
            element
                .sync_state_with_parent()
                .map_err(|err| err.to_string())?;
        }
        let feed = tee
            .request_pad_simple("src_%u")
            .ok_or("the tee gave no pad")?;
        branch.feed = Some(feed.clone());
        let input = branch.queue.static_pad("sink").ok_or("no queue sink pad")?;
        feed.link(&input).map_err(|err| err.to_string())?;
        Ok(())
    }

    /// Take `branch` out: unfed, its pad gone, its elements stopped and
    /// removed, all before returning. The application stops whatever its
    /// pad feeds first, or the branch's last push may hold this up.
    fn remove_branch(&self, branch: &Branch) {
        let obj = self.obj();
        if let Some(feed) = &branch.feed {
            if let Some(input) = branch.queue.static_pad("sink") {
                // Unlinked, the tee drops this branch's frames.
                let _unlinked = feed.unlink(&input);
            }
            if let Some(head) = lock(&self.head).as_ref() {
                head.tee.release_request_pad(feed);
            }
        }
        if let Some(pad) = &branch.pad {
            if let Err(err) = pad.set_active(false) {
                gst::warning!(CAT, imp = self, "stopping pad of {:?}: {err}", branch.id);
            }
            if pad.parent().as_ref() == Some(obj.upcast_ref::<gst::Object>())
                && let Err(err) = obj.remove_pad(pad)
            {
                gst::warning!(CAT, imp = self, "removing pad of {:?}: {err}", branch.id);
            }
        }
        self.discard(&[&branch.queue, &branch.warp, &branch.size]);
    }
}

#[glib::object_subclass]
impl ObjectSubclass for WallTiles {
    const NAME: &'static str = "GstSmithWallTiles";
    type Type = super::WallTiles;
    type ParentType = gst::Bin;
}

impl ObjectImpl for WallTiles {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecString::builder("layout")
                    .nick("Layout")
                    .blurb(
                        "The video wall layout as JSON; each display's pad is src_ followed by \
                         its ID. An invalid layout is ignored",
                    )
                    .mutable_playing()
                    .build(),
                glib::ParamSpecString::builder("location")
                    .nick("Location")
                    .blurb("A file to read the layout from, instead of setting layout")
                    .mutable_playing()
                    .build(),
                glib::ParamSpecEnum::builder_with_default(
                    "interpolation",
                    Interpolation::default(),
                )
                .nick("Interpolation")
                .blurb("How output pixels sample the source")
                .mutable_playing()
                .build(),
                glib::ParamSpecUInt::builder("background")
                    .nick("Background")
                    .blurb(
                        "Color of output pixels that sample outside the source or that a mask \
                         hides, as big-endian ARGB (0xAARRGGBB)",
                    )
                    .default_value(DEFAULT_BACKGROUND)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("gl")
                    .nick("OpenGL")
                    .blurb(
                        "Warp on the GPU with glwallwarp, uploading each frame once for every \
                         display; set before the first display pad or the first start",
                    )
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "layout" => {
                let text = value.get::<Option<String>>().ok().flatten();
                self.set_layout_text(text.as_deref(), None);
            }
            "location" => self.set_location(value.get::<Option<String>>().ok().flatten()),
            "interpolation" => {
                lock(&self.settings).interpolation = value.get().unwrap_or_default();
                self.update_warps();
            }
            "background" => {
                lock(&self.settings).background = value.get().unwrap_or(DEFAULT_BACKGROUND);
                self.update_warps();
            }
            "gl" => {
                let gl = value.get().unwrap_or_default();
                if lock(&self.head).as_ref().is_some_and(|head| head.gl != gl) {
                    gst::warning!(
                        CAT,
                        imp = self,
                        "gl can change only before the first display pad or the first start; \
                         ignoring it"
                    );
                    return;
                }
                lock(&self.settings).gl = gl;
            }
            other => gst::error!(CAT, imp = self, "unknown property {other}"),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = lock(&self.settings);
        match pspec.name() {
            "layout" => settings
                .layout
                .as_ref()
                .and_then(|layout| layout.to_json().ok())
                .to_value(),
            "location" => settings.location.to_value(),
            "interpolation" => settings.interpolation.to_value(),
            "background" => settings.background.to_value(),
            "gl" => settings.gl.to_value(),
            other => {
                gst::error!(CAT, imp = self, "unknown property {other}");
                glib::Value::from_type(pspec.value_type())
            }
        }
    }

    fn constructed(&self) {
        self.parent_constructed();
        let obj = self.obj();
        if let Some(template) = obj.pad_template("sink") {
            let pad = gst::GhostPad::builder_from_template(&template).build();
            if let Err(err) = obj.add_pad(&pad) {
                gst::error!(CAT, imp = self, "adding the sink pad: {err}");
            }
        }
    }
}

impl GstObjectImpl for WallTiles {}

impl ElementImpl for WallTiles {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Video wall tiles",
                "Filter/Converter/Video/Scaler",
                "Splits one video into the streams of a video wall layout's displays, one \
                 source pad per display ID, each warped by wallwarp or glwallwarp",
                "Nemanja Zbiljic <nemanja.zbiljic@gmail.com>",
            )
        });
        Some(&*METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let Ok(caps) = "video/x-raw(ANY)".parse::<gst::Caps>() else {
                return Vec::new();
            };
            [
                ("sink", gst::PadDirection::Sink, gst::PadPresence::Always),
                ("src_%s", gst::PadDirection::Src, gst::PadPresence::Request),
            ]
            .into_iter()
            .filter_map(|(name, direction, presence)| {
                gst::PadTemplate::new(name, direction, presence, &caps).ok()
            })
            .collect()
        });
        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        template: &gst::PadTemplate,
        name: Option<&str>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let Some(id) = name.and_then(|name| name.strip_prefix("src_")) else {
            gst::error!(
                CAT,
                imp = self,
                "request display pads by name: src_ followed by the display ID"
            );
            return None;
        };
        self.add_branch(template, id)
            .inspect_err(|err| gst::error!(CAT, imp = self, "no pad for {id:?}: {err}"))
            .ok()
    }

    fn release_pad(&self, pad: &gst::Pad) {
        let branch = {
            let mut branches = lock(&self.branches);
            let index = branches.iter().position(|branch| {
                branch
                    .pad
                    .as_ref()
                    .is_some_and(|own| own.upcast_ref::<gst::Pad>() == pad)
            });
            index.map(|index| branches.swap_remove(index))
        };
        if let Some(branch) = branch {
            self.remove_branch(&branch);
        }
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if transition == gst::StateChange::NullToReady
            && let Err(err) = self.tee()
        {
            gst::element_imp_error!(self, gst::CoreError::MissingPlugin, ["{err}"]);
            return Err(gst::StateChangeError);
        }
        self.parent_change_state(transition)
    }
}

impl BinImpl for WallTiles {}
