//! The engine behind `cactuswhistle`, replaceable by a fixture in tests.

use serde::Deserialize;

use super::stitch::Word;

/// What one transcription request asks for besides the audio.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub language: Option<String>,
    /// Newline-separated.
    pub keywords: Option<String>,
}

/// Loads weights and transcribes 16 kHz mono PCM with word times.
pub trait Transcriber: Send {
    /// Sets threads on the named engine, then loads the weights there.
    /// Returns their SHA-256 and what happened to the thread setting.
    fn start(
        &mut self,
        engine: &str,
        weights: &[u8],
        threads: u32,
    ) -> Result<(String, cactus::Threads), String>;
    /// Returns the engine JSON for `pcm`, with word timestamps.
    fn transcribe(&mut self, pcm: &[f32], request: &Request) -> Result<String, String>;
    /// Reserves the engine's one speech stream.
    fn open_stream(&mut self) -> Result<(), String>;
    /// Feeds one step to the stream; with `last`, also stops it and adds its
    /// tail. Returns the JSON of each engine call.
    fn stream(&mut self, pcm: &[f32], request: &Request, last: bool)
    -> Result<Vec<String>, String>;
    /// Discards the stream's audio; the next step starts at time zero.
    fn reset_stream(&mut self);
}

/// A named Cactus engine, chosen at start.
#[derive(Default)]
pub struct Engine {
    name: String,
    stream: Option<cactus::StreamId>,
}

impl Engine {
    fn lock(&self) -> Result<cactus::Engine, String> {
        cactus::Engine::lock(&self.name).map_err(|err| err.to_string())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(id) = self.stream.take()
            && let Ok(mut engine) = self.lock()
        {
            engine.close_stream(id);
        }
    }
}

fn speech_request(request: &Request) -> cactus::SpeechRequest<'_> {
    cactus::SpeechRequest {
        language: request.language.as_deref(),
        keywords: request.keywords.as_deref(),
        word_timestamps: true,
    }
}

impl Transcriber for Engine {
    fn start(
        &mut self,
        engine: &str,
        weights: &[u8],
        threads: u32,
    ) -> Result<(String, cactus::Threads), String> {
        engine.clone_into(&mut self.name);
        let mut engine = self.lock()?;
        let threads = engine.set_threads(threads);
        engine.load_speech(weights).map_err(|err| err.to_string())?;
        Ok((cactus::sha256_hex(weights), threads))
    }

    fn transcribe(&mut self, pcm: &[f32], request: &Request) -> Result<String, String> {
        self.lock()?
            .transcribe(pcm, speech_request(request))
            .map(|transcription| transcription.json)
            .map_err(|err| err.to_string())
    }

    fn open_stream(&mut self) -> Result<(), String> {
        if self.stream.is_none() {
            self.stream = Some(self.lock()?.open_stream().map_err(|err| err.to_string())?);
        }
        Ok(())
    }

    fn stream(
        &mut self,
        pcm: &[f32],
        request: &Request,
        last: bool,
    ) -> Result<Vec<String>, String> {
        let id = self.stream.ok_or("the stream is not open")?;
        let mut engine = self.lock()?;
        let mut steps = Vec::new();
        if !pcm.is_empty() {
            let step = engine.stream_process(id, pcm, speech_request(request));
            match step {
                Ok(step) => steps.push(step.json),
                Err(err) => {
                    // A failed step released the stream.
                    self.stream = None;
                    return Err(err.to_string());
                }
            }
        }
        if last {
            steps.push(engine.stream_stop(id).map_err(|err| err.to_string())?.json);
        }
        Ok(steps)
    }

    fn reset_stream(&mut self) {
        if let Some(id) = self.stream
            && let Ok(mut engine) = self.lock()
        {
            let _tail = engine.stream_stop(id);
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawWord {
    word: String,
    start: f64,
    end: f64,
}

#[derive(Debug, Deserialize)]
struct Raw {
    #[serde(default)]
    language: String,
    #[serde(default)]
    words: Vec<RawWord>,
    #[serde(default)]
    pending: String,
    #[serde(default)]
    received: f64,
}

/// A parsed transcription. Silence has no words and an empty language.
#[derive(Debug, Default, PartialEq)]
pub struct Transcript {
    pub language: String,
    /// Times are milliseconds from the first sample of the request, or of
    /// the stream.
    pub words: Vec<Word>,
    /// A stream's unconfirmed tail.
    pub pending: String,
    /// The stream's audio so far, in milliseconds.
    pub received_ms: i64,
}

/// Whistle reports times in seconds with two decimals.
#[expect(
    clippy::cast_possible_truncation,
    reason = "rounded and clamped to a day first"
)]
fn milliseconds(seconds: f64) -> i64 {
    let ms = (seconds * 1000.0).round();
    if ms.is_finite() {
        ms.clamp(0.0, 86_400_000.0) as i64
    } else {
        0
    }
}

pub fn parse(json: &str) -> Result<Transcript, String> {
    let raw: Raw =
        serde_json::from_str(json).map_err(|err| format!("parsing the transcription: {err}"))?;
    Ok(Transcript {
        language: raw.language,
        words: raw
            .words
            .into_iter()
            .map(|word| Word {
                text: word.word.trim().to_owned(),
                start_ms: milliseconds(word.start),
                end_ms: milliseconds(word.end).max(milliseconds(word.start)),
            })
            .filter(|word| !word.text.is_empty())
            .collect(),
        pending: raw.pending.trim().to_owned(),
        received_ms: milliseconds(raw.received),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_words_in_milliseconds() {
        let transcript = parse(
            r#"{"text":"The quick.","language":"en","words":[{"word":"The","start":0.00,"end":0.24,"probability":0.9},{"word":" quick.","start":0.24,"end":0.2,"probability":0.8}],"ttft_ms":1.0,"decode_tps":2.0}"#,
        )
        .expect("parse");
        assert_eq!(transcript.language, "en");
        assert_eq!(
            transcript.words,
            [
                Word {
                    text: "The".into(),
                    start_ms: 0,
                    end_ms: 240
                },
                Word {
                    text: "quick.".into(),
                    start_ms: 240,
                    end_ms: 240
                }
            ]
        );
        let silence =
            parse(r#"{"text":"","language":"","ttft_ms":0.0,"decode_tps":0.0}"#).expect("silence");
        assert_eq!(silence, Transcript::default());
        let step = parse(
            r#"{"text":"","words":[],"pending":"The Quick Brown Fox","language":"en","received":1.00,"pass_ms":13.2}"#,
        )
        .expect("stream step");
        assert_eq!(
            (step.pending.as_str(), step.received_ms),
            ("The Quick Brown Fox", 1000)
        );
        let _err = parse("not json").expect_err("not JSON");
    }
}
