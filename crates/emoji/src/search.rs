//! FST-backed emoji search.
//!
//! The term index maps normalized terms (lowercase, separators collapsed to
//! `_`) to posting lists of `(emoji id, kind)`. A query is scored by:
//!
//! 1. a prefix scan for the whole query, which hits full shortcodes and names
//!    (`slightly_sm` → `slightly_smiling_face`);
//! 2. for multi-word queries, a prefix scan per word, intersected, so word
//!    order doesn't matter (`face smiling`);
//! 3. if that leaves fewer than `limit` results, a Levenshtein-1 prefix scan
//!    so a typo (`smlie`) still finds things.
//!
//! Scores favor shortcodes over names over keywords, exact terms over prefixes,
//! and short completions over long ones; ties fall back to picker order, which
//! puts common smileys first. Callers can add a per-emoji boost (recency /
//! frequency of use).

use fst::automaton::{Automaton, Levenshtein, Str};
use fst::{IntoStreamer, Streamer};

use crate::{EMOJI, Emoji, EmojiId, POSTINGS, TERMS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum TermKind {
    Shortcode = 0,
    Name = 1,
    Word = 2,
    Tag = 3,
}

impl TermKind {
    fn from_bits(bits: u32) -> Self {
        match bits & 0xff {
            0 => TermKind::Shortcode,
            1 => TermKind::Name,
            2 => TermKind::Word,
            _ => TermKind::Tag,
        }
    }

    fn base_score(self) -> f32 {
        match self {
            TermKind::Shortcode => 100.0,
            TermKind::Name => 90.0,
            TermKind::Word => 70.0,
            TermKind::Tag => 55.0,
        }
    }
}

const EXACT_BONUS: f32 = 40.0;
const FUZZY_PENALTY: f32 = 45.0;
/// Cost per unmatched trailing byte of a prefix match, capped.
const LENGTH_PENALTY: f32 = 1.5;
const MAX_LENGTH_PENALTY: f32 = 30.0;

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub limit: usize,
    /// Exclude emoji newer than this Emoji version (platform font support).
    pub max_version: f32,
    /// Allow typo-tolerant matching when exact prefixes run short.
    pub fuzzy: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            max_version: f32::INFINITY,
            fuzzy: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Match {
    pub emoji: &'static Emoji,
    pub score: f32,
}

/// Searches with no usage boost.
pub fn search(query: &str, options: &SearchOptions) -> Vec<Match> {
    search_with_boost(query, options, |_| 0.0)
}

/// Searches, adding `boost(id)` to every matched emoji's score.
pub fn search_with_boost(
    query: &str,
    options: &SearchOptions,
    boost: impl Fn(EmojiId) -> f32,
) -> Vec<Match> {
    let query = normalize(query);
    if query.is_empty() || options.limit == 0 {
        return Vec::new();
    }

    let mut scores = Scores::new();
    scan(&query, Str::new(&query).starts_with(), query.len(), 0.0, &mut scores);

    let words: Vec<&str> = query.split('_').filter(|w| !w.is_empty()).collect();
    if words.len() > 1 {
        let mut combined: Option<Scores> = None;
        for word in &words {
            let mut word_scores = Scores::new();
            scan(word, Str::new(word).starts_with(), word.len(), 0.0, &mut word_scores);
            combined = Some(match combined {
                None => word_scores,
                Some(acc) => acc.intersect_sum(&word_scores),
            });
        }
        if let Some(combined) = combined {
            // Averaged so a two-word hit ranks like a one-word hit of the same quality,
            // minus a little so whole-phrase matches still win.
            scores.merge_max(&combined.scaled(1.0 / words.len() as f32, -5.0));
        }
    }

    if options.fuzzy && scores.count(options.max_version) < options.limit {
        let last = *words.last().unwrap_or(&query.as_str());
        if last.chars().count() >= 3 {
            let mut fuzzy = Scores::new();
            if let Ok(lev) = Levenshtein::new(last, 1) {
                scan(last, lev.starts_with(), last.len(), FUZZY_PENALTY, &mut fuzzy);
            }
            // Levenshtein counts a swapped pair as two edits; try each swap as a prefix.
            let chars: Vec<char> = last.chars().collect();
            for i in 0..chars.len() - 1 {
                if chars[i] == chars[i + 1] {
                    continue;
                }
                let mut swapped = chars.clone();
                swapped.swap(i, i + 1);
                let swapped: String = swapped.into_iter().collect();
                scan(&swapped, Str::new(&swapped).starts_with(), swapped.len(), FUZZY_PENALTY, &mut fuzzy);
            }
            if words.len() > 1 {
                // Keep earlier words as hard constraints.
                let mut constraint: Option<Scores> = None;
                for word in &words[..words.len() - 1] {
                    let mut s = Scores::new();
                    scan(word, Str::new(word).starts_with(), word.len(), 0.0, &mut s);
                    constraint = Some(match constraint {
                        None => s,
                        Some(acc) => acc.intersect_sum(&s),
                    });
                }
                if let Some(constraint) = constraint {
                    fuzzy = constraint.intersect_sum(&fuzzy).scaled(1.0 / words.len() as f32, -5.0);
                }
            }
            scores.merge_max(&fuzzy);
        }
    }

    scores.top(options, boost)
}

/// Streams every term accepted by `automaton` and folds its postings into `scores`.
fn scan<A: Automaton>(query: &str, automaton: A, query_len: usize, penalty: f32, scores: &mut Scores) {
    let mut stream = TERMS.search(automaton).into_stream();
    while let Some((term, packed)) = stream.next() {
        let offset = (packed >> 32) as usize;
        let len = (packed & 0xffff_ffff) as usize;
        let exact = term == query.as_bytes();
        let extra = term.len().saturating_sub(query_len) as f32;
        let term_score = if exact { EXACT_BONUS } else { 0.0 } - (extra * LENGTH_PENALTY).min(MAX_LENGTH_PENALTY) - penalty;
        for &posting in &POSTINGS[offset..offset + len] {
            let id = (posting >> 8) as usize;
            let score = TermKind::from_bits(posting).base_score() + term_score;
            scores.offer(id, score);
        }
    }
}

/// One score slot per emoji; `NEG_INFINITY` = unmatched.
struct Scores(Box<[f32]>);

impl Scores {
    fn new() -> Self {
        Scores(vec![f32::NEG_INFINITY; EMOJI.len()].into_boxed_slice())
    }

    fn offer(&mut self, id: usize, score: f32) {
        let slot = &mut self.0[id];
        if score > *slot {
            *slot = score;
        }
    }

    fn merge_max(&mut self, other: &Scores) {
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            if *b > *a {
                *a = *b;
            }
        }
    }

    fn intersect_sum(mut self, other: &Scores) -> Scores {
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            *a = if a.is_finite() && b.is_finite() { *a + *b } else { f32::NEG_INFINITY };
        }
        self
    }

    fn scaled(mut self, factor: f32, offset: f32) -> Scores {
        for a in self.0.iter_mut().filter(|a| a.is_finite()) {
            *a = *a * factor + offset;
        }
        self
    }

    fn count(&self, max_version: f32) -> usize {
        self.0
            .iter()
            .zip(EMOJI.iter())
            .filter(|(s, e)| s.is_finite() && e.version <= max_version)
            .count()
    }

    fn top(&self, options: &SearchOptions, boost: impl Fn(EmojiId) -> f32) -> Vec<Match> {
        let mut matches: Vec<Match> = self
            .0
            .iter()
            .zip(EMOJI.iter())
            .filter(|(s, e)| s.is_finite() && e.version <= options.max_version)
            .map(|(s, e)| Match {
                emoji: e,
                score: s + boost(e.id),
            })
            .collect();
        let order = |a: &Match, b: &Match| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.emoji.id.cmp(&b.emoji.id))
        };
        if matches.len() > options.limit {
            matches.select_nth_unstable_by(options.limit - 1, order);
            matches.truncate(options.limit);
        }
        matches.sort_unstable_by(order);
        matches
    }
}

/// Lowercases and collapses every run of separators to a single `_`.
/// Must match `normalize` in build.rs.
pub(crate) fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_sep = false;
    for c in s.chars() {
        if c.is_alphanumeric() || c == '+' {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.extend(c.to_lowercase());
        } else if c == '\'' || c == '’' || c == '.' {
            if c == '.' {
                pending_sep = true;
            }
        } else {
            pending_sep = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyphs(query: &str, limit: usize) -> Vec<&'static str> {
        search(
            query,
            &SearchOptions {
                limit,
                ..Default::default()
            },
        )
        .into_iter()
        .map(|m| m.emoji.glyph)
        .collect()
    }

    fn names(query: &str, limit: usize) -> Vec<&'static str> {
        search(
            query,
            &SearchOptions {
                limit,
                ..Default::default()
            },
        )
        .into_iter()
        .map(|m| m.emoji.name)
        .collect()
    }

    #[test]
    fn normalizes_queries() {
        assert_eq!(normalize("  Slightly-Smiling Face "), "slightly_smiling_face");
        assert_eq!(normalize("o’clock"), "oclock");
        assert_eq!(normalize("+1"), "+1");
        assert_eq!(normalize("__a__b__"), "a_b");
    }

    #[test]
    fn exact_shortcode_wins() {
        assert_eq!(glyphs("joy", 1), ["😂"]);
        assert_eq!(glyphs("+1", 1), ["👍️"]);
        assert_eq!(glyphs("heart", 1), ["❤️"]);
        assert_eq!(glyphs("fire", 1), ["🔥"]);
    }

    #[test]
    fn prefix_matches() {
        let results = names("thu", 5);
        assert!(results.contains(&"thumbs up"), "{results:?}");
        let results = names("slightly_sm", 3);
        assert_eq!(results[0], "slightly smiling face");
    }

    #[test]
    fn keywords_match() {
        // "lol" is a CLDR keyword for face with tears of joy.
        let results = glyphs("lol", 5);
        assert!(results.contains(&"😂"), "{results:?}");
    }

    #[test]
    fn multi_word_any_order() {
        let results = names("face smiling", 5);
        assert!(results.iter().any(|n| n.contains("smiling face")), "{results:?}");
        let results = names("cat heart", 5);
        assert!(results.contains(&"smiling cat with heart-eyes"), "{results:?}");
    }

    #[test]
    fn typos_are_tolerated() {
        let results = names("smlie", 10);
        assert!(results.iter().any(|n| n.contains("smil")), "{results:?}");
        let results = names("pizzza", 3);
        assert_eq!(results.first(), Some(&"pizza"));
    }

    #[test]
    fn exact_matches_beat_fuzzy() {
        // "cat" is exact; "car"/"bat" are distance 1.
        assert_eq!(names("cat", 1), ["cat face"]);
    }

    #[test]
    fn respects_limit_and_version() {
        assert_eq!(glyphs("s", 7).len(), 7);
        let old = search(
            "face",
            &SearchOptions {
                limit: 1000,
                max_version: 1.0,
                ..Default::default()
            },
        );
        assert!(!old.is_empty());
        assert!(old.iter().all(|m| m.emoji.version <= 1.0));
    }

    #[test]
    fn boost_reorders() {
        let opts = SearchOptions {
            limit: 3,
            ..Default::default()
        };
        let pizza = crate::by_shortcode("pizza").unwrap().id;
        let plain = search("p", &opts);
        assert_ne!(plain[0].emoji.id, pizza);
        let boosted = search_with_boost("p", &opts, |id| if id == pizza { 1000.0 } else { 0.0 });
        assert_eq!(boosted[0].emoji.id, pizza);
    }

    #[test]
    fn empty_and_garbage() {
        assert!(glyphs("", 10).is_empty());
        assert!(glyphs("::", 10).is_empty());
        assert!(glyphs("zzzzzzzzzzqqq", 10).is_empty());
    }
}
