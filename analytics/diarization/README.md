# gst-plugin-diarization

Speaker diarization for transcription cues: which speaker said each cue.

This is the first part of the plugin. It contains the speaker models and
registers no element yet. The `diarizer` element, which takes audio and timed
text cues and adds speakers to the cues, comes next.

## Speaker models

A speaker model turns a segment of 16 kHz mono audio into an embedding, a
vector that is close for the same voice and far for different voices,
whatever is being said. Each model is used through a profile. A profile says
how to compute the model's input, how long a segment may be, and which
similarity thresholds suit its embeddings. A model file is recognized by its
SHA-256, or a profile can be named.

| Profile | Model | License | Embedding |
| --- | --- | --- | --- |
| `wespeaker-resnet34` | [WeSpeaker ResNet34-LM](https://huggingface.co/Wespeaker/wespeaker-voxceleb-resnet34-LM), trained on VoxCeleb | CC-BY-4.0 | 256 values |

No model is stored in this repository. `mise run diarization:models` downloads
the pinned file to `target/models/` and checks its SHA-256.

Models run on ONNX Runtime 1.28, whose prebuilt library the build downloads.
The version matters: one candidate model, 3D-Speaker CAM++, waits for a newer
one, because 1.28 computes one of its pooling layers differently from the
framework it was trained in.

Input features are Kaldi-compatible log mel filterbanks: 80 bins, 25 ms
frames every 10 ms, with each bin's mean subtracted. They
match `kaldi-native-fbank`, and the embeddings match the reference Python
pipeline (`kaldi-native-fbank` with ONNX Runtime 1.28) at cosine 1.0000 on
every clip tested.

Segments shorter than half a second are refused, because the model returns NaN
rather than an error for very short input. So are segments with a NaN or
infinite sample. Segments longer than 10 s are embedded in parts of equal
length, each normalized on its own, and the parts' embeddings averaged.

On a laptop CPU with 4 threads, embedding takes about 16 ms per second of
audio. On clips of read speech, same-speaker pairs reach a median cosine of
0.70 and different-speaker pairs 0.08.

## Tests

`cargo test -p gst-plugin-diarization` runs the unit tests, including the
filterbank against reference values from `kaldi-native-fbank`.
`mise run //analytics/diarization:smoke` downloads the model and checks its
embeddings against the reference pipeline.
`mise run //analytics/diarization:references` prints those reference values
again, from pinned versions of `kaldi-native-fbank` and ONNX Runtime; it needs
`uv`.
