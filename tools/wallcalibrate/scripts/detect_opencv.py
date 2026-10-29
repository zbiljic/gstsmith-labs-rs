"""Detect tag36h11 markers with OpenCV and write wallcalibrate detections JSON.

OpenCV copes better than the built-in detector with blurred, noisy, or
low-contrast photos. The output can be passed to `wallcalibrate solve`.
"""

import argparse
import json
import sys
from pathlib import Path

import cv2


def detect(photo):
    # Like the built-in detector, imread applies the photo's EXIF orientation.
    image = cv2.imread(str(photo), cv2.IMREAD_GRAYSCALE)
    if image is None:
        hint = ""
        if photo.suffix.lower() in {".heic", ".heif"}:
            hint = (
                "; convert it first, for example with "
                f"libheif's `heif-convert {photo} photo.jpg`"
            )
        raise ValueError(f"cannot read {photo}{hint}")
    parameters = cv2.aruco.DetectorParameters()
    # AprilTag refinement reports corners with pixel (i, j) covering
    # [i, i + 1) x [j, j + 1), as wallcalibrate does; sub-pixel refinement is
    # half a pixel off.
    parameters.cornerRefinementMethod = cv2.aruco.CORNER_REFINE_APRILTAG
    dictionary = cv2.aruco.getPredefinedDictionary(cv2.aruco.DICT_APRILTAG_36h11)
    corners, ids, _rejected = cv2.aruco.ArucoDetector(dictionary, parameters).detectMarkers(image)
    markers = []
    for quad, tag in zip(corners, [] if ids is None else ids.flatten(), strict=True):
        points = [[float(x), float(y)] for x, y in quad.reshape(4, 2)]
        # OpenCV's tag36h11 bits are rotated 180 degrees from the AprilTag
        # reference images, so its first corner is the tag's bottom-right.
        markers.append({"tag": int(tag), "corners": points[2:] + points[:2]})
    markers.sort(key=lambda marker: marker["tag"])
    height, width = image.shape
    return {
        "version": 1,
        "family": "tag36h11",
        "detector": f"opencv {cv2.__version__}",
        "photo": {"path": str(photo), "width": width, "height": height},
        "markers": markers,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("photo", type=Path)
    parser.add_argument("-o", "--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        detections = detect(args.photo)
    except ValueError as error:
        sys.exit(f"detect_opencv: {error}")
    args.output.write_text(json.dumps(detections, indent=2) + "\n")
    tags = ", ".join(str(marker["tag"]) for marker in detections["markers"]) or "none"
    photo = detections["photo"]
    print(
        f"found {len(detections['markers'])} markers in {photo['width']}x{photo['height']} "
        f"photo: tags {tags}"
    )


if __name__ == "__main__":
    main()
