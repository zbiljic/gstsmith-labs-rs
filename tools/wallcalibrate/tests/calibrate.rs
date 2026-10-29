#![expect(
    clippy::expect_used,
    reason = "test setup and assertions require successful commands and files"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use image::DynamicImage;
use wall_geometry::{Layout, Point, Quad, Screen, Size};
use wallcalibrate::calibration::{self, Calibration, SolveOptions, Status};
use wallcalibrate::canvas::{self, CanvasSize, LayoutOptions, Mode};
use wallcalibrate::detect::{self, DETECTIONS_VERSION, Detections, Photo};
use wallcalibrate::manifest::{Manifest, Placement};
use wallcalibrate::synth::{self, SynthOptions};
use wallcalibrate::tag::FAMILY;

const THREE_SCREENS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../video/wallwarp/examples/three-screens.json"
);

fn wallcalibrate(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wallcalibrate"))
        .args(args)
        .output()
        .expect("running wallcalibrate")
}

fn succeeds(args: &[&str]) -> String {
    let output = wallcalibrate(args);
    assert!(
        output.status.success(),
        "wallcalibrate {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn path(directory: &Path, name: &str) -> String {
    directory.join(name).display().to_string()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).expect("reading output")
}

fn distance(a: Point, b: Point) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

/// Largest corner distance between screens of two calibrations, by ID.
fn worst_corner_error(actual: &Calibration, truth: &Calibration) -> f64 {
    truth
        .screens
        .iter()
        .map(|expected| {
            let screen = actual
                .screens
                .iter()
                .find(|screen| screen.id == expected.id)
                .expect("screen");
            let (Some(actual), Some(expected)) = (screen.corners, expected.corners) else {
                return f64::INFINITY;
            };
            actual
                .corners()
                .iter()
                .zip(expected.corners())
                .map(|(a, e)| distance(*a, e))
                .fold(0.0, f64::max)
        })
        .fold(0.0, f64::max)
}

/// Both layout modes from a complete three-screen calibration.
fn check_layouts(dir: &Path, calibration: &str) {
    let layout_path = path(dir, "layout.json");
    let summary = succeeds(&[
        "layout",
        calibration,
        "-o",
        &layout_path,
        "--preview",
        &path(dir, "layout.svg"),
    ]);
    assert!(summary.contains("camera's viewpoint"), "{summary}");
    let layout = Layout::from_json(&read(&layout_path)).expect("viewpoint layout");
    assert_eq!(layout.canvas.width, 1920);
    let ids: Vec<_> = layout
        .screens
        .iter()
        .map(|screen| screen.id.as_str())
        .collect();
    assert_eq!(ids, ["display-c", "display-a", "display-b"]);
    assert!(read(&path(dir, "layout.svg")).contains(">display-b<"));

    let summary = succeeds(&[
        "layout",
        calibration,
        "-o",
        &layout_path,
        "--mode",
        "planar",
        "--reference",
        "display-a",
        "--reference-size",
        "1105x622",
        "--canvas",
        "1920x1080",
    ]);
    assert!(
        summary.contains("plane of display-a") && summary.contains("reference-size units"),
        "{summary}"
    );
    let layout = Layout::from_json(&read(&layout_path)).expect("planar layout");
    assert_eq!(
        layout.canvas,
        Size {
            width: 1920,
            height: 1080
        }
    );
}

#[test]
fn cli_calibrates_a_synthetic_wall_end_to_end() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let dir = directory.path();
    let (manifest, photo, truth) = (
        path(dir, "manifest.json"),
        path(dir, "photo.png"),
        path(dir, "truth.json"),
    );
    let (detections, calibration) = (path(dir, "detections.json"), path(dir, "calibration.json"));

    let listed = succeeds(&["manifest", "--layout", THREE_SCREENS, "-o", &manifest]);
    assert!(
        listed.contains("display-b: 1920x1080, tags 8, 9, 10, 11"),
        "{listed}"
    );
    succeeds(&["pattern", &manifest, "-o", &path(dir, "patterns"), "--png"]);
    for name in [
        "index.html",
        "display-a.html",
        "display-a.svg",
        "display-c.png",
    ] {
        assert!(dir.join("patterns").join(name).is_file(), "{name}");
    }
    succeeds(&[
        "synth",
        &manifest,
        THREE_SCREENS,
        "-o",
        &photo,
        "--truth",
        &truth,
        "--size",
        "1800x1200",
        "--black",
        "40",
        "--noise",
        "4",
    ]);
    let found = succeeds(&["detect", &photo, "-o", &detections]);
    assert!(found.contains("found 12 markers"), "{found}");
    let report = succeeds(&[
        "solve",
        &manifest,
        &detections,
        "-o",
        &calibration,
        "--preview",
        &path(dir, "preview.html"),
    ]);
    assert_eq!(
        report.matches("complete, 4/4 markers").count(),
        3,
        "{report}"
    );

    let solved = Calibration::from_json(&read(&calibration)).expect("calibration");
    let truth = Calibration::from_json(&read(&truth)).expect("truth");
    let error = worst_corner_error(&solved, &truth);
    assert!(error < 0.5, "display corners off by {error} px");
    let preview = read(&path(dir, "preview.html"));
    assert!(preview.contains("data:image/jpeg;base64,") && preview.contains("display-c"));

    check_layouts(dir, &calibration);
}

#[test]
fn incomplete_calibrations_are_reported_and_completed_by_hand() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let dir = directory.path();
    let (manifest, photo, truth) = (
        path(dir, "manifest.json"),
        path(dir, "photo.png"),
        path(dir, "truth.json"),
    );
    let (detections, calibration, layout) = (
        path(dir, "detections.json"),
        path(dir, "calibration.json"),
        path(dir, "layout.json"),
    );
    succeeds(&["manifest", "--layout", THREE_SCREENS, "-o", &manifest]);
    // Cover every tag on display-c (tags 0 to 3) and two on display-a.
    succeeds(&[
        "synth",
        &manifest,
        THREE_SCREENS,
        "-o",
        &photo,
        "--truth",
        &truth,
        "--size",
        "1800x1200",
        "--hide-tag",
        "0",
        "--hide-tag",
        "1",
        "--hide-tag",
        "2",
        "--hide-tag",
        "3",
        "--hide-tag",
        "5",
        "--hide-tag",
        "6",
    ]);
    succeeds(&["detect", &photo, "-o", &detections]);

    let output = wallcalibrate(&["solve", &manifest, &detections, "-o", &calibration]);
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("display-c: MISSING, 0/4 markers"),
        "{stdout}"
    );
    assert!(
        stdout.contains("display-a: partial, 2/4 markers"),
        "{stdout}"
    );
    assert!(stderr.contains("no corners for display-c"), "{stderr}");

    let mut solved = Calibration::from_json(&read(&calibration)).expect("calibration");
    assert_eq!(solved.missing(), ["display-c"]);
    let missing = solved
        .screens
        .iter()
        .find(|screen| screen.id == "display-c")
        .expect("display-c");
    assert_eq!((missing.status, missing.corners), (Status::Missing, None));

    let output = wallcalibrate(&["layout", &calibration, "-o", &layout]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("display-c"));
    let summary = succeeds(&["layout", &calibration, "-o", &layout, "--allow-incomplete"]);
    assert!(
        summary.contains("omitted displays without corners: display-c"),
        "{summary}"
    );
    assert_eq!(
        Layout::from_json(&read(&layout))
            .expect("layout")
            .screens
            .len(),
        2
    );

    // Corners added by hand complete the layout.
    let truth = Calibration::from_json(&read(&truth)).expect("truth");
    let placed = truth
        .screens
        .iter()
        .find(|screen| screen.id == "display-c")
        .expect("truth");
    let screen = solved
        .screens
        .iter_mut()
        .find(|screen| screen.id == "display-c")
        .expect("display-c");
    screen.corners = placed.corners;
    screen.status = Status::Manual;
    std::fs::write(&calibration, solved.to_json().expect("serialize"))
        .expect("writing calibration");
    succeeds(&["layout", &calibration, "-o", &layout]);
    assert_eq!(
        Layout::from_json(&read(&layout))
            .expect("layout")
            .screens
            .len(),
        3
    );
}

#[test]
fn invalid_arguments_are_rejected() {
    for (args, message) in [
        (&["frobnicate"][..], "unknown command"),
        (&["manifest", "-o", "unused.json"][..], "--screen"),
        (
            &["manifest", "--screen", "a:12", "-o", "unused.json"][..],
            "WIDTHxHEIGHT",
        ),
        (&["pattern", "--colour", "x"][..], "unknown option --colour"),
        (
            &[
                "layout",
                "missing.json",
                "-o",
                "unused.json",
                "--mode",
                "planar",
            ][..],
            "reading",
        ),
    ] {
        let output = wallcalibrate(args);
        assert!(!output.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }
    assert!(succeeds(&["help"]).contains("Usage: wallcalibrate"));
}

/// A coplanar wall: two upright 16:9 displays, one 16:9 display mounted
/// rotated 90° clockwise, and gaps between them, in centimetres.
fn coplanar_wall() -> Layout {
    let quad = |values: [[f64; 2]; 4]| Quad::new(values.map(Point::from));
    let screen = |id: &str, width, height, corners| Screen {
        id: id.to_owned(),
        output: Size { width, height },
        corners: quad(corners),
    };
    Layout {
        version: 1,
        canvas: Size {
            width: 300,
            height: 140,
        },
        screens: vec![
            screen(
                "left",
                1920,
                1080,
                [[10.0, 10.0], [106.0, 10.0], [106.0, 64.0], [10.0, 64.0]],
            ),
            screen(
                "lower",
                1280,
                720,
                [[30.0, 80.0], [110.0, 80.0], [110.0, 125.0], [30.0, 125.0]],
            ),
            screen(
                "tall",
                1920,
                1080,
                [[200.0, 15.0], [200.0, 111.0], [146.0, 111.0], [146.0, 15.0]],
            ),
        ],
    }
}

fn similarity_error(actual: &Layout, truth: &Layout, reference: &str) -> f64 {
    // Align by the reference's top-left corner and top edge length.
    let pick = |layout: &Layout| layout.screen(reference).expect("reference").corners;
    let (a, t) = (pick(actual), pick(truth));
    let scale = distance(a.top_left, a.top_right) / distance(t.top_left, t.top_right);
    let width = f64::from(actual.canvas.width);
    truth
        .screens
        .iter()
        .flat_map(|expected| {
            let screen = actual.screen(&expected.id).expect("screen");
            screen
                .corners
                .corners()
                .into_iter()
                .zip(expected.corners.corners())
                .map(|(actual, expected)| {
                    let mapped = Point::new(
                        a.top_left.x + (expected.x - t.top_left.x) * scale,
                        a.top_left.y + (expected.y - t.top_left.y) * scale,
                    );
                    distance(actual, mapped) / width
                })
                .collect::<Vec<_>>()
        })
        .fold(0.0, f64::max)
}

#[test]
fn planar_mode_rectifies_a_photographed_wall() {
    let wall = coplanar_wall();
    let manifest = Manifest::from_layout(&wall, 100, Placement::default()).expect("manifest");
    // The wall seen from the left and below, in strong perspective.
    let view = Quad::new([
        Point::new(260.0, 300.0),
        Point::new(1660.0, 120.0),
        Point::new(1700.0, 1050.0),
        Point::new(200.0, 900.0),
    ]);
    let placements = synth::view_of_layout(&wall, &view).expect("view");
    let options = SynthOptions {
        width: 1800,
        height: 1200,
        ..SynthOptions::default()
    };
    let photo = synth::render(&manifest, &placements, &options).expect("render");
    let detections = Detections {
        version: DETECTIONS_VERSION,
        family: FAMILY.to_owned(),
        detector: detect::BUILTIN_DETECTOR.to_owned(),
        photo: Photo {
            path: None,
            width: 1800,
            height: 1200,
        },
        markers: detect::detect(&DynamicImage::ImageLuma8(photo)).expect("detect"),
    };
    assert_eq!(detections.markers.len(), 12);
    let solved = calibration::solve(&manifest, &detections, SolveOptions::default());
    assert_eq!(solved.missing(), Vec::<&str>::new());

    let planar = canvas::layout(
        &solved,
        &LayoutOptions {
            mode: Mode::Planar {
                reference: "left".to_owned(),
                size: Some((96.0, 54.0)),
            },
            canvas: CanvasSize::Width(1500),
            allow_incomplete: false,
            level: None,
            straighten: false,
        },
    )
    .expect("planar layout");
    // The wall spans 190 cm (10 to 200) across 1500 px.
    let units = planar.units_per_pixel.expect("units");
    assert!(
        (units - 190.0 / 1500.0).abs() < 2e-4,
        "{units} cm per pixel"
    );
    let error = similarity_error(&planar.layout, &wall, "left");
    assert!(
        error < 0.001,
        "planar layout off by {error} of the canvas width"
    );

    let viewpoint = canvas::layout(
        &solved,
        &LayoutOptions {
            mode: Mode::Viewpoint,
            canvas: CanvasSize::Width(1500),
            allow_incomplete: false,
            level: None,
            straighten: false,
        },
    )
    .expect("viewpoint layout");
    let error = similarity_error(&viewpoint.layout, &wall, "left");
    assert!(
        error > 0.03,
        "the viewpoint layout keeps the camera's perspective ({error})"
    );
}

#[test]
#[ignore = "needs uv and network access to install opencv-python-headless"]
fn opencv_detector_matches_the_truth() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let dir = directory.path();
    let manifest = path(dir, "manifest.json");
    succeeds(&["manifest", "--layout", THREE_SCREENS, "-o", &manifest]);
    let script: PathBuf = [env!("CARGO_MANIFEST_DIR"), "scripts", "detect_opencv.py"]
        .iter()
        .collect();
    for (name, degradation) in [
        ("clean", &["--black", "20"][..]),
        (
            "harsh",
            &["--black", "100", "--blur", "3", "--noise", "20"][..],
        ),
    ] {
        let (photo, truth) = (
            path(dir, &format!("{name}.png")),
            path(dir, &format!("{name}.json")),
        );
        let mut args = vec![
            "synth",
            &manifest,
            THREE_SCREENS,
            "-o",
            &photo,
            "--truth",
            &truth,
        ];
        args.extend(degradation);
        succeeds(&args);
        let detections = path(dir, &format!("{name}-detections.json"));
        let status = Command::new("uv")
            .args([
                "run",
                "--quiet",
                "--no-project",
                "--with",
                "opencv-python-headless==4.12.0.88",
            ])
            .args(["python", "-I"])
            .arg(&script)
            .args([&photo, "-o", &detections])
            .status()
            .expect("running uv");
        assert!(status.success(), "OpenCV detector failed on {name}");
        let calibration = path(dir, &format!("{name}-calibration.json"));
        succeeds(&["solve", &manifest, &detections, "-o", &calibration]);
        let solved = Calibration::from_json(&read(&calibration)).expect("calibration");
        let truth = Calibration::from_json(&read(&truth)).expect("truth");
        let error = worst_corner_error(&solved, &truth);
        assert!(error < 1.5, "{name}: display corners off by {error} px");
    }
}
