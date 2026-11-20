//! Session speakers: online clustering of cue embeddings, speaker numbers,
//! merges, and the labels that name them.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::BuildHasher as _;

use crate::embedder::cosine;

/// How cues become speakers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rules {
    /// A cue at least this close to a speaker is that speaker.
    pub matching: f32,
    /// A cue too short to start a speaker goes to the closest one if it is
    /// at least this close, without refining it. At `matching` or above,
    /// short cues match like any other.
    pub short_matching: f32,
    /// A cue that matches no speaker starts one if it is at least this long.
    pub min_new_ms: u64,
    /// A matched cue refines its speaker if it is at least this long.
    pub min_update_ms: u64,
    /// Once this many speakers exist, a cue that matches none goes to the
    /// closest one, which it does not refine. 0 is unlimited.
    pub max_speakers: u32,
}

/// One speaker of the session.
#[derive(Clone, Debug)]
pub struct Speaker {
    /// Numbered from 1 in order of appearance; never reused.
    pub n: u32,
    pub id: String,
    /// Sum of the embeddings that refined the speaker, and its direction.
    sum: Vec<f32>,
    centroid: Vec<f32>,
    /// Cues and milliseconds of audio that refined the speaker.
    pub cues: u32,
    pub ms: u64,
}

/// What a cue was assigned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Outcome {
    /// Speaker `n`. `confidence` is the cosine to the speaker before this
    /// cue, and unset for a speaker the cue started.
    Speaker {
        n: u32,
        confidence: Option<f32>,
        new: bool,
    },
    /// No speaker: the cue matched none, not even as a short cue, and was
    /// too short to start one.
    /// `confidence` is the closest speaker's cosine, if there is one.
    Unknown { confidence: Option<f32> },
}

/// The session's speakers.
#[derive(Debug)]
pub struct Registry {
    rules: Rules,
    speakers: Vec<Speaker>,
    next_n: u32,
    /// Merged-away numbers and the number they now belong to.
    aliases: HashMap<u32, u32>,
    /// The ids of merged-away speakers, by number.
    retired: HashMap<u32, String>,
    ids: std::hash::RandomState,
}

/// `S{n}`, the label of an unnamed speaker.
#[must_use]
pub fn number_key(n: u32) -> String {
    format!("S{n}")
}

fn parse_number_key(key: &str) -> Option<u32> {
    key.strip_prefix('S')
        .filter(|digits| digits.bytes().all(|b| b.is_ascii_digit()) && !digits.starts_with('0'))
        .and_then(|digits| digits.parse().ok())
        .filter(|&n| n > 0)
}

fn normalized(sum: &[f32]) -> Vec<f32> {
    let norm = sum.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        sum.iter().map(|v| v / norm).collect()
    } else {
        sum.to_vec()
    }
}

impl Registry {
    #[must_use]
    pub fn new(rules: Rules) -> Self {
        Self {
            rules,
            speakers: Vec::new(),
            next_n: 1,
            aliases: HashMap::new(),
            retired: HashMap::new(),
            ids: std::hash::RandomState::new(),
        }
    }

    #[must_use]
    pub fn rules(&self) -> Rules {
        self.rules
    }

    /// The live speakers, in order of appearance.
    #[must_use]
    pub fn speakers(&self) -> &[Speaker] {
        &self.speakers
    }

    /// A short id, random per process, so ids of different sessions differ.
    fn new_id(&self, n: u32) -> String {
        const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
        let mut bits = self.ids.hash_one(n);
        (0..8)
            .map(|_| {
                let index = usize::try_from(bits & 31).unwrap_or(0);
                bits >>= 5;
                char::from(ALPHABET.get(index).copied().unwrap_or(b'a'))
            })
            .collect()
    }

    /// The number speaker `n` now goes by, after merges.
    #[must_use]
    pub fn current(&self, n: u32) -> u32 {
        let mut n = n;
        while let Some(&to) = self.aliases.get(&n) {
            n = to;
        }
        n
    }

    /// The live speaker numbered `n`, after merges.
    #[must_use]
    pub fn speaker(&self, n: u32) -> Option<&Speaker> {
        let n = self.current(n);
        self.speakers.iter().find(|speaker| speaker.n == n)
    }

    /// The live speaker a key names: `S{n}` or a speaker id, including the
    /// number or id of a speaker merged into it.
    #[must_use]
    pub fn find(&self, key: &str) -> Option<&Speaker> {
        if let Some(n) = parse_number_key(key) {
            return self.speaker(n);
        }
        self.speakers
            .iter()
            .find(|speaker| speaker.id == key)
            .or_else(|| {
                let (&n, _) = self.retired.iter().find(|(_, id)| *id == key)?;
                self.speaker(n)
            })
    }

    /// The keys that name speaker `n`, most specific first: its id and
    /// number, then the id and number of each speaker merged into it.
    #[must_use]
    pub fn keys_of(&self, n: u32) -> Vec<String> {
        let Some(speaker) = self.speaker(n) else {
            return Vec::new();
        };
        let mut keys = vec![speaker.id.clone(), number_key(speaker.n)];
        for merged in self.numbers_of(speaker.n).into_iter().skip(1) {
            keys.extend(self.retired.get(&merged).cloned());
            keys.push(number_key(merged));
        }
        keys
    }

    /// Every number that now belongs to speaker `n`, its own first.
    #[must_use]
    pub fn numbers_of(&self, n: u32) -> Vec<u32> {
        let mut numbers = vec![n];
        let mut merged: Vec<u32> = self
            .aliases
            .keys()
            .copied()
            .filter(|&old| old != n && self.current(old) == n)
            .collect();
        merged.sort_unstable();
        numbers.extend(merged);
        numbers
    }

    /// Assigns a unit-length cue embedding of `duration_ms` to a speaker.
    pub fn assign(&mut self, embedding: &[f32], duration_ms: u64) -> Outcome {
        let best = self
            .speakers
            .iter()
            .enumerate()
            .filter_map(|(index, speaker)| Some((index, cosine(embedding, &speaker.centroid)?)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        let confidence = best.map(|(_, score)| score);
        if let Some((index, score)) = best {
            let full = self.rules.max_speakers > 0
                && self.speakers.len() >= usize::try_from(self.rules.max_speakers).unwrap_or(0);
            let short = duration_ms < self.rules.min_new_ms && score >= self.rules.short_matching;
            if score >= self.rules.matching || full || short {
                let refine =
                    score >= self.rules.matching && duration_ms >= self.rules.min_update_ms;
                let Some(speaker) = self.speakers.get_mut(index) else {
                    return Outcome::Unknown { confidence };
                };
                if refine {
                    for (sum, value) in speaker.sum.iter_mut().zip(embedding) {
                        *sum += value;
                    }
                    speaker.centroid = normalized(&speaker.sum);
                    speaker.cues += 1;
                    speaker.ms += duration_ms;
                }
                return Outcome::Speaker {
                    n: speaker.n,
                    confidence,
                    new: false,
                };
            }
        }
        if duration_ms < self.rules.min_new_ms {
            return Outcome::Unknown { confidence };
        }
        let n = self.next_n;
        self.next_n += 1;
        let id = self.new_id(n);
        self.speakers.push(Speaker {
            n,
            id,
            sum: embedding.to_vec(),
            centroid: normalized(embedding),
            cues: 1,
            ms: duration_ms,
        });
        Outcome::Speaker {
            n,
            confidence: None,
            new: true,
        }
    }

    /// Folds speaker `drop` into `keep`, for when one person was split in
    /// two. `keep` keeps its number and id; the evidence of both refines it,
    /// and cues that would have gone to `drop` go to `keep`. Returns the
    /// numbers merged, or `None` if either is unknown or they are the same.
    pub fn merge(&mut self, keep: &str, drop: &str) -> Option<(u32, u32)> {
        let keep = self.find(keep)?.n;
        let drop = self.find(drop)?.n;
        if keep == drop {
            return None;
        }
        let index = self.speakers.iter().position(|speaker| speaker.n == drop)?;
        let dropped = self.speakers.remove(index);
        let kept = self.speakers.iter_mut().find(|speaker| speaker.n == keep)?;
        for (sum, value) in kept.sum.iter_mut().zip(&dropped.sum) {
            *sum += value;
        }
        kept.centroid = normalized(&kept.sum);
        kept.cues += dropped.cues;
        kept.ms += dropped.ms;
        self.aliases.insert(drop, keep);
        self.retired.insert(drop, dropped.id);
        Some((keep, drop))
    }
}

/// Whether `key` can name a speaker: `S{n}`, or the id of a live one.
#[must_use]
pub fn is_key(registry: Option<&Registry>, key: &str) -> bool {
    parse_number_key(key).is_some() || registry.is_some_and(|registry| registry.find(key).is_some())
}

/// Speaker names, keyed by `S{n}` or speaker id.
pub type Names = HashMap<String, String>;

/// The name of speaker `n`: by its id first, then by its own number, then
/// by the ids and numbers of speakers merged into it, so the kept speaker's
/// name wins.
#[must_use]
pub fn name_of<'a>(registry: &Registry, names: &'a Names, n: u32) -> Option<&'a str> {
    registry
        .keys_of(n)
        .iter()
        .find_map(|key| names.get(key))
        .map(String::as_str)
        .filter(|name| !name.is_empty())
}

/// What a label format can say about a cue's speaker.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fields<'a> {
    pub n: Option<u32>,
    pub id: Option<&'a str>,
    pub name: Option<&'a str>,
    pub confidence: Option<f32>,
}

impl Fields<'_> {
    /// The name, or `S{n}` when there is none.
    #[must_use]
    pub fn label(&self) -> Option<String> {
        self.name
            .map(str::to_owned)
            .or_else(|| self.n.map(number_key))
    }
}

/// Fills `{n}`, `{id}`, `{name}`, `{label}` and `{confidence}` in
/// `format`. Unset fields become empty; other braces are kept as written.
#[must_use]
pub fn format_label(format: &str, fields: &Fields<'_>) -> String {
    let mut out = String::with_capacity(format.len() + 16);
    let mut rest = format;
    while let Some(open) = rest.find('{') {
        let (before, after) = rest.split_at(open);
        out.push_str(before);
        let Some(close) = after.find('}') else {
            out.push_str(after);
            return out;
        };
        let (field, tail) = after.split_at(close + 1);
        match field {
            "{n}" => {
                if let Some(n) = fields.n {
                    out.push_str(&n.to_string());
                }
            }
            "{id}" => out.push_str(fields.id.unwrap_or_default()),
            "{name}" => out.push_str(fields.name.unwrap_or_default()),
            "{label}" => out.push_str(&fields.label().unwrap_or_default()),
            "{confidence}" => {
                if let Some(confidence) = fields.confidence {
                    let _infallible = write!(out, "{confidence:.2}");
                }
            }
            _ => {
                // Not a placeholder: keep the brace and look again after it.
                out.push('{');
                rest = after.get(1..).unwrap_or_default();
                continue;
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: Rules = Rules {
        matching: 0.5,
        short_matching: 0.2,
        min_new_ms: 1000,
        min_update_ms: 1000,
        max_speakers: 0,
    };

    /// A unit vector mostly along axis `axis`, nudged along the next one.
    fn voice(axis: usize, nudge: f32) -> Vec<f32> {
        let mut v = vec![0.0; 4];
        if let Some(value) = v.get_mut(axis) {
            *value = 1.0;
        }
        if let Some(value) = v.get_mut((axis + 1) % 4) {
            *value = nudge;
        }
        normalized(&v)
    }

    #[test]
    fn cues_join_the_closest_speaker_or_start_one() {
        let mut registry = Registry::new(RULES);
        assert_eq!(
            registry.assign(&voice(0, 0.0), 500),
            Outcome::Unknown { confidence: None },
            "too short to start a speaker"
        );
        assert_eq!(
            registry.assign(&voice(0, 0.0), 2000),
            Outcome::Speaker {
                n: 1,
                confidence: None,
                new: true
            }
        );
        let Outcome::Speaker {
            n: 1,
            confidence: Some(score),
            new: false,
        } = registry.assign(&voice(0, 0.3), 500)
        else {
            panic!("a short cue still matches");
        };
        assert!(score > 0.9);
        assert_eq!(
            registry.speaker(1).map(|s| s.cues),
            Some(1),
            "short: no update"
        );
        assert!(matches!(
            registry.assign(&voice(1, 0.0), 1500),
            Outcome::Speaker {
                n: 2,
                new: true,
                ..
            }
        ));
        assert!(matches!(
            registry.assign(&voice(0, 0.2), 1500),
            Outcome::Speaker {
                n: 1,
                new: false,
                ..
            }
        ));
        assert_eq!(registry.speaker(1).map(|s| (s.cues, s.ms)), Some((2, 3500)));
        let Outcome::Unknown {
            confidence: Some(score),
        } = registry.assign(&voice(2, 0.0), 900)
        else {
            panic!("a short cue that matches nobody is unknown");
        };
        assert!(score < 0.5);
        let ids: Vec<_> = registry.speakers().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|id| id.len() == 8), "{ids:?}");
        assert_ne!(ids.first(), ids.get(1));
    }

    #[test]
    fn short_cues_join_a_closer_speaker_without_refining_it() {
        let mut registry = Registry::new(RULES);
        registry.assign(&voice(0, 0.0), 2000);
        // Cosine 0.3 to S1: below matching, above short_matching.
        let near = normalized(&[0.3, 0.0, 0.0, 0.954]);
        let Outcome::Speaker {
            n: 1,
            confidence: Some(score),
            new: false,
        } = registry.assign(&near, 800)
        else {
            panic!("a short cue joins the closest speaker");
        };
        assert!((score - 0.3).abs() < 0.01, "{score}");
        assert_eq!(
            registry.speaker(1).map(|s| (s.cues, s.ms)),
            Some((1, 2000)),
            "without refining it"
        );
        assert!(
            matches!(
                registry.assign(&normalized(&[0.1, 0.0, 0.0, 0.995]), 800),
                Outcome::Unknown { .. }
            ),
            "below short_matching"
        );
        assert!(
            matches!(
                registry.assign(&near, 1000),
                Outcome::Speaker {
                    n: 2,
                    new: true,
                    ..
                }
            ),
            "a cue long enough to start a speaker does"
        );
        let mut strict = Registry::new(Rules {
            short_matching: 0.5,
            ..RULES
        });
        strict.assign(&voice(0, 0.0), 2000);
        assert!(
            matches!(strict.assign(&near, 800), Outcome::Unknown { .. }),
            "short_matching at matching turns it off"
        );
        // Long enough to refine, too short to start a speaker: only the
        // match below `matching` keeps it from refining.
        let mut defaults = Registry::new(Rules {
            min_new_ms: 1500,
            ..RULES
        });
        defaults.assign(&voice(0, 0.0), 2000);
        assert!(matches!(
            defaults.assign(&near, 1200),
            Outcome::Speaker {
                n: 1,
                new: false,
                ..
            }
        ));
        assert_eq!(
            defaults.speaker(1).map(|s| (s.cues, s.ms)),
            Some((1, 2000)),
            "a short match never refines"
        );
    }

    #[test]
    fn a_full_registry_sends_strangers_to_the_closest_speaker() {
        let mut registry = Registry::new(Rules {
            max_speakers: 2,
            ..RULES
        });
        registry.assign(&voice(0, 0.0), 2000);
        registry.assign(&voice(2, 0.0), 2000);
        // Along axis 3, leaning towards S1's axis 0.
        assert!(matches!(
            registry.assign(&voice(3, 0.5), 2000),
            Outcome::Speaker {
                n: 1,
                new: false,
                ..
            }
        ));
        assert_eq!(registry.speakers().len(), 2);
        assert_eq!(
            registry.speaker(1).map(|s| s.cues),
            Some(1),
            "a stranger does not refine the speaker"
        );
    }

    #[test]
    fn merged_speakers_keep_the_first_number_and_names() {
        let mut registry = Registry::new(RULES);
        registry.assign(&voice(0, 0.0), 2000);
        registry.assign(&voice(1, 0.0), 2000);
        registry.assign(&voice(2, 0.0), 2000);
        let id3 = registry
            .speaker(3)
            .map(|s| s.id.clone())
            .unwrap_or_default();
        assert_eq!(registry.merge("S1", "S1"), None);
        assert_eq!(registry.merge("S1", "S9"), None);
        assert_eq!(registry.merge("S1", &id3), Some((1, 3)));
        assert_eq!(registry.merge("S2", "S3"), Some((2, 1)), "S3 is now S1");
        assert_eq!(registry.speakers().len(), 1);
        assert_eq!(registry.current(3), 2);
        assert_eq!(registry.numbers_of(2), vec![2, 1, 3]);
        assert!(matches!(
            registry.assign(&voice(2, 0.0), 2000),
            Outcome::Speaker {
                n: 2,
                new: false,
                ..
            }
        ));
        assert!(
            matches!(
                registry.assign(&voice(3, 0.0), 2000),
                Outcome::Speaker {
                    n: 4,
                    new: true,
                    ..
                }
            ),
            "numbers are not reused"
        );

        let mut names = Names::new();
        names.insert(id3.clone(), "Swigert".to_owned());
        assert_eq!(
            name_of(&registry, &names, 2),
            Some("Swigert"),
            "by a merged id"
        );
        assert_eq!(registry.find(&id3).map(|s| s.n), Some(2));
        names.clear();
        names.insert("S3".to_owned(), "Haise".to_owned());
        assert_eq!(name_of(&registry, &names, 2), Some("Haise"));
        names.insert("S2".to_owned(), "Lousma".to_owned());
        assert_eq!(name_of(&registry, &names, 3), Some("Lousma"));
        let id2 = registry
            .speaker(2)
            .map(|s| s.id.clone())
            .unwrap_or_default();
        names.insert(id2.clone(), "Swigert".to_owned());
        assert_eq!(name_of(&registry, &names, 2), Some("Swigert"));
        names.insert(id2, String::new());
        assert_eq!(name_of(&registry, &names, 2), None, "empty clears");
        assert!(is_key(Some(&registry), "S7"));
        assert!(is_key(None, "S7"));
        assert!(!is_key(Some(&registry), "S0"));
        assert!(!is_key(Some(&registry), "nobody"));
        assert!(!is_key(Some(&registry), "S-1"));
        assert!(!is_key(None, "S01"), "S1 is written one way");
        let id4 = registry
            .speaker(4)
            .map(|s| s.id.clone())
            .unwrap_or_default();
        assert!(is_key(Some(&registry), &id4));
        assert!(!is_key(None, &id4));
    }

    #[test]
    fn labels_fill_placeholders() {
        let fields = Fields {
            n: Some(2),
            id: Some("k3f9q2ab"),
            name: None,
            confidence: Some(0.714),
        };
        assert_eq!(format_label("{label}: ", &fields), "S2: ");
        assert_eq!(
            format_label("[{n}|{id}|{name}|{confidence}] ", &fields),
            "[2|k3f9q2ab||0.71] "
        );
        let named = Fields {
            name: Some("Lousma"),
            ..fields
        };
        assert_eq!(format_label("{label}: ", &named), "Lousma: ");
        assert_eq!(format_label("{x} {{label}} {", &named), "{x} {Lousma} {");
        assert_eq!(format_label("", &named), "");
        assert_eq!(format_label("{label}", &Fields::default()), "");
    }
}
