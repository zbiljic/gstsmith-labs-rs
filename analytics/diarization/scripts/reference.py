"""Print the reference values the tests compare against, as Rust arrays.

The filterbank references come from kaldi-native-fbank and the embedding
reference from ONNX Runtime, both on the same synthetic signal the tests
generate. ONNX Runtime versions can change a model's output, so run this with
the version `ort` bundles (see `mise run //analytics/diarization:references`).

usage: reference.py [WESPEAKER_MODEL.onnx]
"""

import math
import sys

import kaldi_native_fbank as knf
import numpy as np


def signal(n, rate=16000):
    """Two tones and a little noise, from a 32-bit LCG."""
    state = 1
    out = []
    for i in range(n):
        state = (state * 1664525 + 1013904223) & 0xFFFFFFFF
        noise = ((state >> 8) / 16777216.0 - 0.5) * 0.1
        t = i / rate
        out.append(
            0.3 * math.sin(2 * math.pi * 440 * t)
            + 0.2 * math.sin(2 * math.pi * 1250 * t + 0.5)
            + noise
        )
    return np.array(out, dtype=np.float32)


def fbank(x, rate=16000, bins=80):
    opts = knf.FbankOptions()
    opts.frame_opts.samp_freq = rate
    opts.frame_opts.dither = 0.0
    opts.mel_opts.num_bins = bins
    f = knf.OnlineFbank(opts)
    f.accept_waveform(rate, (x * 32768.0).tolist())
    f.input_finished()
    return np.stack([f.get_frame(i) for i in range(f.num_frames_ready)])


def rust_array(name, values, digits):
    print("#[rustfmt::skip]")
    if digits > 5:
        print('#[expect(clippy::unreadable_literal, reason = "printed by scripts/reference.py")]')
    print(f"const {name}: [f32; {len(values)}] = [")
    for k in range(0, len(values), 8):
        print("    " + ", ".join(f"{v:.{digits}}" for v in values[k : k + 8]) + ",")
    print("];")


def main():
    print(f"// kaldi-native-fbank {knf.__version__}")
    m = fbank(signal(8000))
    print(f"// signal(8000) at 16 kHz, 80 bins: {m.shape[0]} frames")
    for frame in (0, 23, 47):
        rust_array(f"FRAME_{frame}", m[frame], 5)
    m = fbank(signal(11025, 22050), 22050, 64)
    print(f"// signal(11025) at 22050 Hz, 64 bins: {m.shape[0]} frames")
    rust_array("FRAME_22050_LAST", m[-1], 5)
    if len(sys.argv) > 1:
        import onnxruntime as ort

        feats = fbank(signal(32000)).astype(np.float32)
        feats -= feats.mean(axis=0, keepdims=True)
        session = ort.InferenceSession(sys.argv[1], providers=["CPUExecutionProvider"])
        e = session.run(None, {"feats": feats[None]})[0][0]
        e /= np.linalg.norm(e)
        print(f"// onnxruntime {ort.__version__}, signal(32000), normalized")
        rust_array("REFERENCE", e, 6)


if __name__ == "__main__":
    main()
