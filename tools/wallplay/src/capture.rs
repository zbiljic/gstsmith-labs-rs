//! Cameras and screens of the machine `wallplay` runs on.

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use gst::prelude::*;
use serde::Serialize;

/// What a capture device is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Camera,
    Screen,
}

/// A camera or screen a source can play.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Device {
    pub kind: Kind,
    /// What a source names it by: a camera's name, made unique, or a
    /// screen's number from 1.
    pub id: String,
    pub name: String,
    /// The largest picture it offers, as `WIDTHxHEIGHT`, where known.
    pub size: Option<String>,
}

/// Names made unique in order: a second "Camera" becomes "Camera (2)", or
/// the next number that no other camera has or is called.
#[must_use]
pub fn unique_ids(names: &[String]) -> Vec<String> {
    let mut taken: HashSet<String> = HashSet::new();
    names
        .iter()
        .map(|name| {
            let id = std::iter::once(name.clone())
                // Enough numbers that one is free of every name and ID.
                .chain((2..=2 * names.len() + 2).map(|count| format!("{name} ({count})")))
                .find(|id| !taken.contains(id) && (id == name || !names.contains(id)))
                .unwrap_or_else(|| name.clone());
            taken.insert(id.clone());
            id
        })
        .collect()
}

/// The largest fixed size of raw video in `caps`: compressed modes, such as
/// many cameras' larger JPEG ones, are not played.
fn largest_size(caps: &gst::Caps) -> Option<String> {
    caps.iter()
        .filter(|structure| structure.name() == "video/x-raw")
        .filter_map(|structure| {
            Some((
                structure.get::<i32>("width").ok()?,
                structure.get::<i32>("height").ok()?,
            ))
        })
        .max_by_key(|(width, height)| i64::from(*width) * i64::from(*height))
        .map(|(width, height)| format!("{width}x{height}"))
}

/// The cameras `GStreamer`'s device monitor finds, by ID. Starting the
/// monitor can take a moment.
#[must_use]
pub fn cameras() -> Vec<(String, gst::Device)> {
    // Sources are checked while the command line is read, before anything
    // else starts GStreamer.
    if gst::init().is_err() {
        return Vec::new();
    }
    let monitor = gst::DeviceMonitor::new();
    if monitor.add_filter(Some("Video/Source"), None).is_none() || monitor.start().is_err() {
        return Vec::new();
    }
    let devices = monitor.devices();
    monitor.stop();
    let devices: Vec<gst::Device> = devices.into_iter().collect();
    let names: Vec<String> = devices
        .iter()
        .map(|device| device.display_name().to_string())
        .collect();
    unique_ids(&names).into_iter().zip(devices).collect()
}

/// The camera with ID `id`. Starts a device monitor, so keep it off locks
/// and streaming threads.
pub fn camera(id: &str) -> Result<gst::Device> {
    cameras()
        .into_iter()
        .find_map(|(found, device)| (found == id).then_some(device))
        .with_context(|| format!("no camera {id:?} is connected"))
}

/// Screens can be captured on macOS only, by `avfvideosrc`.
pub const SCREEN_CAPTURE: bool = cfg!(target_os = "macos");

/// Most screens looked for.
const MAX_SCREENS: u32 = 8;

/// Caps that give a capture square pixels, which `avfvideosrc` leaves out,
/// so scaling would take the lowest ratio it can and stretch the picture,
/// and raw video, which the canvas converts.
const CAPTURE_CAPS: &str = "video/x-raw,pixel-aspect-ratio=1/1";

/// `source` and a capsfilter giving it [`CAPTURE_CAPS`], in a bin with a
/// `src` pad.
pub fn capture_bin(source: &gst::Element) -> Result<gst::Element> {
    let caps = CAPTURE_CAPS.parse::<gst::Caps>()?;
    let filter = gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()?;
    let bin = gst::Bin::new();
    bin.add_many([source, &filter])?;
    source.link(&filter)?;
    let output = filter.static_pad("src").context("capsfilter has no pad")?;
    bin.add_pad(
        &gst::GhostPad::builder_with_target(&output)?
            .name("src")
            .build(),
    )?;
    Ok(bin.upcast())
}

/// An element capturing screen `number`, counted from 1, with the cursor.
pub fn screen(number: u32) -> Result<gst::Element> {
    if !SCREEN_CAPTURE {
        bail!("screens can be captured on macOS only");
    }
    gst::init()?;
    let index = number
        .checked_sub(1)
        .and_then(|index| i32::try_from(index).ok())
        .context("screens are numbered from 1")?;
    gst::ElementFactory::make("avfvideosrc")
        .property("capture-screen", true)
        .property("capture-screen-cursor", true)
        .property("device-index", index)
        .build()
        .context("creating avfvideosrc, from gst-plugins-bad's applemedia plugin")
}

/// How many screens there are. `avfvideosrc` refuses to start for a screen
/// that does not exist, so each is started in turn until one is refused;
/// a screen slower than a second to start counts as missing.
#[must_use]
pub fn screen_count() -> u32 {
    if !SCREEN_CAPTURE {
        return 0;
    }
    (1..=MAX_SCREENS)
        .take_while(|number| screen_exists(*number))
        .last()
        .unwrap_or(0)
}

fn screen_exists(number: u32) -> bool {
    let Ok(source) = screen(number) else {
        return false;
    };
    // Downstream gives the frame rate, which the source otherwise fixes
    // with a critical warning.
    let pipeline = gst::Pipeline::new();
    let started = (|| {
        let rate = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            )
            .build()
            .ok()?;
        let sink = gst::ElementFactory::make("fakesink").build().ok()?;
        pipeline.add_many([&source, &rate, &sink]).ok()?;
        gst::Element::link_many([&source, &rate, &sink]).ok()?;
        pipeline.set_state(gst::State::Paused).ok()?;
        pipeline.state(gst::ClockTime::from_seconds(1)).0.ok()
    })()
    .is_some();
    let _stopped = pipeline.set_state(gst::State::Null);
    started
}

/// Every camera and screen, cameras first.
#[must_use]
pub fn devices() -> Vec<Device> {
    let cameras = cameras().into_iter().map(|(id, device)| Device {
        kind: Kind::Camera,
        name: device.display_name().to_string(),
        size: device.caps().as_ref().and_then(largest_size),
        id,
    });
    let screens = (1..=screen_count()).map(|number| Device {
        kind: Kind::Screen,
        id: number.to_string(),
        name: format!("Screen {number}"),
        size: None,
    });
    cameras.chain(screens).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_names_get_numbers() {
        let names = ["Camera", "Desk", "Camera", "Camera"].map(str::to_owned);
        assert_eq!(
            unique_ids(&names),
            ["Camera", "Desk", "Camera (2)", "Camera (3)"]
        );
        // A name that looks numbered keeps its own; the copy takes the next.
        let names = ["Cam", "Cam (2)", "Cam"].map(str::to_owned);
        assert_eq!(unique_ids(&names), ["Cam", "Cam (2)", "Cam (3)"]);
        let names = ["Cam", "Cam", "Cam (2)"].map(str::to_owned);
        assert_eq!(unique_ids(&names), ["Cam", "Cam (3)", "Cam (2)"]);
    }

    #[test]
    fn the_largest_fixed_size_sums_up_a_camera() {
        gst::init().expect("GStreamer");
        let caps: gst::Caps = "video/x-raw,width=640,height=480; \
                               video/x-raw,width=1920,height=1080; \
                               image/jpeg,width=3840,height=2160; \
                               video/x-raw,width=[1,100],height=[1,100]"
            .parse()
            .expect("caps");
        assert_eq!(largest_size(&caps).as_deref(), Some("1920x1080"));
        assert_eq!(largest_size(&gst::Caps::new_any()), None);
    }

    #[test]
    fn screens_are_numbered_from_one() {
        gst::init().expect("GStreamer");
        let error = screen(0).expect_err("no screen 0").to_string();
        let expected = if SCREEN_CAPTURE {
            "numbered from 1"
        } else {
            "macOS only"
        };
        assert!(error.contains(expected), "{error}");
    }

    #[test]
    fn captures_get_square_pixels() {
        gst::init().expect("GStreamer");
        let source = gst::ElementFactory::make("videotestsrc")
            .property("num-buffers", 1)
            .build()
            .expect("videotestsrc");
        let bin = capture_bin(&source).expect("bin");
        let caps = bin.static_pad("src").expect("src pad").query_caps(None);
        assert!(
            caps.iter().all(
                |structure| structure.get::<gst::Fraction>("pixel-aspect-ratio")
                    == Ok(gst::Fraction::new(1, 1))
            ),
            "{caps}"
        );
    }
}
