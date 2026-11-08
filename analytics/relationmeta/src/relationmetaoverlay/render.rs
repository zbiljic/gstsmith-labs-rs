//! Draws a scene into a `VideoOverlayComposition` with cairo and pango, in
//! the style of the Cactus web demo: polygons, boxes, relation arrows, label
//! chips, then the optional text and caption chips.

use gst::glib;

use super::layout::{self, Rect};
use super::scene::{Mask, Object, Scene};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstRelationMetaOverlayPalette")]
pub enum Palette {
    /// The demo's six pastels, picked by a hash of the label.
    #[default]
    #[enum_value(name = "Pastels by label, as the Cactus demo", nick = "pastel")]
    Pastel,
    /// Distinct colours per track id; untracked objects use pastels.
    #[enum_value(name = "Distinct colours per track id", nick = "tracking")]
    Tracking,
    /// Everything in the foreground colour.
    #[enum_value(name = "One colour", nick = "mono")]
    Mono,
}

#[derive(Clone, Debug, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per drawn layer, mirroring the element properties"
)]
pub struct Style {
    pub draw_masks: bool,
    pub draw_boxes: bool,
    pub draw_relations: bool,
    pub draw_labels: bool,
    pub draw_text: bool,
    pub draw_caption: bool,
    pub font: String,
    pub line_width: f64,
    /// Opacity of polygon fills; the outline is at least `POLYGON_STROKE`.
    pub polygon_opacity: f64,
    /// Opacity of segmentation mask fills.
    pub mask_opacity: f64,
    pub palette: Palette,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            draw_masks: true,
            draw_boxes: true,
            draw_relations: true,
            draw_labels: true,
            draw_text: false,
            draw_caption: false,
            font: "monospace 14".to_owned(),
            line_width: 1.0,
            polygon_opacity: 0.10,
            mask_opacity: 0.20,
            palette: Palette::Pastel,
        }
    }
}

const POLYGON_STROKE: f64 = 0.35;
const BOX_STROKE: f64 = 0.75;
const ARROW_STROKE: f64 = 0.75;
const CHIP_FILL: f64 = 0.90;
const DIVIDER: f64 = 0.50;
const PAD_X: f64 = 6.0;
const PAD_Y: f64 = 2.0;
const MARGIN: f64 = 8.0;

impl Style {
    fn colour(&self, object: &Object) -> u32 {
        let by_label = || layout::label_colour(object.label.as_deref().unwrap_or("object"));
        match self.palette {
            Palette::Pastel => by_label(),
            Palette::Tracking => object.track.map_or_else(by_label, layout::track_colour),
            Palette::Mono => layout::FOREGROUND,
        }
    }
}

fn source(cr: &cairo::Context, colour: u32, alpha: f64) {
    let (r, g, b) = layout::rgb(colour);
    cr.set_source_rgba(r, g, b, alpha);
}

/// Text measured once and drawn later.
struct Chip {
    parts: Vec<(String, f64, f64)>,
    width: f64,
    height: f64,
}

impl Chip {
    fn new(text_layout: &pango::Layout, parts: &[String]) -> Self {
        let mut measured = Vec::with_capacity(parts.len());
        let mut height: f64 = 0.0;
        for part in parts {
            text_layout.set_text(part);
            let (w, h) = text_layout.pixel_size();
            height = height.max(f64::from(h));
            measured.push((part.clone(), f64::from(w), f64::from(h)));
        }
        let gaps = f64::from(u32::try_from(measured.len().saturating_sub(1)).unwrap_or(0));
        let width = measured
            .iter()
            .map(|(_, w, _)| w + 2.0 * PAD_X)
            .sum::<f64>()
            + gaps;
        Self {
            parts: measured,
            width,
            height: height + 2.0 * PAD_Y,
        }
    }

    fn draw(
        &self,
        cr: &cairo::Context,
        text_layout: &pango::Layout,
        at: Rect,
        border: u32,
        line_width: f64,
    ) -> Result<(), cairo::Error> {
        cr.rectangle(at.x, at.y, at.w, at.h);
        source(cr, layout::BACKGROUND, CHIP_FILL);
        cr.fill_preserve()?;
        source(cr, border, 1.0);
        cr.set_line_width(line_width);
        cr.stroke()?;
        let mut x = at.x;
        for (index, (text, w, h)) in self.parts.iter().enumerate() {
            if index > 0 {
                source(cr, border, DIVIDER);
                cr.move_to(x + 0.5, at.y + PAD_Y);
                cr.line_to(x + 0.5, at.y + at.h - PAD_Y);
                cr.stroke()?;
                x += 1.0;
            }
            source(cr, layout::FOREGROUND, 1.0);
            cr.move_to(x + PAD_X, at.y + (at.h - h) / 2.0);
            text_layout.set_text(text);
            pangocairo::functions::show_layout(cr, text_layout);
            x += w + 2.0 * PAD_X;
        }
        Ok(())
    }
}

fn draw_mask(
    cr: &cairo::Context,
    mask: &Mask,
    colour: u32,
    opacity: f64,
) -> Result<(), cairo::Error> {
    let (Ok(width), Ok(height)) = (i32::try_from(mask.width), i32::try_from(mask.height)) else {
        return Ok(());
    };
    let stride = cairo::Format::ARgb32
        .stride_for_width(u32::try_from(width).unwrap_or(0))
        .map_err(|_err| cairo::Error::InvalidStride)?;
    let row = usize::try_from(stride).map_err(|_err| cairo::Error::InvalidStride)?;
    let (r, g, b) = layout::rgb(colour);
    let premultiply = |channel: f64| {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the value is in 0..=255"
        )]
        let byte = (channel * opacity * 255.0).round().clamp(0.0, 255.0) as u8;
        byte
    };
    let pixel = u32::from_be_bytes([
        premultiply(1.0),
        premultiply(r),
        premultiply(g),
        premultiply(b),
    ])
    .to_ne_bytes();
    let mut data = vec![0u8; row * mask.height];
    for (y, out) in data.chunks_exact_mut(row).enumerate() {
        let Some(line) = mask
            .pixels
            .get(y * mask.stride..y * mask.stride + mask.width)
        else {
            break;
        };
        let (out, _) = out.as_chunks_mut::<4>();
        for (value, out) in line.iter().zip(out.iter_mut()) {
            if *value != 0 {
                out.copy_from_slice(&pixel);
            }
        }
    }
    let surface =
        cairo::ImageSurface::create_for_data(data, cairo::Format::ARgb32, width, height, stride)?;
    cr.save()?;
    cr.translate(mask.rect.x, mask.rect.y);
    cr.scale(
        mask.rect.w / f64::from(width),
        mask.rect.h / f64::from(height),
    );
    cr.set_source_surface(&surface, 0.0, 0.0)?;
    cr.source().set_filter(cairo::Filter::Nearest);
    cr.paint()?;
    cr.restore()
}

fn draw_polygons(
    cr: &cairo::Context,
    object: &Object,
    colour: u32,
    line_width: f64,
    opacity: f64,
) -> Result<(), cairo::Error> {
    for ring in &object.polygons {
        let mut points = ring.iter();
        let Some(&(x, y)) = points.next() else {
            continue;
        };
        cr.move_to(x, y);
        for &(x, y) in points {
            cr.line_to(x, y);
        }
        cr.close_path();
        source(cr, colour, opacity);
        cr.fill_preserve()?;
        source(cr, colour, POLYGON_STROKE.max(opacity));
        cr.set_line_width(line_width);
        cr.stroke()?;
    }
    Ok(())
}

fn draw_box(
    cr: &cairo::Context,
    rect: Rect,
    colour: u32,
    line_width: f64,
) -> Result<(), cairo::Error> {
    // Half-pixel offsets keep odd-width lines crisp.
    let inset = line_width / 2.0;
    cr.rectangle(
        rect.x + inset,
        rect.y + inset,
        (rect.w - line_width).max(0.0),
        (rect.h - line_width).max(0.0),
    );
    source(cr, colour, BOX_STROKE);
    cr.set_line_width(line_width);
    cr.stroke()
}

fn draw_arrow(
    cr: &cairo::Context,
    from: (f64, f64),
    to: (f64, f64),
    line_width: f64,
) -> Result<(), cairo::Error> {
    source(cr, layout::FOREGROUND, ARROW_STROKE);
    cr.set_line_width(line_width);
    cr.move_to(from.0, from.1);
    cr.line_to(to.0, to.1);
    let angle = (to.1 - from.1).atan2(to.0 - from.0);
    let head = 8.0 * line_width.max(1.0);
    for side in [-0.45_f64, 0.45] {
        cr.move_to(to.0, to.1);
        cr.line_to(
            to.0 - head * (angle + side).cos(),
            to.1 - head * (angle + side).sin(),
        );
    }
    cr.stroke()
}

fn label_parts(object: &Object) -> Vec<String> {
    let mut name = object.label.clone().unwrap_or_else(|| "object".to_owned());
    if let Some(track) = object.track {
        name = format!("{name} #{track}");
    }
    if object.score < 0.0 {
        vec![name]
    } else {
        vec![name, format!("{:.0}%", f64::from(object.score) * 100.0)]
    }
}

fn draw(
    cr: &cairo::Context,
    scene: &Scene,
    frame: (f64, f64),
    style: &Style,
) -> Result<(), cairo::Error> {
    let text_layout = pangocairo::functions::create_layout(cr);
    pangocairo::functions::context_set_resolution(&text_layout.context(), 72.0);
    text_layout.context_changed();
    text_layout.set_font_description(Some(&pango::FontDescription::from_string(&style.font)));
    let lw = style.line_width;
    let colours: Vec<u32> = scene
        .objects
        .iter()
        .map(|object| style.colour(object))
        .collect();
    let colour = |index: usize| colours.get(index).copied().unwrap_or(layout::FOREGROUND);
    let visible = |object: &Object| !object.is_text_line() || style.draw_text;

    if style.draw_masks {
        for mask in &scene.masks {
            draw_mask(
                cr,
                mask,
                mask.object.map_or(layout::FOREGROUND, colour),
                style.mask_opacity,
            )?;
        }
        for (index, object) in scene.objects.iter().enumerate().filter(|(_, o)| visible(o)) {
            draw_polygons(cr, object, colour(index), lw, style.polygon_opacity)?;
        }
    }
    if style.draw_boxes {
        for (index, object) in scene.objects.iter().enumerate().filter(|(_, o)| visible(o)) {
            draw_box(cr, object.rect, colour(index), lw)?;
        }
    }
    let mut predicates = Vec::new();
    if style.draw_relations {
        for arrow in &scene.arrows {
            let (Some(subject), Some(object)) = (
                scene.objects.get(arrow.subject),
                scene.objects.get(arrow.object),
            ) else {
                continue;
            };
            let (from, to) = (subject.rect.centre(), object.rect.centre());
            draw_arrow(cr, from, to, lw)?;
            let chip = Chip::new(&text_layout, std::slice::from_ref(&arrow.predicate));
            let at = Rect {
                x: (from.0 + to.0 - chip.width) / 2.0,
                y: (from.1 + to.1 - chip.height) / 2.0,
                w: chip.width,
                h: chip.height,
            }
            .clamped(frame.0, frame.1);
            predicates.push((chip, at));
        }
    }
    // Label chips (and text chips) are placed together so they avoid each other.
    let mut chips = Vec::new();
    for (index, object) in scene.objects.iter().enumerate() {
        let parts = match &object.text {
            Some(text) if style.draw_text => vec![text.clone()],
            _ if object.is_text_line() || !style.draw_labels => continue,
            _ => label_parts(object),
        };
        chips.push((index, Chip::new(&text_layout, &parts)));
    }
    let anchors: Vec<(Rect, (f64, f64))> = chips
        .iter()
        .filter_map(|(index, chip)| {
            Some((scene.objects.get(*index)?.rect, (chip.width, chip.height)))
        })
        .collect();
    let places = layout::place_chips(frame, &anchors);
    for ((index, chip), at) in chips.iter().zip(places) {
        chip.draw(cr, &text_layout, at, colour(*index), lw)?;
    }
    for (chip, at) in &predicates {
        chip.draw(cr, &text_layout, *at, layout::PREDICATE_BORDER, lw)?;
    }
    if style.draw_caption
        && let Some(caption) = &scene.caption
    {
        let width = (frame.0 - 2.0 * (MARGIN + PAD_X)).max(1.0);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "frame widths are far below i32::MAX pango units"
        )]
        text_layout.set_width((width * f64::from(pango::SCALE)) as i32);
        text_layout.set_wrap(pango::WrapMode::WordChar);
        let chip = Chip::new(&text_layout, std::slice::from_ref(caption));
        let at = Rect {
            x: MARGIN,
            y: frame.1 - MARGIN - chip.height,
            w: chip.width,
            h: chip.height,
        }
        .clamped(frame.0, frame.1);
        chip.draw(cr, &text_layout, at, layout::PREDICATE_BORDER, lw)?;
        text_layout.set_width(-1);
    }
    Ok(())
}

/// Renders `scene` for a `width` × `height` frame, or `None` when there is
/// nothing to draw.
pub fn render(
    scene: &Scene,
    width: u32,
    height: u32,
    style: &Style,
) -> Result<Option<gst_video::VideoOverlayComposition>, glib::BoolError> {
    if scene.is_empty() {
        return Ok(None);
    }
    let (Ok(w), Ok(h)) = (i32::try_from(width), i32::try_from(height)) else {
        return Err(glib::bool_error!("frame too large"));
    };
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h)
        .map_err(|err| glib::bool_error!("creating the overlay surface: {err}"))?;
    {
        let cr = cairo::Context::new(&surface)
            .map_err(|err| glib::bool_error!("creating a cairo context: {err}"))?;
        draw(&cr, scene, (f64::from(width), f64::from(height)), style)
            .map_err(|err| glib::bool_error!("drawing the overlay: {err}"))?;
    }
    surface.flush();
    let stride = surface.stride();
    let data = surface
        .take_data()
        .map_err(|err| glib::bool_error!("reading the overlay surface: {err}"))?;
    let mut buffer = gst::Buffer::from_mut_slice(data);
    // cairo's ARGB32 is a native-endian u32, which is BGRA in memory on
    // little-endian machines.
    let format = if cfg!(target_endian = "little") {
        gst_video::VideoFormat::Bgra
    } else {
        gst_video::VideoFormat::Argb
    };
    gst_video::VideoMeta::add_full(
        buffer
            .get_mut()
            .ok_or_else(|| glib::bool_error!("overlay buffer not writable"))?,
        gst_video::VideoFrameFlags::empty(),
        format,
        width,
        height,
        &[0],
        &[stride],
    )?;
    let rectangle = gst_video::VideoOverlayRectangle::new_raw(
        &buffer,
        0,
        0,
        width,
        height,
        gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
    );
    Ok(Some(gst_video::VideoOverlayComposition::new(Some(
        &rectangle,
    ))?))
}

#[cfg(test)]
mod tests {
    use super::super::scene::Arrow;
    use super::*;

    fn object(label: &str, rect: Rect) -> Object {
        Object {
            id: 0,
            label: Some(label.to_owned()),
            score: 0.69,
            rect,
            track: None,
            polygons: Vec::new(),
            text: None,
        }
    }

    /// Reads the BGRA (little-endian) pixel at `x`, `y`.
    fn pixel(composition: &gst_video::VideoOverlayComposition, x: usize, y: usize) -> [u8; 4] {
        let rectangle = composition.rectangle(0).expect("one rectangle");
        let buffer =
            rectangle.pixels_unscaled_raw(gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA);
        let meta = buffer.meta::<gst_video::VideoMeta>().expect("video meta");
        let stride = usize::try_from(meta.stride()[0]).expect("stride");
        let map = buffer.map_readable().expect("readable");
        let offset = y * stride + x * 4;
        map[offset..offset + 4].try_into().expect("pixel")
    }

    #[test]
    fn empty_scenes_render_nothing() {
        gst::init().expect("GStreamer initializes");
        assert!(
            render(&Scene::default(), 64, 48, &Style::default())
                .expect("render")
                .is_none()
        );
    }

    #[test]
    fn boxes_are_stroked_in_the_class_colour_and_inside_stays_clear() {
        gst::init().expect("GStreamer initializes");
        let rect = Rect {
            x: 10.0,
            y: 30.0,
            w: 40.0,
            h: 20.0,
        };
        let scene = Scene {
            objects: vec![object("man", rect)],
            ..Scene::default()
        };
        let style = Style {
            draw_labels: false,
            ..Style::default()
        };
        let composition = render(&scene, 64, 64, &style)
            .expect("render")
            .expect("composition");
        let edge = pixel(&composition, 20, 30);
        let (r, g, b) = layout::rgb(layout::label_colour("man"));
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "test bytes"
        )]
        let expected = [b, g, r].map(|c| (c * BOX_STROKE * 255.0).round() as u8);
        for (got, want) in edge.iter().zip(expected) {
            assert!(
                got.abs_diff(want) <= 1,
                "edge {edge:?}, expected {expected:?}"
            );
        }
        assert_eq!(edge[3], 191, "75% alpha");
        assert_eq!(pixel(&composition, 30, 40), [0, 0, 0, 0], "inside is clear");
        assert_eq!(pixel(&composition, 5, 5), [0, 0, 0, 0], "outside is clear");
    }

    #[test]
    fn label_chips_draw_above_the_box() {
        gst::init().expect("GStreamer initializes");
        let scene = Scene {
            objects: vec![object(
                "man",
                Rect {
                    x: 10.0,
                    y: 40.0,
                    w: 40.0,
                    h: 20.0,
                },
            )],
            ..Scene::default()
        };
        let style = Style {
            draw_boxes: false,
            ..Style::default()
        };
        let composition = render(&scene, 128, 64, &style)
            .expect("render")
            .expect("composition");
        // The chip background is opaque-ish and sits just above the box.
        assert!(pixel(&composition, 12, 38)[3] >= 200);
        assert_eq!(pixel(&composition, 12, 50), [0, 0, 0, 0]);
    }

    fn premultiplied(colour: u32, alpha: f64) -> [u8; 4] {
        let (r, g, b) = layout::rgb(colour);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "test bytes"
        )]
        [b, g, r, 1.0].map(|c| (c * alpha * 255.0).round() as u8)
    }

    fn close(got: [u8; 4], want: [u8; 4]) -> bool {
        got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 1)
    }

    #[test]
    fn masks_fill_their_set_pixels_scaled_to_the_rect() {
        gst::init().expect("GStreamer initializes");
        let man = object(
            "man",
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            },
        );
        let scene = Scene {
            objects: vec![man],
            masks: vec![Mask {
                rect: Rect {
                    x: 20.0,
                    y: 20.0,
                    w: 20.0,
                    h: 20.0,
                },
                width: 2,
                height: 2,
                stride: 2,
                pixels: vec![1, 0, 0, 1],
                object: Some(0),
            }],
            ..Scene::default()
        };
        let style = Style {
            draw_boxes: false,
            draw_labels: false,
            ..Style::default()
        };
        let composition = render(&scene, 64, 64, &style)
            .expect("render")
            .expect("composition");
        let set = pixel(&composition, 25, 25);
        let want = premultiplied(layout::label_colour("man"), style.mask_opacity);
        assert!(close(set, want), "{set:?} vs {want:?}");
        assert_eq!(pixel(&composition, 35, 25), [0, 0, 0, 0], "unset quadrant");
        assert!(
            close(pixel(&composition, 35, 35), want),
            "second set quadrant"
        );
        assert_eq!(
            pixel(&composition, 45, 45),
            [0, 0, 0, 0],
            "outside the rect"
        );
    }

    #[test]
    fn opacities_set_the_fills() {
        gst::init().expect("GStreamer initializes");
        let rect = Rect {
            x: 10.0,
            y: 10.0,
            w: 20.0,
            h: 20.0,
        };
        let mut man = object("man", rect);
        man.polygons = vec![vec![(10.0, 10.0), (30.0, 10.0), (30.0, 30.0), (10.0, 30.0)]];
        let scene = Scene {
            objects: vec![man],
            masks: vec![Mask {
                rect: Rect {
                    x: 40.0,
                    y: 10.0,
                    w: 20.0,
                    h: 20.0,
                },
                width: 1,
                height: 1,
                stride: 1,
                pixels: vec![1],
                object: Some(0),
            }],
            ..Scene::default()
        };
        let colour = layout::label_colour("man");
        for (polygon, mask) in [(0.10, 0.20), (0.5, 0.8)] {
            let style = Style {
                draw_boxes: false,
                draw_labels: false,
                polygon_opacity: polygon,
                mask_opacity: mask,
                ..Style::default()
            };
            let composition = render(&scene, 64, 40, &style)
                .expect("render")
                .expect("composition");
            let inside = pixel(&composition, 20, 20);
            assert!(close(inside, premultiplied(colour, polygon)), "{inside:?}");
            // Just outside the ring only the outer half of the outline lands.
            let edge = pixel(&composition, 20, 9);
            let stroke = premultiplied(colour, POLYGON_STROKE.max(polygon));
            assert!(
                edge[3].abs_diff(stroke[3] / 2) <= 3,
                "outline {edge:?} vs {stroke:?}"
            );
            let masked = pixel(&composition, 50, 20);
            assert!(close(masked, premultiplied(colour, mask)), "{masked:?}");
        }
    }

    #[test]
    fn relations_draw_a_line_between_box_centres() {
        gst::init().expect("GStreamer initializes");
        let left = object(
            "man",
            Rect {
                x: 0.0,
                y: 20.0,
                w: 20.0,
                h: 20.0,
            },
        );
        let right = object(
            "cup",
            Rect {
                x: 180.0,
                y: 20.0,
                w: 20.0,
                h: 20.0,
            },
        );
        let scene = Scene {
            objects: vec![left, right],
            arrows: vec![Arrow {
                subject: 0,
                object: 1,
                predicate: "on".to_owned(),
            }],
            ..Scene::default()
        };
        let style = Style {
            draw_boxes: false,
            draw_labels: false,
            line_width: 2.0,
            ..Style::default()
        };
        let composition = render(&scene, 200, 60, &style)
            .expect("render")
            .expect("composition");
        // On the line, well away from the predicate chip at the midpoint.
        let on_line = pixel(&composition, 40, 30);
        assert!(
            close(on_line, premultiplied(layout::FOREGROUND, ARROW_STROKE)),
            "{on_line:?}"
        );
        assert_eq!(pixel(&composition, 40, 10), [0, 0, 0, 0]);
        // The predicate chip sits at the midpoint.
        assert!(pixel(&composition, 100, 30)[3] >= 200);
    }

    #[test]
    fn the_tracking_palette_colours_by_track_id() {
        let mut first = object(
            "man",
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            },
        );
        let mut second = first.clone();
        first.track = Some(1);
        second.track = Some(2);
        let tracking = Style {
            palette: Palette::Tracking,
            ..Style::default()
        };
        assert_ne!(tracking.colour(&first), tracking.colour(&second));
        assert_eq!(
            Style::default().colour(&first),
            Style::default().colour(&second)
        );
        let mono = Style {
            palette: Palette::Mono,
            ..Style::default()
        };
        assert_eq!(mono.colour(&first), layout::FOREGROUND);
    }
}
