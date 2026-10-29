# wallcalibrate

`wallcalibrate` turns one photo of a video wall into a
[`wall-geometry`](../../utils/wall-geometry/) layout for
[`wallwarp`](../../video/wallwarp/). Every display shows AprilTag `tag36h11`
markers at positions listed in a manifest. The tool finds them in the photo,
fits each display's corners from its own markers, and maps the result onto a
canvas.

The tool is experimental. It is tested with synthetic photos rendered through
known perspective transforms and with screenshots, and has been tried on real
phone photos; see [Limitations](#limitations). It needs no GStreamer, browser,
or network at run time. The optional OpenCV detector needs Python with `uv`.

## Workflow

Run from the repository root; `cargo build --release -p wallcalibrate` puts the
binary at `target/release/wallcalibrate`. [`wallplay`](../wallplay/) runs the
same steps from a browser control page, with displays that register
themselves, and then plays video on them.

1. **Register displays.** A manifest gives every display a stable ID, its
   output resolution, and four corner tags. Tag numbers are unique across the
   wall:

   ```sh
   wallcalibrate manifest \
     --screen display-a:1280x720 \
     --screen display-b:1920x1080 \
     --screen display-c:1024x576 \
     -o manifest.json
   ```

   `--layout LAYOUT.json` takes the IDs and sizes from an existing layout
   instead. `--marker-size` (default `0.3`) and `--inset` (default `0.04`) are
   fractions of each display's shorter side; `--first-tag` sets the first tag
   number.

2. **Show the patterns.** `pattern` writes `<id>.html` and `<id>.svg` for every
   display, and an `index.html` linking them. Open each display's page on that
   display and click it to go fullscreen; the page must cover the whole screen.
   Each page shows its viewport size, in red with a suggested `--screen` value
   when its aspect ratio differs from the manifest; a mismatch stretches the
   tags and gives planar mode a wrongly shaped reference. `--png` also writes
   `<id>.png` at the display's resolution, for players that show images
   rather than web pages.

   ```sh
   wallcalibrate pattern manifest.json -o patterns
   ```

3. **Photograph the wall** with every display in view. HEIC photos, which
   many phones take by default, must be converted to JPEG or PNG first, for
   example with libheif's `heif-convert IMG_1234.HEIC photo.jpg`.

4. **Detect markers:**

   ```sh
   wallcalibrate detect photo.jpg -o detections.json
   ```

5. **Fit the displays and preview the result:**

   ```sh
   wallcalibrate solve manifest.json detections.json \
     -o calibration.json --preview preview.html
   ```

   Each display is reported `complete`, `partial` (some markers missing or
   rejected), or `missing`, with the root-mean-square marker error in photo
   pixels. `preview.html` draws the photo with every display's outline, ID, and
   status, and every used (green), rejected (red), and unknown (dashed orange)
   marker.

6. **Fix by hand if needed.** In the preview, drag a display's corner handles,
   or press **Place** for a missing display and drag it into position, then
   **Download calibration.json**. Edited displays become `manual`. Editing
   `corners` in `calibration.json` directly works too.

7. **Generate the layout**, in one of two modes described below:

   ```sh
   wallcalibrate layout calibration.json \
     --mode planar --reference display-a --canvas 1920x1080 \
     -o layout.json --preview layout.svg
   ```

   `--canvas WIDTH` sets the canvas width and derives its height from the
   displays' extent; `--canvas WIDTHxHEIGHT` fits and centers them, which suits
   a source video of that size. `layout.svg` draws the canvas with every
   display's outline and its own top edge.

   Two options adjust the result. `--level ID`, in viewpoint mode, rotates the
   layout so that display's edges are horizontal and vertical, to the nearest
   quarter turn, removing the camera's tilt. `--straighten` assumes the displays
   were meant to be upright and in line: each becomes an upright rectangle with
   its output's aspect ratio and its center and area from the photo, turned only
   in quarter turns, so it shows a plain crop of the source.

Then distribute video with the layout, for example with `wallwarp`'s
`wall-demo` example:

```sh
cargo run --release -p gst-plugin-wallwarp --example wall-demo -- layout.json --print-launch
```

### Try it without a camera

`synth` renders a synthetic photo of a layout's displays showing their
patterns, and writes the exact calibration for comparison:

```sh
wallcalibrate manifest --layout video/wallwarp/examples/three-screens.json -o manifest.json
wallcalibrate synth manifest.json video/wallwarp/examples/three-screens.json \
  -o photo.png --truth truth.json --black 40 --noise 4
```

`--view X,Y,…` places the canvas corners in the photo (by default, a mild
perspective), `--size` sets the photo size, `--black`, `--blur`, and `--noise`
degrade it, and `--hide-tag` covers a tag.

## Testing with real displays

**Browser windows on one desktop.** Give each window its own manifest entry,
open each pattern page in its own window (not a tab), arrange and resize the
windows freely, and take a screenshot of the whole screen. Every display should
come out `complete` with a sub-pixel error, and the preview outlines should sit
on each window's content area (scaled by the display's pixel ratio on high-DPI
screens). A screenshot has no perspective or lens, so this checks detection and
the pipeline, not a camera. Photographing the monitor with a phone instead adds
the camera while the truth stays known: in planar mode with `--reference-size`
set to a window's content size in CSS pixels, every window should come out at
its real position and size.

**Two laptops side by side.** Give the manifest each laptop's fullscreen
viewport, as its pattern page reports it. Serve the patterns to the second
laptop with `python3 -m http.server 8000 --directory patterns`, make both
pages fullscreen, and photograph both screens with the phone's main lens from
1 to 2 m, screens at moderate brightness and no lights reflecting in them.
Then check:

- planar mode with `--reference-size` measured with a ruler: two laptops of
  the same model come out the same size, and the gap between them matches
  the ruler; this needs both lids at the same angle;
- the result itself: render frames with
  `wall-demo LAYOUT.json --pattern pinwheel --html`, show each
  `screen-<id>.html` fullscreen on its laptop, and check that the lines
  continue across the gap, seen from the camera position for a viewpoint
  layout or from straight in front for a planar one.

## Viewpoint and planar modes

One photo supports two different claims, so the mode is explicit:

| Mode | Assumes | Result |
|---|---|---|
| `viewpoint` (default) | Nothing about the displays' arrangement | The canvas is the photo, cropped and scaled. The video looks continuous from where the photo was taken, and perspective-distorted elsewhere. Works for screens at any angle or depth. |
| `planar` | Every display lies in one plane, and `--reference` has a known aspect ratio | The camera's perspective is removed: the reference display becomes an upright rectangle and every other display is mapped through the same plane. The video looks right from straight in front of the wall. |

Planar mode takes the reference display's aspect ratio from its output size,
which assumes square pixels, or from `--reference-size WIDTHxHEIGHT`, the
measured size of its picture area. Only a measured size makes the canvas
physical; the tool then reports units per canvas pixel. A single photo
cannot recover physical dimensions on its own, and a display off the
reference plane is rejected when it maps to an impossible shape, or lands
in the wrong place when it does not.

## Incomplete calibration

A display needs at least `--min-markers` detected tags (default `2`); with
fewer, it is reported `missing` and gets no corners. Positions are never
guessed. A marker that disagrees with its display's other markers by more than
2 px and 5% of its own size is rejected, and duplicate detections keep the most
confident one. `solve` exits with status `2` when any display is missing.
`layout` then refuses unless the missing displays get corners by hand, or
`--allow-incomplete` leaves them out.

## Detectors

The built-in detector, [kornia-apriltag](https://crates.io/crates/kornia-apriltag)
0.1.10, is pure Rust. It only finds tags whose cells are a few pixels wide
after shrinking the photo, so it searches at several shrink factors, each on
a thread of its own, and keeps each tag from the finest one; that handles
full-size phone photos and screenshots alike. `scripts/detect_opencv.py`
writes the same detections format with OpenCV, which copes better with
degraded photos:

```sh
uv run --with opencv-python-headless==4.12.0.88 \
  python tools/wallcalibrate/scripts/detect_opencv.py photo.jpg -o detections.json
```

On the synthetic three-display photo:

| Photo | Built-in | OpenCV |
|---|---|---|
| Clean | 12 of 12 tags, 0.04 px RMS | 12 of 12, 0.06–0.11 px |
| Gray black level 60, blur, noise ±12 | 12 of 12, under 0.5 px RMS | 12 of 12 |
| Black level 100, heavy blur, noise ±20 | 1 of 12 | 12 of 12, under 0.25 px RMS |

On a real 24-megapixel phone photo of a laptop and a tablet, both found all
8 tags, and the fitted display corners agreed within 0.32 px.

Both apply the photo's EXIF orientation, and both report corners with pixel
`(i, j)` covering `[i, i + 1) × [j, j + 1)`.

## Files

All files are versioned JSON; coordinates are `[x, y]` pairs and corner lists
run top-left, top-right, bottom-right, bottom-left.

- `manifest.json`: per display, its `id`, `output` size, and `markers`, each a
  `tag` and the corners of its black border in display-local coordinates
  (`[0, 1]²` across the output, in the display's own orientation).
- `detections.json`: the `photo` size and every detected `tag` with its
  corners in photo pixels, in the tag's own orientation, plus optional
  `hamming` and `margin` confidence fields.
- `calibration.json`: per display, its `status`, `corners` in photo pixels
  (`null` when missing), `rms_error` and `max_error` in pixels, each marker's
  state (`used`, `rejected`, or `missing`) and error, and `notes`; plus
  `unknown_markers`, detected tags that no display owns.
- `layout.json`: a `wall-geometry` layout.

## Photo tips

- Keep display brightness moderate, so whites do not bloom over the black
  marker borders, and avoid glare across the markers.
- Make each marker at least about 40 photo pixels across; use larger
  `--marker-size` for distant or small displays.
- Use the camera's main lens and keep displays away from the photo's edges:
  lens distortion is not modelled and bends straight display edges.
- For planar mode, photograph from roughly in front of the wall; the
  perspective is removed, but very oblique views lose detail on the far side.

## Limitations

- One photo per calibration; occluded displays must be placed by hand.
- No lens distortion model, so wide-angle photos add error.
- Accuracy is measured only on synthetic photos and screenshots. Real phone
  photos of laptops, a tablet, and an external display have been calibrated,
  without an independent truth.
- Generated manifests place four tags per display, one in each corner.
  Hand-written manifests may place any number of tags per display, each a
  parallelogram (normally a square) in any rotation.

## Development

```sh
cargo test -p wallcalibrate
cargo clippy -p wallcalibrate --all-targets -- -D warnings
```

The tests render patterns and decode them with the built-in detector, solve
synthetic photos of the three-display example and of a coplanar wall in strong
perspective, and check incomplete calibrations, manual corrections, and both
layout modes. Check the OpenCV detector against the same synthetic photos,
clean and heavily degraded, with:

```sh
mise run //tools/wallcalibrate:detector:opencv
```
