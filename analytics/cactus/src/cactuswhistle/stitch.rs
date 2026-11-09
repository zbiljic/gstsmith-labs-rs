//! Commits words from overlapping transcription windows and groups them into
//! cues.
//!
//! Each window repeats some already committed audio as left context, so the
//! model hears every word with what came before it. Words ending before the
//! window's holdback margin are committed; words still being spoken near its
//! end are transcribed again with the next window, with their full audio.
//! Words close to that margin can be required to come out alike from two
//! consecutive windows first (local agreement, as in whisper-streaming).
//! Context words that repeat committed ones are dropped. The last cue stays
//! open across windows until a pause, a sentence end or the cue limits close
//! it, so sentences are not cut at window boundaries.

/// Alignment jitter between two transcriptions of the same audio; Whistle
/// times words in 80 ms steps.
const TOLERANCE_MS: i64 = 100;
/// A transcribed word this close to a committed word with the same text is
/// a duplicate.
const DUPLICATE_MS: i64 = 250;

/// A word on the stream timeline, in milliseconds since its first sample.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    /// The word as written in running text, with its punctuation.
    pub text: String,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Cue boundaries, in milliseconds and characters.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_duration_ms: i64,
    pub max_chars: usize,
    /// A longer silence between two words starts a new cue.
    pub max_pause_ms: i64,
    /// Sentence punctuation ends a cue once it lasts this long.
    pub min_sentence_ms: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_duration_ms: 7000,
            max_chars: 84,
            max_pause_ms: 1000,
            min_sentence_ms: 1200,
        }
    }
}

impl Limits {
    /// How long after its first word a cue can stay open: up to its longest
    /// duration, or just under it and then a pause.
    pub fn max_open_ms(&self) -> i64 {
        self.max_duration_ms + self.max_pause_ms
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// Whether a cue of two or more words is spoken faster than `max_rate`
/// words per second, timed to at least one 80 ms step. Whistle invents
/// short phrases such as "Thank you." in noise, crammed into a step or two;
/// people say at most about six words a second. `max_rate` 0 never drops.
/// Words are counted between spaces, so a stray punctuation token counts.
pub fn too_fast(cue: &Cue, max_rate: u32) -> bool {
    let words = cue.text.split_whitespace().count();
    let duration_ms = (cue.end_ms - cue.start_ms).max(80);
    max_rate > 0
        && words >= 2
        && i64::try_from(words)
            .unwrap_or(i64::MAX)
            .saturating_mul(1000)
            > i64::from(max_rate).saturating_mul(duration_ms)
}

/// Groups time-ordered words into cues.
pub fn group(words: &[Word], limits: Limits) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut current: Option<Cue> = None;
    for word in words.iter().filter(|word| !word.text.is_empty()) {
        if let Some(cue) = current.take() {
            if word.start_ms - cue.end_ms > limits.max_pause_ms
                || word.end_ms - cue.start_ms > limits.max_duration_ms
                || cue.text.chars().count() + 1 + word.text.chars().count() > limits.max_chars
            {
                cues.push(cue);
            } else {
                current = Some(cue);
            }
        }
        let cue = current.get_or_insert_with(|| Cue {
            start_ms: word.start_ms,
            end_ms: word.end_ms,
            text: String::new(),
        });
        if !cue.text.is_empty() {
            cue.text.push(' ');
        }
        cue.text.push_str(&word.text);
        cue.end_ms = cue.end_ms.max(word.end_ms);
        if ends_sentence(&word.text) && cue.end_ms - cue.start_ms >= limits.min_sentence_ms {
            cues.extend(current.take());
        }
    }
    cues.extend(current);
    cues
}

pub fn ends_sentence(text: &str) -> bool {
    text.trim_end_matches(['"', '\'', ')', '»', '”', '’'])
        .ends_with(['.', '?', '!', '…'])
}

/// Compares two transcriptions of a word, ignoring case and punctuation.
fn same_text(a: &str, b: &str) -> bool {
    let letters = |text: &str| {
        text.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    letters(a) == letters(b)
}

/// The words a window committed and the cues that closed.
#[derive(Debug, Default)]
pub struct Committed {
    pub words: Vec<Word>,
    pub cues: Vec<Cue>,
    /// The window was the last one or had stalled, so its words committed
    /// as heard, without holdback or agreement.
    pub forced: bool,
}

/// Audio covered by one transcription, on the stream timeline.
#[derive(Clone, Copy, Debug)]
pub struct Window {
    pub end_ms: i64,
    /// No more audio follows; commit everything.
    pub last: bool,
}

#[derive(Debug)]
pub struct Stitcher {
    context_ms: i64,
    holdback_ms: i64,
    /// Commits everything once this much audio is waiting, so one long word
    /// cannot stall the stream.
    max_pending_ms: i64,
    agreement_ms: i64,
    /// The words are final (the engine's stream): commit them all, as they
    /// come, and only group them into cues.
    trusted: bool,
    limits: Limits,
    committed_until: i64,
    /// Recently committed words, to recognize them in later context.
    history: Vec<Word>,
    /// The previous window's uncommitted words, for agreement.
    tentative: Vec<Word>,
    /// Committed words whose cue is still open.
    open: Vec<Word>,
}

impl Stitcher {
    pub fn new(context_ms: i64, holdback_ms: i64, max_pending_ms: i64, limits: Limits) -> Self {
        Self {
            context_ms,
            holdback_ms,
            max_pending_ms,
            agreement_ms: 0,
            trusted: false,
            limits,
            committed_until: 0,
            history: Vec::new(),
            tentative: Vec::new(),
            open: Vec::new(),
        }
    }

    /// Words ending within this much before the commit point wait until two
    /// consecutive windows transcribe them alike.
    pub fn with_agreement_ms(mut self, agreement_ms: i64) -> Self {
        self.agreement_ms = agreement_ms;
        self
    }

    /// For final words, such as the engine stream's: none is held back or
    /// dropped, and a window's end only says how far silence is settled.
    pub fn trusted(limits: Limits) -> Self {
        Self {
            trusted: true,
            ..Self::new(0, 0, i64::MAX, limits)
        }
    }

    /// Everything before this time has been committed.
    pub fn committed_until(&self) -> i64 {
        self.committed_until
    }

    /// Where the next window starts, to include the configured context.
    pub fn context_start(&self) -> i64 {
        (self.committed_until - self.context_ms).max(0)
    }

    /// The start of the open cue, before which no cue can follow.
    pub fn open_since(&self) -> Option<i64> {
        self.open.first().map(|word| word.start_ms)
    }

    /// Commits a window's words, given on the stream timeline in time order,
    /// and returns them with the cues that closed, in start order.
    pub fn accept(&mut self, words: Vec<Word>, window: Window) -> Committed {
        let stalled = window.end_ms - self.committed_until >= self.max_pending_ms;
        let forced = window.last || stalled;
        let horizon = if forced {
            window.end_ms
        } else {
            window.end_ms - self.holdback_ms
        };
        let mut candidates: Vec<Word> = words
            .into_iter()
            .filter(|word| {
                // A held word's next transcription may start a little before
                // the committed point, which that word itself set.
                self.trusted
                    || ((word.start_ms >= self.committed_until - TOLERANCE_MS || self.agrees(word))
                        && !self.is_duplicate(word))
            })
            .collect();
        candidates.sort_by_key(|word| (word.start_ms, word.end_ms));
        // Commit a time-ordered prefix; the first word that is not ready
        // holds everything after it.
        let ready = candidates
            .iter()
            .position(|word| {
                !self.trusted
                    && (word.end_ms > horizon
                        || !(forced
                            || word.end_ms <= horizon - self.agreement_ms
                            || self.agrees(word)))
            })
            .unwrap_or(candidates.len());
        let held = candidates.split_off(ready);
        let fresh = candidates;
        let committed = fresh.iter().map(|word| word.end_ms).max().unwrap_or(0);
        let advance = held
            .first()
            .map_or(horizon, |word| word.start_ms.min(horizon));
        self.committed_until = self.committed_until.max(advance).max(committed);
        self.tentative = held;
        self.history.extend(fresh.iter().cloned());
        let keep_from = self.committed_until - self.context_ms - 1000;
        self.history.retain(|word| word.end_ms >= keep_from);
        let cues = self.close_cues(&fresh, forced);
        Committed {
            words: fresh,
            cues,
            forced,
        }
    }

    /// Groups the open and newly committed words into cues and returns those
    /// that can no longer grow, which is all but possibly the last.
    fn close_cues(&mut self, fresh: &[Word], forced: bool) -> Vec<Cue> {
        let mut pending = std::mem::take(&mut self.open);
        pending.extend(fresh.iter().cloned());
        // A fresh word may start a little before an open one ends.
        pending.sort_by_key(|word| (word.start_ms, word.end_ms));
        let mut cues = group(&pending, self.limits);
        let Some(last) = cues.last() else {
            return cues;
        };
        let limits = self.limits;
        let committed = self.committed_until;
        let closed = forced
            || (ends_sentence(&last.text) && last.end_ms - last.start_ms >= limits.min_sentence_ms)
            || committed - last.end_ms > limits.max_pause_ms
            || committed - last.start_ms >= limits.max_duration_ms;
        if !closed {
            let start = last.start_ms;
            self.open = pending
                .into_iter()
                .filter(|word| word.start_ms >= start)
                .collect();
            cues.pop();
        }
        cues
    }

    fn agrees(&self, word: &Word) -> bool {
        self.tentative.iter().any(|seen| {
            (seen.start_ms - word.start_ms).abs() <= DUPLICATE_MS
                && same_text(&seen.text, &word.text)
        })
    }

    fn is_duplicate(&self, word: &Word) -> bool {
        self.history.iter().any(|known| {
            (known.start_ms - word.start_ms).abs() <= DUPLICATE_MS
                && same_text(&known.text, &word.text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, start_ms: i64, end_ms: i64) -> Word {
        Word {
            text: text.into(),
            start_ms,
            end_ms,
        }
    }

    fn texts(cues: &[Cue]) -> Vec<(i64, &str)> {
        cues.iter()
            .map(|cue| (cue.start_ms, cue.text.as_str()))
            .collect()
    }

    fn window(end_ms: i64, last: bool) -> Window {
        Window { end_ms, last }
    }

    fn stitcher() -> Stitcher {
        Stitcher::new(5000, 1000, 30_000, Limits::default())
    }

    #[test]
    fn drops_phrases_spoken_too_fast() {
        let cue = |text: &str, start_ms, end_ms| Cue {
            start_ms,
            end_ms,
            text: text.to_owned(),
        };
        assert!(too_fast(&cue("Thank you.", 0, 80), 10));
        assert!(too_fast(&cue("Thank you.", 0, 0), 10));
        assert!(!too_fast(&cue("Thank you.", 0, 80), 0));
        assert!(!too_fast(&cue("Thank you.", 0, 240), 10));
        assert!(!too_fast(&cue("Yes.", 0, 0), 10));
        // A fast cue measured in clean speech (the fastest was 5 words a
        // second).
        assert!(!too_fast(
            &cue("where are we with the new cluster?", 0, 1520),
            10
        ));
    }

    #[test]
    fn groups_at_sentences_pauses_and_limits() {
        let words = [
            word("Okay,", 0, 400),
            word("Houston.", 400, 1400),
            word("Say", 1500, 1900),
            word("again.", 1900, 2600),
            word("We", 5000, 5200),
            word("have", 5200, 5400),
        ];
        assert_eq!(
            texts(&group(&words, Limits::default())),
            [
                (0, "Okay, Houston."),
                (1500, "Say again."),
                (5000, "We have")
            ]
        );
        let words: Vec<_> = (0..12)
            .map(|i| word("word", i * 1000, i * 1000 + 900))
            .collect();
        let cues = group(&words, Limits::default());
        assert_eq!(cues.len(), 2);
        assert!(cues.iter().all(|cue| cue.end_ms - cue.start_ms <= 7000));
        let words: Vec<_> = (0..30)
            .map(|i| word("abcdefghi", i * 100, i * 100 + 90))
            .collect();
        assert!(
            group(&words, Limits::default())
                .iter()
                .all(|cue| cue.text.chars().count() <= 84)
        );
    }

    #[test]
    fn defers_words_near_the_window_end() {
        let mut stitcher = stitcher();
        let committed = stitcher.accept(
            vec![word("Hello.", 5000, 6400), word("There", 9200, 9900)],
            window(10_000, false),
        );
        assert_eq!(texts(&committed.cues), [(5000, "Hello.")]);
        assert_eq!(stitcher.committed_until(), 9000);
        assert_eq!(stitcher.context_start(), 4000);
        // The next window repeats context and transcribes the held word with
        // its full audio.
        let committed = stitcher.accept(
            vec![
                word("Hello.", 5030, 6420),
                word("There", 9210, 9950),
                word("friend.", 10_000, 10_500),
            ],
            window(20_000, false),
        );
        assert_eq!(texts(&committed.cues), [(9210, "There friend.")]);
        assert_eq!(stitcher.committed_until(), 19_000);
    }

    #[test]
    fn continues_a_sentence_across_windows() {
        let mut stitcher = stitcher();
        let committed = stitcher.accept(
            vec![word("We", 3000, 3300), word("have", 3300, 3700)],
            window(5000, false),
        );
        assert_eq!(texts(&committed.cues), []);
        assert_eq!(stitcher.open_since(), Some(3000));
        let committed = stitcher.accept(
            vec![
                word("We", 3010, 3300),
                word("have", 3300, 3700),
                word("a", 4100, 4200),
                word("problem.", 4200, 4900),
            ],
            window(10_000, false),
        );
        assert_eq!(texts(&committed.cues), [(3000, "We have a problem.")]);
        assert_eq!(stitcher.open_since(), None);
    }

    #[test]
    fn agreement_waits_for_a_second_transcription() {
        let mut stitcher = stitcher().with_agreement_ms(3000);
        let committed = stitcher.accept(
            vec![word("Early.", 1000, 2300), word("Maybe", 7000, 7400)],
            window(10_000, false),
        );
        assert_eq!(texts(&committed.cues), [(1000, "Early.")]);
        assert_eq!(stitcher.committed_until(), 7000);
        // The next transcription changes the tentative word; only the agreed
        // one commits.
        let committed = stitcher.accept(
            vec![
                word("Early.", 1000, 2300),
                word("Maybe,", 7010, 7400),
                word("yes.", 7500, 8800),
            ],
            window(12_000, false),
        );
        assert_eq!(texts(&committed.cues), []);
        assert_eq!(stitcher.committed_until(), 7500);
        let committed = stitcher.accept(
            vec![word("Maybe,", 7000, 7400), word("yes.", 7520, 8800)],
            window(15_000, false),
        );
        assert_eq!(texts(&committed.cues), [(7010, "Maybe, yes.")]);
    }

    #[test]
    fn keeps_a_held_word_whose_alignment_moves_earlier() {
        let mut stitcher = stitcher();
        let _committed = stitcher.accept(
            vec![word("Okay.", 3000, 5400), word("there", 8700, 9900)],
            window(10_000, false),
        );
        assert_eq!(stitcher.committed_until(), 8700);
        let committed = stitcher.accept(
            vec![word("Okay.", 3000, 5400), word("there.", 8550, 9900)],
            window(20_000, true),
        );
        assert_eq!(texts(&committed.cues), [(8550, "there.")]);
    }

    #[test]
    fn skips_duplicates_and_advances_through_silence() {
        let mut stitcher = stitcher();
        let _committed = stitcher.accept(vec![word("One", 8800, 8990)], window(10_000, false));
        let committed = stitcher.accept(vec![word("One", 8850, 9050)], window(20_000, false));
        // Only the first transcription's cue, which waited for a possible
        // continuation.
        assert_eq!(texts(&committed.cues), [(8800, "One")]);
        assert_eq!(stitcher.committed_until(), 19_000);
    }

    #[test]
    fn commits_a_stalled_window() {
        let mut stitcher = Stitcher::new(5000, 1000, 15_000, Limits::default());
        let committed = stitcher.accept(vec![word("Long", 100, 15_900)], window(16_000, false));
        assert_eq!(texts(&committed.cues), [(100, "Long")]);
        assert_eq!(stitcher.committed_until(), 16_000);
    }

    #[test]
    fn trusted_words_commit_even_behind_the_settled_point() {
        let mut stitcher = Stitcher::trusted(Limits::default());
        let committed = stitcher.accept(vec![word("Hello.", 0, 1300)], window(3000, false));
        assert_eq!(texts(&committed.cues), [(0, "Hello.")]);
        assert_eq!(stitcher.committed_until(), 3000);
        // Confirmed late, starting before the point silence was settled to,
        // and repeated: still committed, in order.
        let committed = stitcher.accept(
            vec![
                word("No,", 2500, 2700),
                word("no,", 2750, 2900),
                word("no.", 2950, 4000),
            ],
            window(4000, false),
        );
        assert_eq!(texts(&committed.cues), [(2500, "No, no, no.")]);
    }

    #[test]
    fn the_last_window_closes_everything() {
        let mut stitcher = stitcher();
        let committed = stitcher.accept(
            vec![word("No", 1000, 1200), word("end", 1300, 1500)],
            window(1600, true),
        );
        assert_eq!(texts(&committed.cues), [(1000, "No end")]);
        assert_eq!(stitcher.open_since(), None);
    }
}
