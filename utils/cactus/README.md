# cactus

Safe access to named [Cactus](https://cactuscompute.com) 4.1.0 engines
through [`cactus-sys`](../cactus-sys/).

- **Named engines, taking turns.** The C ABI has no handle: an engine keeps
  every loaded model in globals, and its calls are not thread-safe. Each
  name passed to `Engine::lock(name)` gets its own engine, a separate copy
  of the engine library loaded on first use; the empty name is the default
  engine. The guard it returns is the only way in. Callers of one engine
  take turns in the order they asked, so a 300 ms `describe` delays a
  transcription on that engine by up to 300 ms, while engines with
  different names run at the same time. A panic during a turn ends it.
  Engines are never unloaded.
- **Weights.** `load_vision` and `load_speech` classify the `.cact`
  archive first (the vision or audio manifest after `final_norm`, as
  `cactus-go` does) and fail clearly on weights of another kind and on
  weights from another engine generation. Weights are identified by their
  SHA-256. Loading the same bytes again does nothing; different weights of
  a kind are refused by that engine for the life of the process, because
  an engine has one slot per kind and no unload.
- **`describe`.** Takes a tightly packed RGB24 image and a request (options
  as a JSON object, empty meaning `{}`; language; newline-separated
  keywords) and returns the engine's JSON unchanged with its token count.
  The output buffer starts at 1 MiB and grows when the engine reports, for
  example, `output buffer is too small: 3275 bytes needed`; that error comes
  only after inference ran, so each growth runs inference again, which
  `Description::grew_to` reports. The grown buffer is kept.
- **`transcribe`.** Takes 16 kHz mono PCM and a request (language, empty
  for detection; newline-separated keywords; word timestamps) and returns
  the engine's JSON unchanged. Up to 30 seconds run as one pass. The call
  is stateless and shares the output buffer and its growth with
  `describe`.
- **Streaming.** Each engine holds one speech stream. `open_stream`
  reserves it (a second caller gets `Error::StreamInUse`),
  `stream_process` feeds a step and returns the words two consecutive
  passes agreed on, `stream_stop` returns the tail and lets the next step
  start again at time zero, and `close_stream` discards the tail and
  releases the stream. Other calls, including `describe` and `transcribe`,
  can run between steps without disturbing the stream. Stream calls are
  never retried on an output overflow, because a retry would feed the
  audio again; the buffer is sized for the step first.
- **Threads.** `set_threads(0)` keeps the engine default and never calls the
  engine. The first explicit value wins for the engine and applies only to
  models loaded afterwards: set threads before loading weights. A later
  different value returns `Threads::Ignored`, a first value after a model
  is loaded `Threads::TooLate`.

## Tests

The default tests need no weights. Real-engine tests are opt-in:

```sh
mise run cactus:smoke
```

That fetches the pinned Mirage and Whistle weights and a speech sample to
`target/models/` and runs `tests/engine.rs` and `tests/speech.rs`.
