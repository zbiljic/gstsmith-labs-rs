"""Download verified smoke-test assets and specialize their image inputs."""

import argparse
import hashlib
from pathlib import Path
import subprocess
import tempfile

import onnx

RELEASES = "https://github.com/jamjamjon/assets/releases/download"
# release path, local name, source SHA256, image size, specialized SHA256
MODELS = {
    "fastsam": [
        ("fastsam/FastSAM-s.onnx", "FastSAM-s.onnx",
         "fca938545ec084df1f65009cf8af670970b0b264b744725bc87ff128fd933c2a", 640,
         "5ab0ac16b6e3c5f235517da6a207de8431a5f023200f883d3179cf98acaafbab"),
    ],
    "mobile-sam": [
        ("sam/mobile-sam-vit-t-encoder.onnx", "mobile-encoder.onnx",
         "8b8168033ea6687bb55ba242222b67a301ac9da30fd5cbfd04dcebbb180ec2a8", 1024,
         "d075f9b8ce20807dbebe0e8d55543d1c7dc33605480992cac500a0a2410fa34b"),
        ("sam/mobile-sam-vit-t-decoder.onnx", "mobile-decoder.onnx",
         "1b216fb3b8ceeee00a65f89670c01e4c0d823fcacec39dd9accc233f85341dc4", None, None),
    ],
}


def verify(path, expected):
    with path.open("rb") as stream:
        actual = hashlib.file_digest(stream, "sha256").hexdigest()
    if actual != expected:
        raise ValueError(f"SHA256 mismatch for {path}: expected {expected}, got {actual}")


def specialize(source, destination, size):
    model = onnx.load(source)
    inputs = model.graph.input
    if len(inputs) != 1 or inputs[0].type.tensor_type.elem_type != onnx.TensorProto.FLOAT:
        raise ValueError("expected one FP32 image input")
    dims = inputs[0].type.tensor_type.shape.dim
    shape = [1, 3, size, size]
    if len(dims) != 4 or any(d.HasField("dim_value") and d.dim_value != n for d, n in zip(dims, shape)):
        raise ValueError(f"image input is incompatible with {shape}")
    for dim, value in zip(dims, shape):
        dim.ClearField("dim_param")
        dim.dim_value = value
    onnx.checker.check_model(model)
    onnx.save(model, destination)


def prepare(profile, directory):
    directory.mkdir(parents=True, exist_ok=True)
    # Publish only verified files; a failed download or conversion leaves existing files intact.
    with tempfile.TemporaryDirectory(prefix=".prepare-segment-", dir=directory) as staging:
        for release, name, digest, size, fixed_digest in MODELS[profile]:
            source = directory / name
            if not source.exists():
                downloaded = Path(staging) / name
                subprocess.run([
                    "curl", "--fail", "--location", "--retry", "3", "--retry-delay", "1",
                    f"{RELEASES}/{release}", "--output", str(downloaded),
                ], check=True)
                verify(downloaded, digest)
                downloaded.replace(source)
            verify(source, digest)
            if size is None:
                print(source)
                continue
            destination = source.with_stem(f"{source.stem}-batch1")
            if not destination.exists():
                generated = Path(staging) / destination.name
                specialize(source, generated, size)
                verify(generated, fixed_digest)
                generated.replace(destination)
            verify(destination, fixed_digest)
            print(destination)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("model", choices=MODELS)
    parser.add_argument("--output-dir", type=Path, default=Path("target/models"))
    args = parser.parse_args()
    prepare(args.model, args.output_dir)
