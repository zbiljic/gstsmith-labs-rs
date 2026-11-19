# gst-plugin-diarization

Speaker diarization for transcription cues: which speaker said each cue.

Requires GStreamer 1.28 or newer, for the analytics metadata.

## `diarizer`

`diarizer` takes mono audio and the timed text cues transcribed from it, and
passes the cues on with their speakers.

```text
audio ─ tee ─ queue ─ cactuswhistle ─ diarizer.text_sink
           └─ queue ───────────────── diarizer.audio_sink
                                      diarizer.src ─ …
```

```sh
gst-launch-1.0 \
  filesrc location=talk.wav \
  ! decodebin \
  ! audioconvert \
  ! audioresample \
  ! audio/x-raw,format=F32LE,rate=16000,channels=1 \
  ! tee name=t \
  t. \
  ! queue \
  ! cactuswhistle model-file=whistle.cact \
  ! d.text_sink \
  t. \
  ! queue \
  ! d.audio_sink \
  diarizer name=d model-file=wespeaker-resnet34.onnx \
  ! fakesink dump=true
```

- **`audio_sink`** takes F32LE mono audio at the model's rate (16 kHz).
  Put `audioconvert ! audioresample` in front of it.
- **`text_sink`** takes `text/x-raw, format=utf8` cues, each buffer one cue
  with a timestamp and duration, such as `cactuswhistle` output.
- **`src`** pushes the same cues, in the same order.

Audio is kept by running time for `audio-buffer-duration` (60 s), which must
cover how far the text runs behind the audio. A cue is matched to the audio
at its own running time, so the transcriber must stamp cues with the time
they were spoken. A cue waits up to 5 s for its audio; after a cue waited
in vain, later cues do not wait until audio arrives again. A cue is
embedded with whatever part of its audio is there:
if part of it already left the buffer, or the audio ended early, the rest
is used, and only a cue left with less than the model's minimum, or with
none, gets an unknown speaker. A jump in the audio timestamps of more than
the buffer's length starts the buffer again, and audio marked as a
discontinuity that steps back replaces what was buffered from there on.
Upstream events and queries from `src`, such as seeks, go to the text
branch. The reported latency is the text's plus 1 s; latency added on the
audio branch is not counted, so keep that branch a plain `queue`.

### How a cue gets a speaker

1. The cue's audio is embedded on its own with the speaker model. A cue
   shorter than the model's minimum (half a second), or one that is silent
   (below −70 dBFS), gets an unknown speaker.
2. The embedding is compared with each session speaker's centroid. At
   `match-threshold` or above, the closest speaker is the cue's; a cue at
   least `min-update-duration` long also refines that speaker.
3. A cue that matches nobody starts a new speaker if it is at least
   `min-new-duration` long; a shorter one gets an unknown speaker. Once
   `max-speakers` exist, it goes to the closest speaker instead, without
   refining it.

Speakers are numbered `S1`, `S2`, … in order of appearance and keep their
number for the session. Each also gets a random id.

### Output

- **`x-speaker` document.** Every cue buffer gets a frame-level `x-speaker`
  JSON document in its analytics relation meta (see
  [`analytics-meta`](../../utils/analytics-meta/)):

  ```json
  {"id": "nhhrxb5b", "n": 2, "name": "Haise", "label": "Haise",
   "confidence": 0.71, "new": false, "known": false,
   "model": "wespeaker-resnet34"}
  ```

  `confidence` is the cosine similarity to the speaker, on the model's
  scale; it is unset for the cue that started the speaker. A cue whose
  speaker is unknown still gets a document, with `id`, `n`, `name` and
  `label` set to `null`; its `confidence` is the closest speaker's
  similarity, if there was one to compare with.
- **Speaker-change event.** Before the first cue of a different speaker, a
  serialized custom downstream event `rstranscribe/speaker-change` carries
  `speaker`, the label. gst-plugins-rs' `speechmatics` transcriber sends the
  same event.
  A cue with an unknown speaker does not change the speaker.
- **Bus messages.** An element message `diarizer/speaker` for every cue,
  with the document's fields, the cue's `pts`, `duration` and `text`;
  unset fields are left out. `diarizer/speaker-new` (`n`, `id`, `pts`,
  `text`) when a cue starts a speaker, so an application can ask who it is.
- **Text prefix.** `label-format` is prepended to the text, for consumers
  that only show text, such as `textoverlay`. With `label-mode=on-change`
  only cues whose speaker differs from the previous known speaker get it,
  and the first known cue after an `unknown-label` prefix.
  `label-format=""` leaves the text unchanged.

### Names and merges

`speaker-names` holds names keyed by `S{n}` or speaker id, such as
`names, S1=Lousma, S2=Haise`. It can be set while playing; a name applies to
cues pushed after it is set. Two action signals change speakers while
playing:

- **`set-speaker-name(key, name) -> bool`** sets one name; an empty name
  clears every name of that speaker, under any of its keys. The key is
  `S{n}`, for a speaker that exists or not yet, or a speaker's id.
- **`merge-speakers(keep, drop) -> bool`** folds `drop` into `keep`, for
  when one person was split in two. `keep` keeps its number, id and name (or
  takes `drop`'s if it has none), the two centroids are combined, and later
  cues of either voice go to `keep`. `drop`'s number and id keep naming
  `keep`.

### Properties

| Property | Default | Notes |
| --- | --- | --- |
| `model-file` | none | The speaker model ONNX file. Required |
| `model-profile` | `auto` | `auto` recognizes a qualified model file by its SHA-256 and refuses any other; `custom` reads `model-config` |
| `model-config` | none | A TOML file with the profile for `model-profile=custom`; see [Other models](#other-models). Empty is unset |
| `threads` | 0 | ONNX Runtime threads; 0 lets ONNX Runtime choose, which takes every core it finds |
| `match-threshold` | 0 | 0 uses the profile's (0.4 for `wespeaker-resnet34`) |
| `min-new-duration` | 1500 | Milliseconds |
| `min-update-duration` | 1000 | Milliseconds |
| `max-speakers` | 0 | 0 is unlimited |
| `speaker-names` | empty | Can be set while playing |
| `label-format` | `"{label}: "` | Placeholders `{label}` (the name, or `S{n}`), `{name}`, `{n}`, `{id}`, `{confidence}`. Can be set while playing |
| `label-mode` | `on-change` | `always` or `on-change`. Can be set while playing |
| `unknown-label` | `""` | The prefix of a cue with an unknown speaker. Can be set while playing |
| `audio-buffer-duration` | 60000 | Milliseconds; up to an hour, for a transcriber that only produces cues at the end. Audio takes about 230 MB per hour at 16 kHz |

### Measured

On short labelled conversations, cut into cues of one speaker each
(overlapping speech is left out, and an unknown cue counts as wrong):

| Conversation | Speakers | Cue time correct | Speakers found |
| --- | --- | --- | --- |
| Two-person recording (30 s) | 2 | 91% | 2 |
| AMI meeting excerpts (2 × 30 s) | 2 each | 100% and 97% | 2 each |
| Turns of both, interleaved | 4 | 96% | 4 |

Results on transcribed broadcast audio are [below](#on-broadcast-audio).

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

### Other models

A model that takes Kaldi filterbank features and returns one embedding runs
with `model-profile=custom` and a `model-config` file. The filterbank is
the one described below, with a Povey window and no dither; the config sets
its sizes, frequencies and scaling, not its window. This one runs the model
above the way the built-in profile does:

```toml
name = "wespeaker-copy"
input = "feats"
output = "embs"
min-duration = 500
max-duration = 10000
match-threshold = 0.4
sha256 = ["7bb2f06e9df17cdf1ef14ee8a15ab08ed28e8d0ef5054ee135741560df2ec068"]

[front-end]
kind = "kaldi-fbank"
sample-rate = 16000
frame-length = 25
frame-shift = 10
mel-bins = 80
low-freq = 20
high-freq = 0
preemphasis = 0.97
input-scale = 32768
mean-normalize = true
```

- `name` goes into the `x-speaker` document's `model`. It is up to 64
  lowercase letters, digits, `.`, `_` and `-`, starting with a letter or
  digit, and not `auto`, `custom` or a built-in profile's name.
- `input` and `output` name the model's tensors. The input is
  `(1, frames, mel-bins)`; the output is one vector, `(1, D)` or `(D)`.
- `min-duration` and `max-duration` are in milliseconds. Shorter cues get an
  unknown speaker, and longer ones are embedded in parts. `max-duration`
  must be at least four times `min-duration`.
- `match-threshold` is the cosine at which a cue belongs to a speaker.
  Measure it on the model's own scores: each model spreads them
  differently.
- `sha256` is optional. When given, a model file that is not listed is
  used with a warning.
- In `[front-end]`, `kind` is `kaldi-fbank`, the only one so far, and the
  other keys may be left out. Without them the values are those shown,
  except `mean-normalize`, which subtracts each bin's mean over the segment
  and is off by default. `frame-length` and `frame-shift` are milliseconds,
  `low-freq` and `high-freq` Hz, and `input-scale` multiplies the samples
  first. A `high-freq` of zero or less counts down from the Nyquist
  frequency. The audio pad takes `sample-rate`, up to 384 kHz.

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
0.70 and different-speaker pairs 0.08, but turns of different speakers in
the two-person recording reach 0.49, which is why the profile matches no
lower than 0.4.

### On broadcast audio

`mise run //analytics/diarization:evaluate` runs 44 recordings of the
VoxConverse dev set through `cactuswhistle language=en ! diarizer` with the
default settings, and scores each cue against the human-labelled turns; a
cue belongs to whoever talks most during it. VoxConverse, by the Visual
Geometry Group at the University of Oxford, is news, debates and panel
shows from online video, offered for research under CC-BY-4.0; the audio's
copyright stays with the videos' owners.

| Recordings | Cues correct | Unknown | Speakers found |
| --- | --- | --- | --- |
| 44, which chose the defaults | 94.6% | 2.9% | 221 for 209 |

The totals hide errors both ways: on these recordings, 19 found
speakers were a second speaker for a voice already found (`merge-speakers`
folds one back), and 7 people never got one of their own. About 4% of cue
speech belongs to someone other than the cue's speaker, where a turn
changes within a cue or two people talk at once; labelling whole cues
cannot get that right.

Raising `min-new-duration` trades more unknown cues for fewer extra
speakers, but in conversations of short turns a second voice can be
absorbed by the first. When the number of speakers is known,
`max-speakers` caps it.

## Tests

`cargo test -p gst-plugin-diarization` runs the unit tests: the filterbank
against reference values from `kaldi-native-fbank`, clustering, labels, and
the element with a stand-in model. `mise run //analytics/diarization:smoke`
downloads the model and the labelled conversations (pyannote.audio's
tutorial sample and AMI test excerpts, pinned by commit and SHA-256), checks
the embeddings against the reference pipeline, and runs the conversations
through `diarizer`. `mise run //analytics/diarization:references` prints the
reference values again, from pinned versions of `kaldi-native-fbank` and
ONNX Runtime; it needs `uv`.

`mise run //analytics/diarization:evaluate` scores real broadcast audio, as
[above](#on-broadcast-audio). It downloads the speaker model, the Cactus
weights and a 485 MB VoxConverse shard from a pinned Hugging Face mirror,
builds both plugins, and takes about 40 minutes on a laptop CPU; it needs
`uv`. Arguments after `--` choose recordings (`--only ID,ID`), parallel
runs (`--jobs N`, default 3), and `diarizer` properties
(`min-new-duration=2000`).
