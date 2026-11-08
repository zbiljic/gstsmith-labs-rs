# cactus

Safe access to the process-wide [Cactus](https://cactuscompute.com) 4.1.0
engine through [`cactus-sys`](../cactus-sys/).

- **One engine, one lock.** The C ABI has no handle: one engine per process
  holds every loaded model. `Engine::lock()` is the only way in, so vision
  and speech calls are serialized; a 300 ms `describe` delays a concurrent
  transcription by 300 ms.
- **Weights.** `load_vision` classifies the `.cact` archive first (the
  vision manifest after `final_norm`, as `cactus-go` does) and fails clearly
  on text or speech weights and on weights from another engine generation.
  Weights are identified by their SHA-256. Loading the same bytes again does
  nothing; different vision weights are refused for the life of the
  process, because the engine has one vision slot and no unload.
- **`describe`.** Takes a tightly packed RGB24 image and a request (options
  as a JSON object, empty meaning `{}`; language; newline-separated
  keywords) and returns the engine's JSON unchanged with its token count.
  The output buffer starts at 1 MiB and grows when the engine reports, for
  example, `output buffer is too small: 3275 bytes needed`; that error comes
  only after inference ran, so each growth runs inference again, which
  `Description::grew_to` reports. The grown buffer is kept.
- **Threads.** `set_threads(0)` keeps the engine default and never calls the
  engine. The first explicit value wins for the process and applies only to
  models loaded afterwards: set threads before loading weights. A later
  different value returns `Threads::Ignored`, a first value after a model
  is loaded `Threads::TooLate`.

## Tests

The default tests need no weights. Real-engine tests are opt-in:

```sh
mise run cactus:smoke
```

That fetches the pinned Mirage weights to `target/models/mirage.cact` and
runs `tests/engine.rs`.
