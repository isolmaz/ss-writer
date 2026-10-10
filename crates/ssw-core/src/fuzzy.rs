//! Fuzzy matching for Quick Open, `[[` completion and `#` heading search (spec §7).
//!
//! A thin layer over `nucleo-matcher`:
//!
//! * The query is split into whitespace-separated words that are matched independently (all
//!   must match, scores add up, highlighted indices are merged), as the matcher recommends.
//! * Case is ignored and Latin letters match their unaccented form (`ç` ~ `c`). The Turkish
//!   `i`/`İ`/`ı`/`I` equivalence comes from that normalization: `İstanbul`, `ıstakoz` and
//!   `ISTANBUL` all match `istanbul`.
//! * Paths use the matcher's path bonuses (a character after `/` starts a word) and a word that
//!   matches entirely inside the file name scores [`FILE_NAME_BONUS`] more than any match
//!   spread over the directories.
//! * Callers pass the most-recently-used files, which get a small boost.
//!
//! All indices are `char` (Unicode scalar) indices into the matched string; the UI converts
//! them to offsets for highlighting.

use std::collections::HashMap;

use nucleo_matcher::{Config, Matcher, Utf32Str, chars};

/// Added to the score of a word that matches inside the file name of a path. Larger than any
/// score a single word can reach, so file-name matches always outrank directory-only matches.
const FILE_NAME_BONUS: u32 = 1 << 17;

/// Boost for the most recently used file; every further rank loses [`RECENT_STEP`].
const RECENT_BONUS: u32 = 48;
const RECENT_STEP: u32 = 2;

/// A successful match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Higher is better. `0` for an empty query.
    pub score: u32,
    /// Sorted, deduplicated `char` indices of the matched characters.
    pub indices: Vec<usize>,
}

/// One entry of a ranked result list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ranked {
    /// Position of the entry in the list that was ranked.
    pub index: usize,
    pub score: u32,
    /// Matched `char` indices for highlighting (empty for an empty query).
    pub indices: Vec<usize>,
}

/// Reusable matcher; creating one allocates about 135 KB, so keep it around.
#[derive(Debug)]
pub struct FuzzyMatcher {
    matcher: Matcher,
    haystack: Vec<char>,
    needle: Vec<char>,
    hits: Vec<u32>,
}

impl Default for FuzzyMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl FuzzyMatcher {
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            haystack: Vec::new(),
            needle: Vec::new(),
            hits: Vec::new(),
        }
    }

    /// Matches `query` against plain text (a heading, a title). An empty query matches
    /// everything with score 0.
    pub fn match_text(&mut self, query: &str, text: &str) -> Option<FuzzyMatch> {
        self.match_words(query, text, false)
    }

    /// Matches `query` against a `/`-separated path with the file-name boost.
    pub fn match_path(&mut self, query: &str, path: &str) -> Option<FuzzyMatch> {
        self.match_words(query, path, true)
    }

    /// Ranks `texts` for `query`, best first (ties keep their input order), at most `limit`
    /// entries. An empty query returns the first `limit` texts in input order.
    pub fn rank_texts<S: AsRef<str>>(
        &mut self,
        query: &str,
        texts: &[S],
        limit: usize,
    ) -> Vec<Ranked> {
        self.rank(query, texts, &[], limit, false)
    }

    /// Ranks `paths` for `query`, best first, at most `limit` entries. `recent` lists indices
    /// into `paths`, most recently used first (out-of-range and repeated indices are ignored);
    /// those files get a boost that shrinks with their position. Equal scores prefer shorter
    /// paths, then input order. An empty query lists the recent files in recency order
    /// followed by the remaining paths in input order.
    pub fn rank_paths<S: AsRef<str>>(
        &mut self,
        query: &str,
        paths: &[S],
        recent: &[usize],
        limit: usize,
    ) -> Vec<Ranked> {
        self.rank(query, paths, recent, limit, true)
    }

    fn rank<S: AsRef<str>>(
        &mut self,
        query: &str,
        items: &[S],
        recent: &[usize],
        limit: usize,
        path: bool,
    ) -> Vec<Ranked> {
        let mut recency: HashMap<usize, usize> = HashMap::new();
        for &index in recent {
            if index < items.len() {
                let rank = recency.len();
                recency.entry(index).or_insert(rank);
            }
        }
        if query.split_whitespace().next().is_none() {
            let mut recents: Vec<(usize, usize)> = recency
                .iter()
                .map(|(&index, &rank)| (rank, index))
                .collect();
            recents.sort_unstable();
            let rest = (0..items.len()).filter(|index| !recency.contains_key(index));
            return recents
                .into_iter()
                .map(|(_, index)| index)
                .chain(rest)
                .take(limit)
                .map(|index| Ranked {
                    index,
                    score: 0,
                    indices: Vec::new(),
                })
                .collect();
        }
        let mut ranked = Vec::new();
        for (index, item) in items.iter().enumerate() {
            let Some(found) = self.match_words(query, item.as_ref(), path) else {
                continue;
            };
            let boost = recency.get(&index).map_or(0, |&rank| {
                let lost = RECENT_STEP.saturating_mul(u32::try_from(rank).unwrap_or(u32::MAX));
                RECENT_BONUS.saturating_sub(lost)
            });
            ranked.push(Ranked {
                index,
                score: found.score.saturating_add(boost),
                indices: found.indices,
            });
        }
        ranked.sort_by(|a, b| {
            b.score.cmp(&a.score).then_with(|| {
                items[a.index]
                    .as_ref()
                    .len()
                    .cmp(&items[b.index].as_ref().len())
            })
        });
        ranked.truncate(limit);
        ranked
    }

    fn match_words(&mut self, query: &str, text: &str, path: bool) -> Option<FuzzyMatch> {
        let Self {
            matcher,
            haystack,
            needle,
            hits,
        } = self;
        matcher.config = if path {
            Config::DEFAULT.match_paths()
        } else {
            Config::DEFAULT
        };
        haystack.clear();
        haystack.extend(text.chars());
        let name_start = path.then(|| {
            haystack
                .iter()
                .rposition(|&c| c == '/')
                .map_or(0, |slash| slash + 1)
        });
        let mut score = 0u32;
        let mut indices: Vec<usize> = Vec::new();
        for word in query.split_whitespace() {
            needle.clear();
            needle.extend(
                word.chars()
                    .map(|c| chars::to_lower_case(chars::normalize(c))),
            );
            let word = Utf32Str::Unicode(needle.as_slice());
            hits.clear();
            let in_name = name_start.and_then(|start| {
                matcher
                    .fuzzy_indices(Utf32Str::Unicode(&haystack[start..]), word, hits)
                    .map(|s| (u32::from(s) + FILE_NAME_BONUS, start))
            });
            let (word_score, offset) = match in_name {
                Some(found) => found,
                None => {
                    hits.clear();
                    let all = Utf32Str::Unicode(haystack.as_slice());
                    let s = matcher.fuzzy_indices(all, word, hits)?;
                    (u32::from(s), 0)
                }
            };
            score = score.saturating_add(word_score);
            indices.extend(hits.iter().map(|&i| offset + i as usize));
        }
        indices.sort_unstable();
        indices.dedup();
        Some(FuzzyMatch { score, indices })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_indices(query: &str, text: &str) -> Option<Vec<usize>> {
        FuzzyMatcher::new()
            .match_text(query, text)
            .map(|m| m.indices)
    }

    fn order(ranked: &[Ranked]) -> Vec<usize> {
        ranked.iter().map(|r| r.index).collect()
    }

    #[test]
    fn subsequence_matches_report_char_indices() {
        assert_eq!(text_indices("fb", "foo bar"), Some(vec![0, 4]));
        assert_eq!(text_indices("bar", "foo bar"), Some(vec![4, 5, 6]));
        assert_eq!(text_indices("xyz", "foo bar"), None);
        assert_eq!(text_indices("foobarbaz", "foo bar"), None);
        // Char indices, not byte offsets: `日本` is two chars (six bytes).
        assert_eq!(text_indices("foo", "日本 foo"), Some(vec![3, 4, 5]));
        assert_eq!(text_indices("本", "日本 foo"), Some(vec![1]));
    }

    #[test]
    fn empty_and_blank_queries_match_everything_with_zero_score() {
        let mut m = FuzzyMatcher::default();
        for query in ["", "   ", "\t"] {
            let found = m.match_text(query, "anything").unwrap();
            assert_eq!(
                found,
                FuzzyMatch {
                    score: 0,
                    indices: Vec::new()
                }
            );
        }
    }

    #[test]
    fn case_and_accents_are_ignored() {
        assert!(text_indices("FOO", "foo").is_some());
        assert!(text_indices("foo", "FOO").is_some());
        assert!(text_indices("cafe", "Café").is_some());
        assert_eq!(text_indices("çay", "Çay"), Some(vec![0, 1, 2]));
        assert!(text_indices("日本", "日本語").is_some());
    }

    #[test]
    fn turkish_i_variants_are_equivalent() {
        let names = [
            "İstanbul.md",
            "ıstanbul.md",
            "ISTANBUL.md",
            "istanbul.md",
            "Istanbul.md",
        ];
        let mut m = FuzzyMatcher::new();
        for query in ["istanbul", "İSTANBUL", "ıstanbul", "Istanbul", "ISTANBUL"] {
            for name in names {
                assert!(
                    m.match_path(query, name).is_some(),
                    "{query:?} should match {name:?}"
                );
            }
        }
        let found = m.match_path("ISTAKOZ", "ıstakoz.txt").unwrap();
        assert_eq!(found.indices, [0, 1, 2, 3, 4, 5, 6]);
        // Unrelated letters still do not match.
        assert!(m.match_path("ist", "ıstanbul.md").is_some());
        assert!(m.match_path("xyz", "ıstanbul.md").is_none());
    }

    #[test]
    fn multiple_words_all_have_to_match() {
        let mut m = FuzzyMatcher::new();
        let found = m.match_path("doc read", "docs/readme.md").unwrap();
        assert_eq!(found.indices, [0, 1, 2, 5, 6, 7, 8]);
        assert!(m.match_path("doc license", "docs/readme.md").is_none());
        // Words may overlap; indices are merged without duplicates.
        let found = m.match_text("ab ab", "ab").unwrap();
        assert_eq!(found.indices, [0, 1]);
    }

    #[test]
    fn file_name_matches_outrank_directory_matches() {
        let mut m = FuzzyMatcher::new();
        let in_name = m.match_path("main", "src/main.rs").unwrap();
        let in_dir = m.match_path("main", "main/lib/util.rs").unwrap();
        assert!(in_name.score >= FILE_NAME_BONUS);
        assert!(in_dir.score < FILE_NAME_BONUS);
        assert_eq!(in_name.indices, [4, 5, 6, 7]);
        assert_eq!(in_dir.indices, [0, 1, 2, 3]);
        // A path without a directory counts as a file name too.
        assert!(m.match_path("main", "main.rs").unwrap().score >= FILE_NAME_BONUS);
        // A word split between directory and name falls back to the whole path.
        let split = m.match_path("sm", "src/main.rs").unwrap();
        assert_eq!(split.indices, [0, 4]);
        assert!(split.score < FILE_NAME_BONUS);
        // Plain text never gets the boost.
        assert!(m.match_text("main", "src/main.rs").unwrap().score < FILE_NAME_BONUS);
    }

    #[test]
    fn rank_paths_orders_by_score_then_length() {
        let paths = [
            "a/deep/dir/main.rs",
            "docs/maintenance.md",
            "src/main.rs",
            "readme.md",
        ];
        let ranked = FuzzyMatcher::new().rank_paths("main", &paths, &[], 10);
        assert_eq!(order(&ranked), [2, 0, 1]);
        assert!(ranked.windows(2).all(|w| w[0].score >= w[1].score));
        assert_eq!(ranked[0].indices, [4, 5, 6, 7]);
        // Shorter path first when scores tie.
        let tie = FuzzyMatcher::new().rank_paths("md", &["notes/ab.md", "ab.md"], &[], 10);
        assert_eq!(order(&tie), [1, 0]);
        // Fully equal entries keep their input order.
        let same = FuzzyMatcher::new().rank_paths("a", &["x/a", "y/a"], &[], 10);
        assert_eq!(order(&same), [0, 1]);
    }

    #[test]
    fn recent_files_get_a_boost_that_shrinks_with_rank() {
        let paths = ["notes/alpha.md", "notes/alpine.md", "notes/alps.md"];
        let mut m = FuzzyMatcher::new();
        let plain = m.rank_paths("alp", &paths, &[], 10);
        assert_eq!(order(&plain), [2, 0, 1]);
        let boosted = m.rank_paths("alp", &paths, &[1], 10);
        assert_eq!(boosted[0].index, 1);
        assert_eq!(boosted[0].score, plain[2].score + RECENT_BONUS);
        // Recency rank 1 gets less than rank 0; repeated and unknown indices are ignored.
        let ranked = m.rank_paths("alp", &paths, &[7, 1, 1, 0], 10);
        let score_of = |index| ranked.iter().find(|r| r.index == index).unwrap().score;
        let base_of = |index| plain.iter().find(|r| r.index == index).unwrap().score;
        assert_eq!(score_of(1), base_of(1) + RECENT_BONUS);
        assert_eq!(score_of(0), base_of(0) + RECENT_BONUS - RECENT_STEP);
        assert_eq!(score_of(2), base_of(2));
    }

    #[test]
    fn the_recent_boost_runs_out_after_enough_ranks() {
        let mut paths: Vec<String> = (0..30).map(|i| format!("z{i}.md")).collect();
        paths.push("alpha.md".to_owned());
        let mut m = FuzzyMatcher::new();
        let plain = m.rank_paths("alp", &paths, &[], 10);
        assert_eq!(order(&plain), [30]);
        // Rank 24 and beyond: 48 - 2 * 24 leaves nothing.
        let recent: Vec<usize> = (0..30).chain([30]).collect();
        let ranked = m.rank_paths("alp", &paths, &recent, 10);
        assert_eq!(ranked[0].score, plain[0].score);
        let at_23: Vec<usize> = (0..23).chain([30]).collect();
        let ranked = m.rank_paths("alp", &paths, &at_23, 10);
        assert_eq!(
            ranked[0].score,
            plain[0].score + RECENT_BONUS - 23 * RECENT_STEP
        );
    }

    #[test]
    fn empty_query_lists_recent_files_first() {
        let paths = ["a.md", "b.md", "c.md", "d.md"];
        let mut m = FuzzyMatcher::new();
        let ranked = m.rank_paths("", &paths, &[2, 9, 0, 2], 10);
        assert_eq!(order(&ranked), [2, 0, 1, 3]);
        assert!(ranked.iter().all(|r| r.score == 0 && r.indices.is_empty()));
        assert_eq!(order(&m.rank_paths("  ", &paths, &[], 2)), [0, 1]);
        assert!(m.rank_paths("", &paths, &[], 0).is_empty());
    }

    #[test]
    fn limit_and_missing_matches() {
        let paths = ["aa.md", "ab.md", "ac.md", "zz.md"];
        let mut m = FuzzyMatcher::new();
        assert_eq!(m.rank_paths("a", &paths, &[], 2).len(), 2);
        assert_eq!(m.rank_paths("a", &paths, &[], 10).len(), 3);
        assert!(m.rank_paths("q", &paths, &[], 10).is_empty());
        assert!(m.rank_paths("a", &paths, &[], 0).is_empty());
        let none: [&str; 0] = [];
        assert!(m.rank_paths("a", &none, &[], 5).is_empty());
    }

    #[test]
    fn rank_texts_for_headings() {
        let headings = [
            "Introduction",
            "Installation guide",
            "İçindekiler",
            "Appendix",
        ];
        let mut m = FuzzyMatcher::new();
        let ranked = m.rank_texts("inst", &headings, 10);
        assert_eq!(order(&ranked), [1]);
        assert_eq!(ranked[0].indices, [0, 1, 2, 3]);
        let turkish = m.rank_texts("icindekiler", &headings, 10);
        assert_eq!(order(&turkish), [2]);
        let all = m.rank_texts("", &headings, 3);
        assert_eq!(order(&all), [0, 1, 2]);
        let owned: Vec<String> = headings.iter().map(|h| (*h).to_owned()).collect();
        assert_eq!(order(&m.rank_texts("app", &owned, 5)), [3]);
    }
}
