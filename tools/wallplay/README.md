# wallplay

`wallplay` is a browser playback demo for irregular video walls. Every display
is a web browser that opens one URL with its own ID. A control page shows the
calibration patterns, takes a photo of the wall, makes a layout, and plays one
video across all displays: a test pattern, a file, a live stream or network
camera, or a web page such as YouTube, with its sound on one display. Each
display's part comes from its own [`wallwarp`](../../video/wallwarp/) branch
and reaches its browser through `webrtcsink`. A measurement mode reports how
far apart the browsers present the same frame, and can align them.

It is a test application, not a video wall manager: the server, the pages,
and the signalling choices stay out of the `wallwarp` element, and the
calibration runs [`wallcalibrate`](../wallcalibrate/)'s steps from the
browser.

## Requirements

- `webrtcsink`, from gst-plugins-rs's `rswebrtc` plugin. Some GStreamer
  packages include it; otherwise build it from
  [gst-plugins-rs](https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs).
- libnice's GStreamer elements, which `webrtcbin` needs for ICE, often
  packaged apart from GStreamer itself. `wallplay` refuses to play without
  them.
- For YouTube and some other web pages,
  [yt-dlp](https://github.com/yt-dlp/yt-dlp) on the `PATH`, or named with
  `--yt-dlp`. For WHEP endpoints, `whepclientsrc`, also from `rswebrtc`.
- For sound, GStreamer's Opus encoder, `opusenc`, and `audiobuffersplit`
  from gst-plugins-bad.
- Browsers with WebRTC VP8 and `requestVideoFrameCallback`: current Chrome,
  Edge, Safari, and Firefox. The displays and the server must reach each
  other on the local network; no STUN or TURN server is used.

## Quick start

```sh
cargo run --release -p wallplay
```

It prints the control page's address, for example `http://192.168.1.20:8000/`.

1. **Register displays.** On every display, open
   `http://192.168.1.20:8000/display/<id>` with a stable ID, such as
   `/display/lobby-left`, and tap or click the page to go fullscreen. Opening
   `/display` asks for an ID and remembers it in that browser. The control
   page lists every display with its viewport, fullscreen state, and stream.
2. **Show the patterns.** Every online display gets AprilTag markers sized to
   its current viewport, so make every display fullscreen first. A display
   whose viewport changes afterwards says so on its screen.
3. **Photograph the wall** with **Take photo**, which opens a phone's camera
   directly when the control page is open on the phone, or **Choose photo**
   (JPEG or PNG). The photo is calibrated as soon as it arrives: the server
   detects the markers and fits every display, as `wallcalibrate detect` and
   `solve` do, and reports each display `complete`, `partial`, or `missing`.
   **Open corner editor** shows the photo with the result; drag corners or
   place a missing display, then press **Apply**.
4. **Make the layout**; see [Layouts](#layouts). Once a layout exists, every
   change to its options makes a new one.
5. **Play.** Every display in the layout connects to its stream and shows it
   stretched over the whole page, exactly where its pattern was. Pick a test
   pattern or a URL under **Source**; see [Sources](#sources). A new source,
   or a new layout of the same displays, plays at once without the displays
   reconnecting.
6. **Measure and align** while playing; see [Measuring drift](#measuring-drift)
   and [Aligning displays](#aligning-displays).

### Options

```text
--listen ADDRESS:PORT    control and display pages [default: 0.0.0.0:8000]
--state DIRECTORY        calibration files [default: wallplay-state]
--source PATTERN|URL|PIPELINE  what to play first [default: bars]
--standby PATTERN        shown whenever the source is not [default: slate]
--play-once              leave the standby showing when the source ends
--sound DISPLAY          play the sound on this display; without it, no sound
--yt-dlp PROGRAM         finds the video of web pages [default: yt-dlp]
--fps N                  canvas frame rate [default: 30]
--max-output PIXELS      longest side of each display's stream [default: 1920]
--signalling-port PORT   first port of the per-display signalling servers [default: 8443]
--signalling-uri URI     one external signalling server for every display
```

`--source` takes a test pattern's ID, a URL (see [Sources](#sources)), or a
`gst-launch-1.0` description with one video output and at most one audio
output, such as `"videotestsrc is-live=true pattern=snow"`. The control page
can switch between patterns and URLs, but not set a pipeline description:
over an unauthenticated page, a pipeline could write files.

The state directory keeps `manifest.json`, the photo, `detections.json`,
`calibration.json`, and `layout.json` under `wallcalibrate`'s names, so a
restarted server plays the last layout, and the files work with the
`wallcalibrate` command line.

Each display's stream has its viewport's aspect ratio, at most
`--max-output` pixels on the longer side and in CSS pixels, so a high-DPI
display at 2x scaling gets a stream at half its device resolution.

## Sources

A URL under **Source** opens by what it looks like, or as **Open as** says:

| URL | Opens with | Examples |
|---|---|---|
| Media: a file, or any URL whose scheme is not HTTP(S) | `uridecodebin` | `file:///Users/me/video.mp4`, `rtsp://camera.local/stream1`, `srt://10.0.0.5:7001`, `rtmp://…` |
| HTTP(S) ending in a media extension | `uridecodebin` | `https://…/video.mp4`, HLS `…/index.m3u8`, DASH `…/manifest.mpd`, MJPEG `…/video.mjpg` |
| HTTP(S) ending in `/whep` | `whepclientsrc` (WebRTC-HTTP egress) | `http://mediamtx:8889/cam/whep` |
| Any other HTTP(S) page | yt-dlp, then `uridecodebin` | YouTube videos and live streams, Vimeo, and [many more](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md) |

Their sound plays when it is turned on, except WHEP's; see [Sound](#sound).

- **Web pages** take a few seconds to look up. yt-dlp picks video up to 1080
  lines, H.264 where it can, since it decodes in hardware on most machines.
  `wallplay` sends the headers yt-dlp reports with every request, as YouTube
  refuses its streams without them. The stream URLs it finds expire after
  some hours, so a page's source that fails is looked up again before
  retrying. Without yt-dlp, a URL on an unknown site is tried as media.
- **RTSP** uses a 200 ms jitter buffer instead of `rtspsrc`'s two seconds.
- **The standby** pattern shows while the first source connects, after a
  source ends without restarting, and when one fails or sends no frame for
  two seconds. The default `slate` says which of these it is, with the
  source's name or error. Any test pattern can be the standby.
- **Restarting**, on by default: a clip that ends plays again, its last frame
  staying until the next run's first, and a stream that ends reconnects. A
  source that fails, or sends no frame for ten seconds, retries after 1, 2,
  5, then every 10 seconds, with the reason on the control page and the
  slate.
- **Changing the source** while playing swaps it inside the running pipeline.
  The displays keep their WebRTC sessions and the old source until the new
  one's first frame, then cut to it; if the new one fails, or the old one
  stops for two seconds first, the standby shows. Changing the standby or the
  restart setting applies at once too.

Each source is a bin feeding an `input-selector` beside the standby's.
`wallplay` itself picks what shows, from the frames each bin sends, rather
than leaving it to `fallbackswitch`. That keeps it to GStreamer's base
plugins, and it keeps each failure's message, which `fallbacksrc` hides. The
bins handle their own state changes, so a source that is still prerolling, or
never will, cannot pause the pipeline and freeze the standby with it. A
source that is not live is moved to the clock on its first frame and paced in
real time by `clocksync`; live sources, such as RTSP, SRT, WHEP, and test
patterns, pass without waiting for their latency a second time.

Tested against MediaMTX 1.21 (RTSP, HLS, SRT, and WHEP with H.264
and VP8), local files, a YouTube video, and a live channel on another site.

### Test patterns

| ID | Pattern | Shows |
|---|---|---|
| `bars` | Moving color bars (default) | Sync and color: bar edges break across a gap when one display lags |
| `sweep` | A vertical bar crossing the canvas in about three seconds | Sync between side-by-side displays: the bar jumps when one lags |
| `ball` | A ball circling once per second | Motion across displays |
| `spokes` | Lines from the canvas center | Geometry: lines stay straight across gaps |
| `checkerboard` | Squares of about 80 canvas pixels | Geometry and scale: squares stay square and in line |
| `timecode` | Moving color bars with a large running timecode | Sync by photo: displays that show the timecode should agree |
| `slate` | Dark gray with what the source does (the default standby) | That the wall plays, and why the source is not showing |
| `black` | Black | Nothing: the displays go dark |

The timecode and the slate's text sit in the canvas center, so they show
only when a display covers the center. Both need GStreamer's Pango plugin,
which some systems package separately; without it the timecode pattern shows
plain bars and the slate a plain dark screen.

## Sound

Pick the display that plays the sound under **Sound** on the control page, or
with `--sound`. Every display's stream then carries the sound, and every
display but that one keeps it muted: a browser holds a stream's video back to
keep it in sync with its sound, muted or not, so displays that all receive
it stay in step. Turning the sound on or off restarts playback, since every
stream gains or loses its audio; moving it to another display only unmutes
that one.

- **What sounds**: files and streams with sound, web pages, whose sound
  yt-dlp often finds apart from the video and which then plays from a second
  URL, and pipelines with an audio output. WHEP endpoints play without sound,
  since servers such as MediaMTX refuse an offer with audio for a stream
  without it. Test patterns, the standby, and sources without sound play
  silence.
- **In sync**: a source's sound moves to the clock so that it plays the
  video's first timestamp when the video does. A clip's sound and picture
  start together; a live page's video and audio playlists, fetched apart,
  may start a segment apart, but their timestamps share one timeline.
- **Browsers** play sound only after a tap, click, or key press on the page,
  unless set up to allow it, such as Chrome with
  `--autoplay-policy=no-user-gesture-required`. Until then the display plays
  muted and says so.

With two headless Chromium displays on one machine showing a test pattern,
sound raised the median latency from 91 to 113 ms over four runs each, the
browsers holding the video back 9–37 ms for lip sync. Each browser buffers
the sound for itself, so the displays, all but level without sound, stayed
within one 60 Hz refresh of each other; [aligning](#aligning-displays)
corrects the rest.

Sound is converted to 48 kHz stereo, since `webrtcsink` cannot change an
audio format while playing, and cut into 20 ms buffers, one Opus frame each.
Browsers then receive it evenly and buffer less of it, which their lip sync
makes the video wait for: in 21.3 ms buffers, the video waited 46–57 ms
instead of 30–35 ms. The silence is paced like a clip rather than live:
`webrtcsink` sends each frame at its running time plus the latency GStreamer
configures for every sink when the pipeline starts, and a live audio source
raised that latency by its 21.3 ms buffer, the video's too.

## Layouts

- **Mode**: viewpoint or planar, as described in
  [`wallcalibrate`](../wallcalibrate/README.md#viewpoint-and-planar-modes).
- **Anchor**: the display the layout aligns to. In planar mode it is the
  reference: it shows a plain crop of the video, unwarped, and every other
  display is placed on its plane. In viewpoint mode it is optional and levels
  the layout to that display's edges, removing the camera's tilt, while the
  rest of the camera's perspective stays.
- **Straighten**: for displays meant to be upright and in line. Every display
  becomes an upright rectangle with its own aspect ratio, turned only in
  quarter turns, with the center and area it has in the photo, so each shows
  a plain crop. Small rotations and perspective, from the photo or the
  mounting, are dropped: from the camera's spot, lines no longer meet exactly
  across gaps, which the simulation shows.
- **Canvas**: *exactly the displays* (the default) makes the canvas the
  displays' extent, its longer side the given size, so no part of the video
  falls outside every display. *A fixed size* centers the displays in it.
- **Video**: *fills the displays* (the default) crops a video whose shape
  differs from the canvas, so the displays show no empty bars; *shows whole*
  scales it in with black bars. Test patterns are drawn at the canvas size and
  always fill it.

A new layout applies while playing, and playback does not restart:

- Each display's `wallwarp` takes its new corners with the first frame of the
  new canvas size, and the source is converted to that size in place, so a
  clip does not start over. Test patterns restart at the new size, and so does
  a source whose **Video** fill changed.
- A display whose stream size changed keeps its WebRTC connection: its encoder
  starts again at the new size, and the browser follows from the next key
  frame.
- A display the layout leaves out goes back to waiting, and one it adds starts
  a stream of its own. The other displays keep their connections, though they
  can pause briefly while a new display's stream starts: its codec discovery
  and first encoder compete with theirs for the CPU.

## Simulation

The simulation, under the layout, shows the photo with every playing
display's output drawn onto its outline through the same perspective the
camera saw: what the camera would see from where the photo was taken. A
viewpoint layout continues lines across gaps from there; straightening or a
planar layout of displays that are not quite in one plane shows where it
does not. Previews come from the playback pipeline, scaled to 480 pixels and
at most four frames per second, so the displays must be playing; the
simulation follows a new layout within seconds.

## Signalling

Browsers speak the protocol of `webrtcsink`'s signalling server directly, in
about a hundred lines of `display.html`: they register as listeners, find the
producer whose metadata names their display, and answer its offer. There is no
other signalling layer.

Every display's `webrtcsink` is a separate producer with
`meta="meta,display=<id>,generation=<n>"`, and encodes separately for each
browser that connects. By default every sink runs its own built-in signalling
server on its own port, the first free ones from `--signalling-port`, so a
firewall must allow that range. One shared built-in server does not work:
`webrtcsink`'s signalling client connects once without retrying, so sinks
cannot depend on a server another sink starts. With
`--signalling-uri ws://HOST:8443`, every display uses one external server
instead, such as `gst-webrtc-signalling-server`, which must be running before
playback starts and reachable by the browsers at that same address.

## Measuring drift

**Start measuring** records when every display's output frame leaves the
pipeline, by frame number from the frame's running time. Every browser tells
which frame it presents with `requestVideoFrameCallback`: Chrome and Edge
report each frame's RTP timestamp, which `wallplay` makes the running time at
90 kHz by setting every payloader's `timestamp-offset` to zero, so no mark
on the picture is needed. For other browsers, the pipeline draws a 12-bit
frame number into the bottom-left corner of every output, and they read it
back; it is drawn until every display has reported RTP timestamps. Each
browser takes its expected presentation time, converts it to the server's
clock, and reports it once a second. Clock synchronization keeps the fastest
of the last 30 seconds of HTTP round trips, over a kept-alive connection, and
frames are recorded only after the first five. The control page then shows,
over the last 20 seconds:

- **latency**, from a frame's capture time, in steps: **Pipeline** until it
  is warped and ready to encode, **Arrives** until its last packet reaches
  the browser, and **On screen** until the browser expects it on screen.
  **Jitter buffer** is the browser's own buffering, from its WebRTC
  statistics, and part of the last step. The later steps add the pipeline's
  median to times measured from stamping, so they are approximate;
- **offset**: a display's presentation time minus the reference display's,
  for the same frame, with its median and 95th percentile;
- **offset change**: the median offset over the window's last quarter minus
  its first. Presentation times fall on display refreshes, so a display that
  slips by a frame shows as one refresh interval, about 16.7 ms at 60 Hz;
- **spread**: latest minus earliest presentation of each frame that every
  measured display presented.

`/api/measure` returns the same as JSON.

These are the browsers' own estimates of when frames reach the display. They
leave out the panel's latency, which differs between devices, and they are
only as good as the clock synchronization, about half the best round trip,
shown as **Clock ±**. Checking the result against reality needs a high-speed
camera filming all displays.

WebRTC does not synchronize independent receivers: each browser runs its own
jitter buffer and decoder, and `webrtcsink` encodes separately for each.
Before aligning, two headless Chromium displays on one machine, measured over
20-second windows, presented most frames within one 60 Hz refresh of each
other:

| Machine | Latency median | Spread median / p95 / max |
|---|---|---|
| Lightly loaded | 50–70 ms | 0.1–0.3 / 17–33 / 17–333 ms |
| Busy (load average 20) | 105–125 ms, about 17 of 30 frames presented | 17 / 34 / 100 ms |

The offset between the two moved by whole refreshes, as one display slipped
a frame and later caught up. Real devices on Wi-Fi will differ: a laptop on
Wi-Fi driving its built-in and an external display showed frames about
130–140 ms after stamping, with about 47 ms until arrival.

### Aligning displays

**Align displays** measures as above and corrects what it measures. Every
few seconds, once each display has presented 45 frames since the last
change had two seconds to settle, it takes each display's median offset from
the reference and raises the jitter buffer target of every display that
presents earlier than the latest one by 70% of its lag. The smallest added
delay is kept at zero, so aligning adds no latency beyond the latest
display's own, and a display that became the late one has its delay reduced
instead. Displays within 10 ms of the latest count as aligned: presentation
falls on each display's refreshes, so displays whose refreshes are out of
phase cannot agree closer than half a refresh, and a tighter tolerance would
hunt between two equally good refreshes. The added delays show as **Added
delay**, on top of the **Browser buffer** setting.

In the browser check, one of two headless Chromium displays is made 100 ms
late on purpose by raising its own jitter buffer target; aligning delayed the
other by about 94 ms and brought them from about 100 ms to 0.2 ms apart in
two or three steps, 15 to 30 seconds.

Aligning needs browsers that honor `RTCRtpReceiver.jitterBufferTarget`:
Chrome, Edge, and Firefox do; Safari ignores it, so a Safari display can only
be the late one that others wait for. It cannot make a display earlier than
its own jitter buffer allows. It is only as exact as each display's clock
synchronization, **Clock ±**, and panels add their own latency on top. A
display that stalls or restarts shifts again, and aligning keeps correcting
while it runs.

### Reducing latency

- **Browser buffer** on the control page sets the browsers'
  `jitterBufferTarget`, applied while playing without reconnecting. A lower
  target cuts latency where the network is jittery, at the cost of stutter;
  Chrome and Firefox support it, Safari ignores it. On one machine the
  browser's jitter buffer was already 5–9 ms, so it changed nothing there.
- Put the server on wired Ethernet, so only the displays use Wi-Fi.
- Built-in patterns skip `videorate`, which holds a frame to match frame
  rates: the time in the pipeline fell from about 19 to 11 ms. Files and
  other sources keep it.
- About two refreshes between arrival and presentation are the browser's
  decoding and compositing, and remain.

## Evaluation of webrtcsink

Tested with GStreamer 1.28.8 and `rswebrtc` 0.15.4:

- **Works:** one producer per display with its ID in the metadata, built-in
  or external signalling, per-browser encoding and congestion control, and
  reconnecting browsers.
- **H.264 fails on macOS:** `webrtcsink` picks `vtenc_h264`, and its codec
  discovery rejects that encoder's output (`not-negotiated`), which fails the
  whole pipeline, not one session. `wallplay` offers only VP8, which every
  platform encodes and every browser plays.
- **Stale signalling connections:** when a sink stops, its built-in signalling
  server stops accepting connections but keeps the open ones, which never see
  a producer again. Browsers therefore reconnect when their session ends or
  their producer does not appear within five seconds.
- **Teardown deadlock:** stopping a sink while a session is still applying
  the browser's answer, as right after a browser connects, can deadlock
  inside `webrtcsink`, and the pipeline never stops. `wallplay` waits 1.5
  seconds after the last browser connected before stopping a pipeline, which
  avoided the deadlock in every run of the reload-then-restart scenario that
  had hit it 3 times in 4. If a pipeline gets stuck anyway, it is stopped in
  the background without blocking the server; its source and frame stamping
  stop, and the next pipeline's signalling servers move to free ports and a
  new generation. The control page counts stuck pipelines; their threads stay
  until `wallplay` restarts.
- **mDNS candidates:** Chrome hides local addresses behind `.local` names,
  which libnice cannot resolve and logs as errors. Connections still succeed
  on a local network through the browser's own connectivity checks.
- **Mitigation modes:** `webrtcsink` lowers resolution and frame rate under
  congestion by default; `wallplay` turns that off, so every display keeps its
  picture.
- **Audio inputs:** a sink announces its producer only after every input has
  had data, so an audio input needs silence when there is no sound. It
  refuses any change of an audio input's caps, even a channel mask that the
  silence's caps leave out and a converted source's carry, and its sound then
  stops. `webrtcbin` gives every track the session's CNAME as its stream ID,
  so browsers lip-sync a sink's audio and video without further setup.

## HTTP API

| Request | Purpose |
|---|---|
| `GET /`, `GET /display[/ID]` | Control page, display page |
| `GET /api/displays/ID/poll?since=N&epoch=&w=&h=&scale=&fullscreen=&player=` | Long poll: report the viewport, receive what to show; answers at once when `epoch` is from an earlier run of the server |
| `POST /api/patterns` | Make a manifest from the online displays' viewports and show it |
| `POST /api/photo` | Upload a photo (body: JPEG or PNG), detect, and solve |
| `GET`, `PUT /api/calibration` | The calibration; `PUT` replaces it after validation |
| `GET /calibration` | Corner editor |
| `POST /api/layout` | `{"mode", "anchor", "reference_size", "straighten", "canvas", "fill", "allow_incomplete"}`; `canvas` is `[width, height]` or `{"longest": pixels}`, `fill` is `cover` or `contain` |
| `GET /api/photo.jpg`, `GET /api/displays/ID/preview.jpg` | The photo, reduced, and a display's latest output, for the simulation; the preview is `204 No Content` before a display's first frame |
| `GET /api/layout`, `/api/layout.svg`, `/api/manifest` | Files and preview |
| `POST /api/play`, `POST /api/idle` | Start or stop playback |
| `POST /api/source` | `{"pattern": ID}`, or a URL as `{"uri": URL}` to open it by its look, `{"media": URL}`, `{"page": URL}` or `{"whep": URL}`; switches while playing |
| `POST /api/playback` | `{"restart": bool, "standby": ID}`, either one; applies while playing |
| `POST /api/buffer` | `{"ms": N}` or `{"ms": null}`: the browsers' jitter buffer target |
| `POST /api/sound` | `{"display": ID}` or `{"display": null}`: the display that plays the sound; turning it on or off restarts playback |
| `POST /api/measure`, `GET /api/measure` | `{"on": bool}`; measurement summary, with the aligner's progress while aligning |
| `POST /api/align` | `{"on": bool}`: align the displays' presentation while playing |
| `POST /api/clock`, `POST /api/displays/ID/frames` | Used by display pages while measuring |
| `GET /api/state`, `DELETE /api/displays/ID` | Everything the control page shows; forget an offline display |

The server is a small HTTP/1.1 implementation on the standard library and
`httparse`, with one thread per connection, kept-alive connections, and no TLS
or authentication; run it only on a trusted network.

## Development

```sh
cargo test -p wallplay
cargo clippy -p wallplay --all-targets -- -D warnings
```

The tests check the frame code, RTP timestamps, drift statistics, the
aligner, HTTP parsing, and how URLs open, and run the whole workflow over
HTTP against a real pipeline with `appsink`s in place of `webrtcsink`. That
covers registration, patterns, a synthetic photo, corrections, layout,
playback, frame codes matching running times, measurement and aligning from
simulated browser reports, a new layout while playing, a source that ends,
restarts, fails, and is replaced while playing, sound that follows the source
with its picture's offset and turns to silence on standby, and sound moving
between displays.

With `webrtcsink`, libnice, `uv`, and Playwright's Chromium installed, a full
check runs the real pages in headless Chromium:

```sh
mise run //tools/wallplay:browser:check
```

It starts `wallplay`, opens a landscape and a portrait display in separate
browsers, calibrates them from a synthetic photo, applies the editor's result,
plays, and measures. It applies a new layout while playing and checks that
the displays keep their streams, then reloads one display, restarts
playback, and checks that both present frames again. It switches to
another pattern and to a clip played once from the control page, and checks
that the displays keep their streams and show the standby when it ends. It
plays a clip with a tone and its sound on one display, checks that both
displays receive the tone and only that one plays it, moves the sound to the
other display, and turns it off. Finally it makes one display 100 ms late and
checks that aligning brings both together. `--signalling-server` repeats it
with an external `gst-webrtc-signalling-server`, and `--output DIR` keeps
screenshots.

## Limitations

- One source and one layout at a time.
- Sound plays on one display, and every display's video waits for it; WHEP
  sources play without it.
- No local cameras or screen capture; a camera on the network plays over
  RTSP, SRT, or WHEP.
- Patterns come from `videotestsrc`; there is no pattern drawn per display,
  such as a timecode on every screen.
- The simulation shows the photographed position only; it cannot show how the
  wall looks from anywhere else.
- Filling the displays with a video needs `videocrop` from
  gst-plugins-good.
- The measurement covers the browsers, not the panels, and aligning
  corrects only what the browsers report.
- No authentication or TLS; browsers that require a secure context for the
  Screen Wake Lock API get none, so turn off the display's own sleep and
  auto-lock, which also drop it out of fullscreen.
- Drift and aligning are tested with headless Chromium on one machine; on
  real devices, only latency has been measured, on one laptop.
