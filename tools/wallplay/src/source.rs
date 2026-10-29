//! What feeds the canvas, and how each source opens.
//!
//! A URL opens by what it looks like, unless the control page says how:
//!
//! - media that `GStreamer` opens itself with `uridecodebin`: files, HTTP(S)
//!   videos, HLS and DASH streams, RTSP, SRT and RTMP;
//! - web pages such as `YouTube` and others, whose stream `yt-dlp` finds;
//! - WHEP endpoints (WebRTC-HTTP egress), which `whepclientsrc` plays.

use std::collections::BTreeMap;
use std::process::Command;
use std::str::FromStr as _;

use anyhow::{Context, Result, bail, ensure};
use gst::prelude::*;
use serde::{Deserialize, Serialize};
use wall_geometry::Size;

/// What feeds the canvas.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A built-in test pattern from [`PATTERNS`], by ID.
    Pattern(String),
    /// A URL, opened by what it looks like; see [`classify`].
    Uri(String),
    /// A URL `GStreamer` opens itself.
    Media(String),
    /// A web page whose video `yt-dlp` finds.
    Page(String),
    /// A WHEP endpoint.
    Whep(String),
    /// A `gst-launch-1.0` description with one unlinked video source pad.
    Launch(String),
}

impl Source {
    /// A pattern if `text` names one, a URL if it has a scheme, otherwise a
    /// pipeline description.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        if pattern(text).is_some() {
            Self::Pattern(text.to_owned())
        } else if text.contains("://") {
            Self::Uri(text.to_owned())
        } else {
            Self::Launch(text.to_owned())
        }
    }
}

/// Extensions of URLs that are media rather than web pages.
const MEDIA_EXTENSIONS: [&str; 17] = [
    "m3u8", "mpd", "mp4", "m4v", "mov", "mkv", "webm", "ts", "m2ts", "mts", "flv", "avi", "ogv",
    "ogg", "mjpg", "mjpeg", "ism",
];

/// Sites whose pages only `yt-dlp` can play.
const PAGE_SITES: [&str; 6] = [
    "youtube.com",
    "youtu.be",
    "twitch.tv",
    "vimeo.com",
    "dailymotion.com",
    "kick.com",
];

/// How a [`Source::Uri`] opens: HTTP(S) URLs ending in `/whep` as WHEP,
/// others without a media file extension as web pages, and everything else,
/// such as `rtsp://`, `srt://` or `file://`, as media.
#[must_use]
pub fn classify(url: &str) -> Source {
    let media = || Source::Media(url.to_owned());
    let Some((scheme, rest)) = url.split_once("://") else {
        return media();
    };
    if !["http", "https"].contains(&scheme.to_ascii_lowercase().as_str()) {
        return media();
    }
    let path = rest
        .split(['?', '#'])
        .next()
        .and_then(|rest| rest.split_once('/'))
        .map_or(String::new(), |(_, path)| path.to_ascii_lowercase());
    let file = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if file
        .rsplit_once('.')
        .is_some_and(|(_, extension)| MEDIA_EXTENSIONS.contains(&extension))
    {
        media()
    } else if file == "whep" {
        Source::Whep(url.to_owned())
    } else {
        Source::Page(url.to_owned())
    }
}

/// Whether `url` is on a site only `yt-dlp` can play.
fn on_page_site(url: &str) -> bool {
    let host = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split(['/', '?', '#', ':'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    PAGE_SITES
        .iter()
        .any(|site| host == *site || host.ends_with(&format!(".{site}")))
}

/// Finds the stream of a web page with `yt-dlp`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolver {
    /// The `yt-dlp` program.
    pub program: String,
}

impl Default for Resolver {
    fn default() -> Self {
        Self {
            program: "yt-dlp".to_owned(),
        }
    }
}

/// Formats `yt-dlp` picks, best first: video up to 1080 lines, H.264 if it
/// can, since it decodes in hardware on most machines, and not split into
/// DASH fragments, which `GStreamer` cannot fetch from a single URL.
const PAGE_FORMATS: &str = "bv*[height<=?1080][vcodec^=avc1][protocol!*=dash]\
                            /bv*[height<=?1080][protocol!*=dash]/b[protocol!*=dash]/b";

/// A page's stream as `yt-dlp` found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub uri: String,
    /// HTTP headers the stream's server expects; `YouTube` refuses requests
    /// without its User-Agent.
    pub headers: Vec<(String, String)>,
    pub title: Option<String>,
    pub live: Option<bool>,
}

#[derive(Deserialize)]
struct PageFormat {
    url: Option<String>,
    #[serde(default)]
    http_headers: BTreeMap<String, String>,
    vcodec: Option<String>,
}

#[derive(Deserialize)]
struct PageInfo {
    title: Option<String>,
    is_live: Option<bool>,
    #[serde(flatten)]
    format: PageFormat,
    #[serde(default)]
    requested_formats: Vec<PageFormat>,
}

impl Resolver {
    /// The stream of `page`; takes a few seconds.
    pub fn resolve(&self, page: &str) -> Result<Resolved> {
        let output = Command::new(&self.program)
            .args(["-J", "--no-playlist", "--no-warnings", "--socket-timeout"])
            .args(["15", "-f", PAGE_FORMATS, "--", page])
            .output();
        let output = match output {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(anyhow::Error::new(error).context(format!(
                    "{} is not installed; it finds the video of YouTube and some other web pages",
                    self.program
                )));
            }
            Err(error) => return Err(error).with_context(|| format!("running {}", self.program)),
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("failed")
                .trim_start_matches("ERROR: ");
            bail!("{}: {reason}", self.program);
        }
        parse_page_info(&output.stdout)
    }
}

/// The stream in `yt-dlp -J` output.
fn parse_page_info(json: &[u8]) -> Result<Resolved> {
    let info: PageInfo = serde_json::from_slice(json).context("reading yt-dlp's output")?;
    // Formats merged from separate video and audio streams list them apart.
    let format = if info.format.url.is_some() {
        info.format
    } else {
        info.requested_formats
            .into_iter()
            .find(|format| {
                format
                    .vcodec
                    .as_deref()
                    .is_some_and(|codec| codec != "none")
            })
            .context("yt-dlp found no video stream")?
    };
    Ok(Resolved {
        uri: format.url.context("yt-dlp found no stream URL")?,
        headers: format.http_headers.into_iter().collect(),
        title: info.title,
        live: info.is_live,
    })
}

/// What opens a source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Via {
    Pattern,
    Gstreamer,
    YtDlp,
    Whep,
    Pipeline,
}

/// What a player plugs in for a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Pattern(String),
    Uri {
        uri: String,
        headers: Vec<(String, String)>,
    },
    Whep(String),
    Launch(String),
}

/// RTSP's jitter buffer, down from `rtspsrc`'s two seconds.
const RTSP_LATENCY_MS: u32 = 200;

impl Input {
    #[must_use]
    pub fn is_pattern(&self) -> bool {
        matches!(self, Self::Pattern(_))
    }

    /// A new element producing this input's video, for a canvas of `canvas`.
    /// Patterns and pipelines have a `src` pad; other elements add theirs.
    pub fn element(&self, canvas: Size) -> Result<gst::Element> {
        let description = match self {
            Self::Uri { uri, .. } => {
                return gst::ElementFactory::make("uridecodebin")
                    .property("uri", uri)
                    // Any memory, so a hardware decoder can negotiate system memory
                    // with what follows rather than end decoding early.
                    .property("caps", gst::Caps::from_str("video/x-raw(ANY)")?)
                    .property("expose-all-streams", false)
                    .build()
                    .context("creating uridecodebin");
            }
            Self::Whep(endpoint) => {
                let source = gst::ElementFactory::make("whepclientsrc").build().context(
                    "WHEP needs whepclientsrc, from the rswebrtc plugin of gst-plugins-rs",
                )?;
                source
                    .property::<gst::glib::Object>("signaller")
                    .set_property("whep-endpoint", endpoint);
                // Only video is used, and servers such as MediaMTX refuse an
                // offer with audio for a stream without it.
                source.set_property("audio-codecs", gst::Array::new(Vec::<String>::new()));
                return Ok(source);
            }
            Self::Pattern(id) => pattern_launch(id, canvas)?,
            Self::Launch(description) => description.clone(),
        };
        // Only the video output gets a ghost pad: an unlinked sink, such as
        // textoverlay's text input, would be waited on for ever.
        let bin = gst::parse::bin_from_description(&description, false)
            .with_context(|| format!("invalid source pipeline {description:?}"))?;
        let output = bin
            .find_unlinked_pad(gst::PadDirection::Src)
            .context("the source pipeline has no video output")?;
        bin.add_pad(
            &gst::GhostPad::builder_with_target(&output)?
                .name("src")
                .build(),
        )?;
        Ok(bin.upcast())
    }

    /// Set up `element`, just added somewhere inside this input's element:
    /// the stream's headers on HTTP sources, a short jitter buffer on RTSP.
    pub fn configure(&self, element: &gst::Element) {
        let Some(factory) = element.factory() else {
            return;
        };
        match factory.name().as_str() {
            "souphttpsrc" => {
                let Self::Uri { headers, .. } = self else {
                    return;
                };
                let mut extra = gst::Structure::builder("headers");
                for (name, value) in headers {
                    if name.eq_ignore_ascii_case("user-agent") {
                        element.set_property("user-agent", value);
                    } else {
                        extra = extra.field(name.as_str(), value);
                    }
                }
                element.set_property("extra-headers", extra.build());
            }
            "rtspsrc" => element.set_property("latency", RTSP_LATENCY_MS),
            _ => {}
        }
    }
}

/// A source ready to play: what was asked for and what opens it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Prepared {
    pub source: Source,
    pub via: Via,
    #[serde(skip)]
    pub input: Input,
    pub title: Option<String>,
    pub live: Option<bool>,
}

impl Prepared {
    /// Pattern `id`, which must exist.
    pub fn pattern(id: &str) -> Result<Self> {
        Self::prepare(&Source::Pattern(id.to_owned()), &Resolver::default())
    }

    /// Get `source` ready to play, asking `yt-dlp` for a web page's stream.
    pub fn prepare(source: &Source, resolver: &Resolver) -> Result<Self> {
        let plain = |via, input| Self {
            source: source.clone(),
            via,
            input,
            title: None,
            live: None,
        };
        let url = match source {
            Source::Pattern(id) => {
                let pattern = pattern(id).with_context(|| format!("no pattern {id:?}"))?;
                return Ok(Self {
                    title: Some(pattern.name.to_owned()),
                    live: Some(true),
                    ..plain(Via::Pattern, Input::Pattern(id.clone()))
                });
            }
            Source::Launch(description) => {
                return Ok(plain(Via::Pipeline, Input::Launch(description.clone())));
            }
            Source::Uri(url) | Source::Media(url) | Source::Page(url) | Source::Whep(url) => url,
        };
        ensure!(
            url.contains("://"),
            "{url:?} is not a URL; use file:///path for a file on this server"
        );
        let opened = match source {
            Source::Uri(_) => classify(url),
            other => other.clone(),
        };
        let media = || Input::Uri {
            uri: url.clone(),
            headers: Vec::new(),
        };
        match opened {
            Source::Page(_) => match resolver.resolve(url) {
                Ok(found) => Ok(Self {
                    title: found.title,
                    live: found.live,
                    ..plain(
                        Via::YtDlp,
                        Input::Uri {
                            uri: found.uri,
                            headers: found.headers,
                        },
                    )
                }),
                // Without yt-dlp, a URL given as is might still be media.
                Err(error)
                    if matches!(source, Source::Uri(_))
                        && !on_page_site(url)
                        && error
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    Ok(plain(Via::Gstreamer, media()))
                }
                Err(error) => Err(error),
            },
            Source::Whep(_) => Ok(Self {
                live: Some(true),
                ..plain(Via::Whep, Input::Whep(url.clone()))
            }),
            _ => Ok(plain(Via::Gstreamer, media())),
        }
    }
}

/// A built-in test pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Pattern {
    pub id: &'static str,
    pub name: &'static str,
    /// What it shows up on a wall.
    pub checks: &'static str,
    /// A `gst-launch-1.0` description; `{cols}` and `{rows}` become about one
    /// column and row per 80 canvas pixels, `{speed}` a pixel step that crosses
    /// the canvas in about three seconds, `{width}` and `{height}` its size.
    #[serde(skip)]
    pub launch: &'static str,
    /// Text drawn over it, an element description whose first word is its
    /// factory, left out where that element is missing: the Pango plugin is
    /// its own package on Debian and Ubuntu, `gstreamer1.0-x`.
    #[serde(skip)]
    pub text: Option<&'static str>,
}

/// Test patterns, the first being the default source.
pub const PATTERNS: [Pattern; 8] = [
    Pattern {
        id: "bars",
        name: "Moving color bars",
        checks: "sync and color: bar edges break across a gap when one display lags",
        launch: "videotestsrc is-live=true pattern=smpte horizontal-speed=4",
        text: None,
    },
    Pattern {
        id: "sweep",
        name: "Sweeping bar",
        checks: "sync between side-by-side displays: the bar jumps when one lags",
        launch: "videotestsrc is-live=true pattern=bar horizontal-speed={speed}",
        text: None,
    },
    Pattern {
        id: "ball",
        name: "Circling ball",
        checks: "motion across displays, once around per second",
        launch: "videotestsrc is-live=true pattern=ball motion=sweep",
        text: None,
    },
    Pattern {
        id: "spokes",
        name: "Spokes",
        checks: "geometry: lines stay straight across gaps",
        launch: "videotestsrc is-live=true pattern=spokes",
        text: None,
    },
    Pattern {
        id: "checkerboard",
        name: "Checkerboard",
        checks: "geometry and scale: squares stay square and in line",
        launch: "videotestsrc is-live=true pattern=checkers-1 ! video/x-raw,width={cols},height={rows} \
                 ! videoscale method=nearest-neighbour add-borders=false \
                 ! capsfilter caps=video/x-raw,width={width},height={height}",
        text: None,
    },
    Pattern {
        id: "timecode",
        name: "Color bars with timecode",
        checks: "sync by photo: displays that show the timecode should agree",
        launch: "videotestsrc is-live=true pattern=smpte horizontal-speed=4",
        text: Some(
            "timeoverlay font-desc=\"Sans 72\" halignment=center valignment=center \
             shaded-background=true",
        ),
    },
    Pattern {
        id: "slate",
        name: "Slate saying what the source does",
        checks: "that the wall plays: as a standby, it says why the source is not showing",
        launch: "videotestsrc is-live=true pattern=solid-color foreground-color=0xff1c2430",
        text: Some(
            "textoverlay name=slate-text text=\"No signal\" font-desc=\"Sans 18\" \
             halignment=center valignment=center",
        ),
    },
    Pattern {
        id: "black",
        name: "Black",
        checks: "nothing: the displays go dark",
        launch: "videotestsrc is-live=true pattern=black",
        text: None,
    },
];

/// The pattern with this ID.
#[must_use]
pub fn pattern(id: &str) -> Option<&'static Pattern> {
    PATTERNS.iter().find(|pattern| pattern.id == id)
}

/// The pipeline description of pattern `id` for a canvas of `size`.
pub fn pattern_launch(id: &str, size: Size) -> Result<String> {
    let pattern = pattern(id).with_context(|| format!("no pattern {id:?}"))?;
    let (width, height) = (size.width, size.height);
    let text = pattern.text.filter(|text| {
        text.split_whitespace()
            .next()
            .is_some_and(|factory| gst::ElementFactory::find(factory).is_some())
    });
    let launch = match text {
        Some(text) => format!("{} ! {text}", pattern.launch),
        None => pattern.launch.to_owned(),
    };
    Ok(launch
        .replace("{cols}", &width.div_ceil(80).to_string())
        .replace("{rows}", &height.div_ceil(80).to_string())
        .replace("{speed}", &(width / 90).max(1).to_string())
        .replace("{width}", &width.to_string())
        .replace("{height}", &height.to_string()))
}

/// The default source.
pub const DEFAULT_SOURCE: &str = "bars";

/// The default standby pattern.
pub const DEFAULT_STANDBY: &str = "slate";

/// Name of the slate's text element, which a player keeps up to date.
pub const SLATE_TEXT: &str = "slate-text";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_patterns_urls_or_pipelines() {
        assert_eq!(
            Source::parse("file:///tmp/a.mp4"),
            Source::Uri("file:///tmp/a.mp4".to_owned())
        );
        assert_eq!(
            Source::parse(DEFAULT_SOURCE),
            Source::Pattern("bars".to_owned())
        );
        assert_eq!(
            Source::parse("videotestsrc pattern=ball"),
            Source::Launch("videotestsrc pattern=ball".to_owned())
        );
    }

    #[test]
    fn urls_open_by_what_they_look_like() {
        let kind = |url: &str| match classify(url) {
            Source::Media(_) => "media",
            Source::Page(_) => "page",
            Source::Whep(_) => "whep",
            _ => "other",
        };
        for url in [
            "file:///videos/a.mp4",
            "rtsp://camera.local:554/stream1",
            "srt://10.0.0.5:7001",
            "https://example.com/live/index.m3u8?token=1",
            "https://example.com/Video.MP4",
            "http://camera.local/video.mjpg",
            "gstwebrtc://127.0.0.1:8443?peer-id=abc",
        ] {
            assert_eq!(kind(url), "media", "{url}");
        }
        for url in [
            "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            "https://youtu.be/aqz-KE-bpKQ",
            "https://www.twitch.tv/somechannel",
            "https://example.com/watch/clip.html",
            "https://example.com/videos.mp4/page",
        ] {
            assert_eq!(kind(url), "page", "{url}");
        }
        for url in [
            "http://127.0.0.1:8889/cam/whep",
            "https://example.com/whep/",
        ] {
            assert_eq!(kind(url), "whep", "{url}");
        }
        assert!(on_page_site("https://m.youtube.com/watch?v=x"));
        assert!(on_page_site("https://www.twitch.tv/x"));
        assert!(!on_page_site("https://notyoutube.com/x"));
    }

    #[test]
    fn yt_dlp_output_gives_the_video_stream_and_its_headers() {
        let single = br#"{"title": "Clip", "is_live": false, "url": "https://cdn/v.mp4",
            "vcodec": "avc1.64002A", "http_headers": {"User-Agent": "Mozilla/5.0", "Accept": "*/*"}}"#;
        let resolved = parse_page_info(single).expect("single format");
        assert_eq!(resolved.uri, "https://cdn/v.mp4");
        assert_eq!(resolved.title.as_deref(), Some("Clip"));
        assert_eq!(resolved.live, Some(false));
        assert!(
            resolved
                .headers
                .contains(&("User-Agent".to_owned(), "Mozilla/5.0".to_owned()))
        );
        let merged = br#"{"title": "Live", "is_live": true, "requested_formats": [
            {"url": "https://cdn/a.m4a", "vcodec": "none"},
            {"url": "https://cdn/v.m3u8", "vcodec": "avc1"}]}"#;
        assert_eq!(
            parse_page_info(merged).expect("merged formats").uri,
            "https://cdn/v.m3u8"
        );
        parse_page_info(br#"{"title": "Nothing"}"#).expect_err("no stream");
    }

    #[test]
    fn preparing_checks_and_classifies() {
        let missing = Resolver {
            program: "wallplay-no-such-yt-dlp".to_owned(),
        };
        let prepared =
            Prepared::prepare(&Source::Uri("rtsp://camera/1".to_owned()), &missing).expect("media");
        assert_eq!(prepared.via, Via::Gstreamer);
        Prepared::prepare(&Source::Uri("video.mp4".to_owned()), &missing).expect_err("not a URL");
        Prepared::prepare(&Source::Pattern("plaid".to_owned()), &missing).expect_err("no pattern");
        // Without yt-dlp, an unknown page is tried as media, a known site is
        // refused with the reason.
        let unknown = Prepared::prepare(
            &Source::Uri("https://example.com/live".to_owned()),
            &missing,
        )
        .expect("tried as media");
        assert_eq!(unknown.via, Via::Gstreamer);
        let error = Prepared::prepare(
            &Source::Uri("https://www.twitch.tv/somechannel".to_owned()),
            &missing,
        )
        .expect_err("needs yt-dlp");
        assert!(error.to_string().contains("not installed"), "{error}");
        let whep = Prepared::prepare(&Source::Uri("http://h:8889/cam/whep".to_owned()), &missing)
            .expect("whep");
        assert_eq!(whep.input, Input::Whep("http://h:8889/cam/whep".to_owned()));
    }

    #[test]
    fn patterns_fill_in_the_canvas_and_parse() {
        let size = Size {
            width: 1920,
            height: 1080,
        };
        let checkerboard = pattern_launch("checkerboard", size).expect("pattern");
        assert!(
            checkerboard.contains("width=24,height=14"),
            "{checkerboard}"
        );
        assert!(
            checkerboard.ends_with("width=1920,height=1080"),
            "{checkerboard}"
        );
        assert!(
            pattern_launch("sweep", size)
                .expect("pattern")
                .ends_with("=21")
        );
        let missing = pattern_launch("plaid", size).expect_err("no such pattern");
        assert!(missing.to_string().contains("plaid"), "{missing}");
        assert!(pattern(DEFAULT_STANDBY).is_some());
        gst::init().expect("GStreamer");
        for pattern in PATTERNS {
            let description = pattern_launch(pattern.id, size).expect("pattern");
            gst::parse::bin_from_description(&description, true)
                .unwrap_or_else(|error| panic!("{}: {error}", pattern.id));
        }
    }
}
