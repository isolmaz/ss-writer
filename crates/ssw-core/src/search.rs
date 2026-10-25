//! Find and replace (spec §7).
//!
//! A [`Query`] is compiled into a [`Searcher`]; the free functions ([`find_all`],
//! [`find_next`], [`find_prev`], [`replace_one`], [`replace_all`]) compile on every call and are
//! meant for one-shot use. All offsets are UTF-8 byte offsets into the searched text.
//!
//! * **Literal** search with *Match case* off compares characters through [`fold_char`]:
//!   Unicode simple case folding plus the Turkish rule that `i`, `I`, `İ` and `ı` are all the
//!   same letter. Folding maps one `char` to one `char`, so matches always start and end on
//!   character boundaries.
//! * **Whole word** in literal mode requires the match to start and end on a UAX #29 word
//!   boundary (`日本語` is three words, `foo_bar` is one). In regex mode the pattern is wrapped
//!   in `\b(?: … )\b` (the regex engine's Unicode word boundary).
//! * **Regex** mode uses the `regex` crate with `^`/`$` matching at every line. Case-insensitive
//!   regex matching follows the engine's Unicode case folding; the Turkish equivalence is a
//!   literal-mode feature only.
//! * Matches never overlap and are reported left to right. Empty regex matches are reported
//!   (`^` replaced by `> ` quotes every line).
//! * Replacements: literal mode inserts the replacement verbatim (`$` is an ordinary
//!   character). Regex mode expands `$1`, `${1}` and `$$`; see [`Searcher::replace_all`].

use regex::{Captures, Regex, RegexBuilder};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

use crate::edit::Edit;
use crate::text::TextRange;

/// What the user typed in the find bar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub pattern: String,
    pub match_case: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// A query cannot be compiled.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SearchError {
    /// The regular expression is invalid; the payload is the engine's message.
    #[error("invalid regular expression: {0}")]
    InvalidRegex(String),
}

/// Unicode simple uppercase mapping: the single uppercase `char` of `c`, or `c` itself when
/// the mapping is absent or expands to several characters (`ß`).
pub fn simple_upper(c: char) -> char {
    single_or(c, c.to_uppercase())
}

/// Unicode simple lowercase mapping: the single lowercase `char` of `c`, or `c` itself when the
/// mapping is absent or expands to several characters (`İ` has no simple lowercase here).
pub fn simple_lower(c: char) -> char {
    single_or(c, c.to_lowercase())
}

fn single_or(c: char, mut mapped: impl Iterator<Item = char>) -> char {
    match (mapped.next(), mapped.next()) {
        (Some(only), None) => only,
        _ => c,
    }
}

/// Canonical form used for case-insensitive literal matching: simple case folding
/// (`lower(upper(c))`, which also unifies `ς`/`σ` and the Kelvin sign with `k`) and the Turkish
/// dotted/dotless `i` equivalence: `i`, `I`, `İ` and `ı` all fold to `i`.
pub fn fold_char(c: char) -> char {
    match c {
        'i' | 'I' | '\u{130}' | '\u{131}' => 'i',
        _ => simple_lower(simple_upper(c)),
    }
}

/// A compiled [`Query`].
#[derive(Debug)]
pub struct Searcher {
    engine: Engine,
}

#[derive(Debug)]
enum Engine {
    /// Empty pattern: nothing matches.
    Empty,
    Literal(Literal),
    Regex(Regex),
}

/// A literal needle and whether matches must be whole words.
#[derive(Debug)]
struct Literal {
    needle: Needle,
    whole_word: bool,
}

#[derive(Debug)]
enum Needle {
    /// Case-sensitive.
    Exact(String),
    /// Case-insensitive: the folded characters (`first` + `rest`; never empty).
    Folded { first: char, rest: Vec<char> },
}

impl Searcher {
    /// Compiles `query`. An empty pattern is valid and matches nothing.
    pub fn new(query: &Query) -> Result<Self, SearchError> {
        let pattern = query.pattern.as_str();
        let mut folded = pattern.chars().map(fold_char);
        let engine = match folded.next() {
            None => Engine::Empty,
            Some(_) if query.regex => {
                Engine::Regex(build_regex(pattern, query.match_case, query.whole_word)?)
            }
            Some(_) if query.match_case => Engine::Literal(Literal {
                needle: Needle::Exact(pattern.to_owned()),
                whole_word: query.whole_word,
            }),
            Some(first) => Engine::Literal(Literal {
                needle: Needle::Folded {
                    first,
                    rest: folded.collect(),
                },
                whole_word: query.whole_word,
            }),
        };
        Ok(Self { engine })
    }

    /// Every match, left to right and non-overlapping.
    pub fn find_all(&self, text: &str) -> Vec<TextRange> {
        match &self.engine {
            Engine::Empty => Vec::new(),
            Engine::Regex(re) => re
                .find_iter(text)
                .map(|m| TextRange::new(m.start(), m.end()))
                .collect(),
            Engine::Literal(literal) => literal.find_all(text),
        }
    }

    /// First match starting at or after `from` (rounded up to a char boundary of `text`),
    /// wrapping to the first match of the text. The search starts at `from`, so a match that
    /// begins there is found even when the matches reported by [`Searcher::find_all`] overlap
    /// it. `None` when there is no match at all.
    pub fn next(&self, text: &str, from: usize) -> Option<TextRange> {
        let from = text.ceil_char_boundary(from);
        match &self.engine {
            Engine::Empty => None,
            Engine::Regex(re) => re
                .find_at(text, from)
                .or_else(|| re.find_at(text, 0))
                .map(match_range),
            Engine::Literal(literal) => {
                let bounds = literal.bounds(text);
                literal
                    .find_from(text, from, &bounds)
                    .or_else(|| literal.find_from(text, 0, &bounds))
            }
        }
    }

    /// Last match starting before `from`, wrapping to the last match of the text.
    /// `None` when there is no match at all.
    pub fn prev(&self, text: &str, from: usize) -> Option<TextRange> {
        match &self.engine {
            Engine::Empty => None,
            Engine::Regex(re) => last_before(re.find_iter(text).map(match_range), from),
            Engine::Literal(literal) => last_before(literal.matches(text), from),
        }
    }

    /// The edit that replaces the match found by [`Searcher::next`] from `from`.
    pub fn replace_one(&self, text: &str, replacement: &str, from: usize) -> Option<Edit> {
        match &self.engine {
            Engine::Regex(re) => re
                .captures_at(text, text.ceil_char_boundary(from))
                .or_else(|| re.captures_at(text, 0))
                .map(|caps| regex_edit(&caps, replacement)),
            Engine::Empty | Engine::Literal(_) => self
                .next(text, from)
                .map(|range| Edit::replace(range, replacement)),
        }
    }

    /// One edit per match, sorted and disjoint, all against `text` (apply them as one
    /// transaction). In regex mode `replacement` is expanded per match:
    ///
    /// * `$0` is the whole match, `$1`… the capture groups; digits are taken greedily while
    ///   the number names an existing group (`$10` is group 1 followed by `0` in a pattern with
    ///   fewer than eleven groups);
    /// * `${1}` is the explicit form (`${1}0`);
    /// * `$$` is a literal `$`;
    /// * a group that did not participate expands to nothing; a `$` that starts none of the
    ///   forms above, or names a group that does not exist, stays literal.
    pub fn replace_all(&self, text: &str, replacement: &str) -> Vec<Edit> {
        match &self.engine {
            Engine::Regex(re) => re
                .captures_iter(text)
                .map(|caps| regex_edit(&caps, replacement))
                .collect(),
            Engine::Empty | Engine::Literal(_) => self
                .find_all(text)
                .into_iter()
                .map(|range| Edit::replace(range, replacement))
                .collect(),
        }
    }
}

impl Literal {
    fn find_all(&self, text: &str) -> Vec<TextRange> {
        self.matches(text).collect()
    }

    /// The matches in `text`, left to right, found on demand.
    fn matches<'a>(&'a self, text: &'a str) -> LiteralMatches<'a> {
        LiteralMatches {
            literal: self,
            text,
            bounds: self.bounds(text),
            pos: 0,
        }
    }

    /// The word boundaries of `text` when matches must be whole words, otherwise none.
    fn bounds(&self, text: &str) -> Vec<usize> {
        if self.whole_word {
            word_boundaries(text)
        } else {
            Vec::new()
        }
    }

    /// First match starting at or after `from` (a char boundary of `text`). `bounds` are the
    /// word boundaries of `text`, only consulted when `whole_word` is set.
    fn find_from(&self, text: &str, mut from: usize, bounds: &[usize]) -> Option<TextRange> {
        loop {
            let found = match &self.needle {
                Needle::Exact(needle) => text[from..]
                    .find(needle.as_str())
                    .map(|i| TextRange::new(from + i, from + i + needle.len())),
                Needle::Folded { first, rest } => find_folded(text, from, *first, rest),
            }?;
            let on_boundary = |p: usize| bounds.binary_search(&p).is_ok();
            if !self.whole_word || (on_boundary(found.start) && on_boundary(found.end)) {
                return Some(found);
            }
            from = found.start + text[found.start..].chars().next().map_or(1, char::len_utf8);
        }
    }
}

/// Iterator over the matches of a [`Literal`].
struct LiteralMatches<'a> {
    literal: &'a Literal,
    text: &'a str,
    bounds: Vec<usize>,
    pos: usize,
}

impl Iterator for LiteralMatches<'_> {
    type Item = TextRange;

    fn next(&mut self) -> Option<TextRange> {
        let found = self.literal.find_from(self.text, self.pos, &self.bounds)?;
        self.pos = found.end;
        Some(found)
    }
}

fn match_range(m: regex::Match<'_>) -> TextRange {
    TextRange::new(m.start(), m.end())
}

/// The last of `matches` (in ascending order) that starts before `from`; when none does, the
/// last one overall. Stops reading as soon as a match at or after `from` follows one before it.
fn last_before(matches: impl Iterator<Item = TextRange>, from: usize) -> Option<TextRange> {
    let (mut before, mut last) = (None, None);
    for m in matches {
        if m.start < from {
            before = Some(m);
        } else if before.is_some() {
            break;
        } else {
            last = Some(m);
        }
    }
    before.or(last)
}

/// Compiles the user's pattern; with `whole_word` additionally the `\b(?: … )\b` wrapper (the
/// plain pattern is compiled first so that a wrapper can never repair an unbalanced pattern).
fn build_regex(pattern: &str, match_case: bool, whole_word: bool) -> Result<Regex, SearchError> {
    let compile = |p: &str| {
        RegexBuilder::new(p)
            .case_insensitive(!match_case)
            .multi_line(true)
            .build()
            .map_err(|e| SearchError::InvalidRegex(e.to_string()))
    };
    let plain = compile(pattern)?;
    if whole_word {
        compile(&format!(r"\b(?:{pattern})\b"))
    } else {
        Ok(plain)
    }
}

/// First occurrence of the folded needle `first` + `rest` at or after `from` (a char boundary).
fn find_folded(text: &str, from: usize, first: char, rest: &[char]) -> Option<TextRange> {
    for (i, c) in text[from..].char_indices() {
        if fold_char(c) != first {
            continue;
        }
        let start = from + i;
        let mut end = start + c.len_utf8();
        let mut tail = text[end..].chars();
        let all_equal = rest.iter().all(|&n| match tail.next() {
            Some(h) if fold_char(h) == n => {
                end += h.len_utf8();
                true
            }
            _ => false,
        });
        if all_equal {
            return Some(TextRange::new(start, end));
        }
    }
    None
}

/// Start offsets of every UAX #29 word-boundary segment plus the end of the text.
fn word_boundaries(text: &str) -> Vec<usize> {
    let mut bounds: Vec<usize> = text.split_word_bound_indices().map(|(i, _)| i).collect();
    bounds.push(text.len());
    bounds
}

fn regex_edit(caps: &Captures<'_>, template: &str) -> Edit {
    let whole = caps.get_match();
    let mut text = String::new();
    expand(caps, template, &mut text);
    Edit::replace(TextRange::new(whole.start(), whole.end()), text)
}

/// Expands `template` for one match (rules in [`Searcher::replace_all`]).
fn expand(caps: &Captures<'_>, template: &str, out: &mut String) {
    let groups = caps.len();
    let mut rest = template;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        let (group, used) = parse_reference(after, groups);
        match group {
            Some(n) => out.push_str(caps.get(n).map_or("", |m| m.as_str())),
            None => out.push('$'),
        }
        rest = &after[used..];
    }
    out.push_str(rest);
}

/// Parses what follows a `$`: the group it names (`None`: a literal `$`) and the number of
/// bytes it consumed after the `$`.
fn parse_reference(after: &str, groups: usize) -> (Option<usize>, usize) {
    if after.starts_with('$') {
        return (None, 1);
    }
    if let Some(braced) = after.strip_prefix('{')
        && let Some(close) = braced.find('}')
    {
        let digits = &braced[..close];
        if digits.bytes().all(|b| b.is_ascii_digit())
            && let Ok(n) = digits.parse::<usize>()
            && n < groups
        {
            return (Some(n), close + 2);
        }
        return (None, 0);
    }
    let mut digits = after.bytes().take_while(u8::is_ascii_digit);
    let Some(first) = digits.next() else {
        return (None, 0);
    };
    let mut n = usize::from(first - b'0');
    if n >= groups {
        return (None, 0);
    }
    let mut used = 1;
    for d in digits {
        let next = n * 10 + usize::from(d - b'0');
        if next >= groups {
            break;
        }
        n = next;
        used += 1;
    }
    (Some(n), used)
}

/// Every match of `query` in `text`.
pub fn find_all(text: &str, query: &Query) -> Result<Vec<TextRange>, SearchError> {
    Ok(Searcher::new(query)?.find_all(text))
}

/// Match after `from` with wrap-around; see [`Searcher::next`].
pub fn find_next(text: &str, query: &Query, from: usize) -> Result<Option<TextRange>, SearchError> {
    Ok(Searcher::new(query)?.next(text, from))
}

/// Match before `from` with wrap-around; see [`Searcher::prev`].
pub fn find_prev(text: &str, query: &Query, from: usize) -> Result<Option<TextRange>, SearchError> {
    Ok(Searcher::new(query)?.prev(text, from))
}

/// Replaces the next match from `from`; see [`Searcher::replace_one`].
pub fn replace_one(
    text: &str,
    query: &Query,
    replacement: &str,
    from: usize,
) -> Result<Option<Edit>, SearchError> {
    Ok(Searcher::new(query)?.replace_one(text, replacement, from))
}

/// Replaces every match; see [`Searcher::replace_all`].
pub fn replace_all(text: &str, query: &Query, replacement: &str) -> Result<Vec<Edit>, SearchError> {
    Ok(Searcher::new(query)?.replace_all(text, replacement))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(pattern: &str) -> Query {
        Query {
            pattern: pattern.to_owned(),
            ..Query::default()
        }
    }

    fn with(pattern: &str, match_case: bool, whole_word: bool, regex: bool) -> Query {
        Query {
            pattern: pattern.to_owned(),
            match_case,
            whole_word,
            regex,
        }
    }

    fn found<'a>(text: &'a str, q: &Query) -> Vec<&'a str> {
        find_all(text, q)
            .unwrap()
            .into_iter()
            .map(|r| &text[r.start..r.end])
            .collect()
    }

    fn ranges(text: &str, q: &Query) -> Vec<(usize, usize)> {
        find_all(text, q)
            .unwrap()
            .into_iter()
            .map(|r| (r.start, r.end))
            .collect()
    }

    fn apply(text: &str, edits: &[Edit]) -> String {
        let mut out = String::new();
        let mut pos = 0;
        for e in edits {
            out.push_str(&text[pos..e.range.start]);
            out.push_str(&e.text);
            pos = e.range.end;
        }
        out.push_str(&text[pos..]);
        out
    }

    #[test]
    fn single_char_mappings() {
        assert_eq!(simple_upper('a'), 'A');
        assert_eq!(simple_upper('ß'), 'ß');
        assert_eq!(simple_upper('ς'), 'Σ');
        assert_eq!(simple_lower('Σ'), 'σ');
        assert_eq!(simple_lower('\u{130}'), '\u{130}');
        assert_eq!(simple_lower('Ä'), 'ä');
        assert_eq!(simple_lower('日'), '日');
    }

    #[test]
    fn fold_unifies_the_four_turkish_i_and_simple_case() {
        for c in ['i', 'I', '\u{130}', '\u{131}'] {
            assert_eq!(fold_char(c), 'i');
        }
        assert_eq!(fold_char('Ş'), 'ş');
        assert_eq!(fold_char('ς'), fold_char('Σ'));
        assert_eq!(fold_char('\u{212A}'), 'k');
        assert_eq!(fold_char('ß'), 'ß');
        assert_eq!(fold_char('日'), '日');
    }

    #[test]
    fn empty_pattern_matches_nothing_in_every_mode() {
        for regex in [false, true] {
            for match_case in [false, true] {
                let q = with("", match_case, false, regex);
                assert!(find_all("abc", &q).unwrap().is_empty());
                assert_eq!(find_next("abc", &q, 0).unwrap(), None);
                assert_eq!(find_prev("abc", &q, 3).unwrap(), None);
                assert_eq!(replace_one("abc", &q, "x", 0).unwrap(), None);
                assert!(replace_all("abc", &q, "x").unwrap().is_empty());
            }
        }
    }

    #[test]
    fn literal_case_sensitivity() {
        let text = "Foo foo FOO";
        assert_eq!(found(text, &query("foo")), ["Foo", "foo", "FOO"]);
        assert_eq!(found(text, &with("foo", true, false, false)), ["foo"]);
        assert_eq!(found(text, &with("Foo", true, false, false)), ["Foo"]);
    }

    #[test]
    fn literal_matches_do_not_overlap() {
        assert_eq!(
            ranges("aaaaa", &with("aa", true, false, false)),
            [(0, 2), (2, 4)]
        );
        assert_eq!(ranges("aaaaa", &query("AA")), [(0, 2), (2, 4)]);
    }

    #[test]
    fn turkish_i_variants_are_equivalent_when_case_is_ignored() {
        let text = "İstanbul ıstanbul ISTANBUL istanbul Istanbul";
        assert_eq!(found(text, &query("istanbul")).len(), 5);
        assert_eq!(found(text, &query("İSTANBUL")).len(), 5);
        assert_eq!(found(text, &query("ıstanbul")).len(), 5);
        // `İ` is two bytes, `ı` is two bytes.
        let r = ranges(text, &query("istanbul"));
        assert_eq!(r[0], (0, 9));
        assert_eq!(r[1], (10, 19));
        assert_eq!(r[2], (20, 28));
        // Match case keeps them apart.
        assert_eq!(
            found(text, &with("istanbul", true, false, false)),
            ["istanbul"]
        );
        assert_eq!(
            found(text, &with("İstanbul", true, false, false)),
            ["İstanbul"]
        );
        assert_eq!(
            found("ıstakoz ISTAKOZ istakoz", &query("ISTAKOZ")),
            ["ıstakoz", "ISTAKOZ", "istakoz"]
        );
    }

    #[test]
    fn unicode_simple_case_folding() {
        assert_eq!(
            found("ΟΔΟΣ οδος οδοσ", &query("οδοσ")),
            ["ΟΔΟΣ", "οδος", "οδοσ"]
        );
        assert_eq!(found("Kelvin \u{212A}elvin", &query("kelvin")).len(), 2);
        assert_eq!(found("ÉCOLE école", &query("école")), ["ÉCOLE", "école"]);
        // `ß` has no simple mapping: it does not match `SS`.
        assert!(found("Straße", &query("STRASSE")).is_empty());
    }

    #[test]
    fn multibyte_offsets_are_byte_offsets() {
        let text = "日本語 café 😀 café";
        let q = with("café", true, false, false);
        assert_eq!(ranges(text, &q), [(10, 15), (21, 26)]);
        assert_eq!(ranges(text, &query("😀")), [(16, 20)]);
        assert_eq!(ranges(text, &query("本")), [(3, 6)]);
        assert_eq!(&text[10..15], "café");
    }

    #[test]
    fn pattern_longer_than_text_or_cut_short_does_not_match() {
        assert!(found("ab", &query("abc")).is_empty());
        assert!(found("ab", &with("abc", true, false, false)).is_empty());
        assert!(found("xab", &query("abc")).is_empty());
    }

    #[test]
    fn whole_word_uses_unicode_word_boundaries() {
        let text = "foo foobar foo_bar foo-bar Foo";
        assert_eq!(
            ranges(text, &with("foo", false, true, false)),
            [(0, 3), (19, 22), (27, 30)]
        );
        assert_eq!(
            ranges(text, &with("foo", true, true, false)),
            [(0, 3), (19, 22)]
        );
        // Every ideograph is its own word; non-word characters are boundaries.
        assert_eq!(
            ranges("你好世界", &with("你好", true, true, false)),
            [(0, 6)]
        );
        assert_eq!(
            ranges("你好世界", &with("好世", false, true, false)),
            [(3, 9)]
        );
        assert!(ranges("a b", &with("c", false, true, false)).is_empty());
        assert_eq!(ranges("(foo)", &with("foo", false, true, false)), [(1, 4)]);
        // `é` is part of the word: no match inside `café`.
        assert!(found("café", &with("caf", false, true, false)).is_empty());
        // The start of the candidate fails, a later one succeeds.
        assert_eq!(
            ranges("xfoo foo", &with("foo", true, true, false)),
            [(5, 8)]
        );
        // Candidate start inside a multi-byte word.
        assert_eq!(
            ranges("éfoo foo", &with("foo", false, true, false)),
            [(6, 9)]
        );
    }

    #[test]
    fn whole_word_turkish_and_empty_text() {
        let text = "Işık ışık";
        assert_eq!(
            found(text, &with("IŞIK", false, true, false)),
            ["Işık", "ışık"]
        );
        // `ş` is a different letter from `s`.
        assert!(found(text, &with("ISIK", false, true, false)).is_empty());
        assert!(found("", &with("a", false, true, false)).is_empty());
    }

    #[test]
    fn regex_matching_and_flags() {
        let text = "a1 b22 C333";
        assert_eq!(
            found(text, &with(r"\d+", false, false, true)),
            ["1", "22", "333"]
        );
        assert_eq!(found(text, &with("c", false, false, true)), ["C"]);
        assert!(found(text, &with("c", true, false, true)).is_empty());
        assert_eq!(
            found("日本語 日本", &with("日.", true, false, true)),
            ["日本", "日本"]
        );
        assert_eq!(
            found("foo foobar", &with("foo", false, true, true)),
            ["foo"]
        );
        assert_eq!(
            found("foo foobar bar", &with("foo|bar", false, true, true)),
            ["foo", "bar"]
        );
    }

    #[test]
    fn regex_anchors_work_per_line() {
        let text = "a\nb\n";
        assert_eq!(
            ranges(text, &with("^", true, false, true)),
            [(0, 0), (2, 2), (4, 4)]
        );
        assert_eq!(
            ranges(text, &with("$", true, false, true)),
            [(1, 1), (3, 3), (4, 4)]
        );
    }

    #[test]
    fn invalid_regex_is_an_error_everywhere() {
        let q = with("(", false, false, true);
        let err = find_all("x", &q).unwrap_err();
        let SearchError::InvalidRegex(message) = &err;
        assert!(message.ends_with("unclosed group"), "{message}");
        assert!(err.to_string().starts_with("invalid regular expression:"));
        assert!(find_next("x", &q, 0).is_err());
        assert!(find_prev("x", &q, 0).is_err());
        assert!(replace_one("x", &q, "", 0).is_err());
        assert!(replace_all("x", &q, "").is_err());
        // The unbalanced pattern is reported even though the whole-word wrapper would parse.
        let wrapped = with("a)|(b", false, true, true);
        assert!(find_all("x", &wrapped).is_err());
        assert!(find_all("x", &with(r"\", true, false, true)).is_err());
    }

    #[test]
    fn next_and_prev_wrap_around() {
        let text = "ab ab ab";
        let q = with("ab", true, false, false);
        let next = |from| find_next(text, &q, from).unwrap();
        let prev = |from| find_prev(text, &q, from).unwrap();
        assert_eq!(next(0), Some(TextRange::new(0, 2)));
        assert_eq!(next(1), Some(TextRange::new(3, 5)));
        assert_eq!(next(3), Some(TextRange::new(3, 5)));
        assert_eq!(next(6), Some(TextRange::new(6, 8)));
        assert_eq!(next(7), Some(TextRange::new(0, 2)));
        assert_eq!(prev(8), Some(TextRange::new(6, 8)));
        assert_eq!(prev(6), Some(TextRange::new(3, 5)));
        assert_eq!(prev(4), Some(TextRange::new(3, 5)));
        assert_eq!(prev(3), Some(TextRange::new(0, 2)));
        assert_eq!(prev(0), Some(TextRange::new(6, 8)));
        assert_eq!(find_next("xyz", &q, 1).unwrap(), None);
        assert_eq!(find_prev("xyz", &q, 1).unwrap(), None);
    }

    #[test]
    fn next_and_prev_treat_a_match_at_the_offset_as_after_and_not_before() {
        let text = "a1 b a2 a3";
        let regex = with(r"a\d", true, false, true);
        let next = |from| find_next(text, &regex, from).unwrap();
        let prev = |from| find_prev(text, &regex, from).unwrap();
        assert_eq!(next(0), Some(TextRange::new(0, 2)));
        assert_eq!(next(1), Some(TextRange::new(5, 7)));
        assert_eq!(next(5), Some(TextRange::new(5, 7)));
        assert_eq!(next(8), Some(TextRange::new(8, 10)));
        assert_eq!(next(9), Some(TextRange::new(0, 2)));
        assert_eq!(prev(10), Some(TextRange::new(8, 10)));
        assert_eq!(prev(8), Some(TextRange::new(5, 7)));
        assert_eq!(prev(6), Some(TextRange::new(5, 7)));
        assert_eq!(prev(5), Some(TextRange::new(0, 2)));
        assert_eq!(prev(0), Some(TextRange::new(8, 10)));
        assert_eq!(find_next("xyz", &regex, 1).unwrap(), None);
        assert_eq!(find_prev("xyz", &regex, 1).unwrap(), None);
        // Anchors and word boundaries see the text in front of the offset.
        let line_start = with("^b", true, false, true);
        assert_eq!(
            find_next("a\nb", &line_start, 2).unwrap(),
            Some(TextRange::new(2, 3))
        );
        assert_eq!(find_next("ab", &line_start, 1).unwrap(), None);
    }

    #[test]
    fn next_and_prev_find_whole_words_and_case_folded_literals() {
        let text = "foobar foo bar foo";
        let word = with("foo", true, true, false);
        let next = |from| find_next(text, &word, from).unwrap();
        let prev = |from| find_prev(text, &word, from).unwrap();
        assert_eq!(next(0), Some(TextRange::new(7, 10)));
        assert_eq!(next(7), Some(TextRange::new(7, 10)));
        assert_eq!(next(8), Some(TextRange::new(15, 18)));
        assert_eq!(next(16), Some(TextRange::new(7, 10)));
        assert_eq!(prev(18), Some(TextRange::new(15, 18)));
        assert_eq!(prev(15), Some(TextRange::new(7, 10)));
        assert_eq!(prev(7), Some(TextRange::new(15, 18)));
        let folded = query("ÇA");
        let next_folded = |from| find_next("çaÇA", &folded, from).unwrap();
        assert_eq!(next_folded(2), Some(TextRange::new(3, 6)));
        assert_eq!(next_folded(3), Some(TextRange::new(3, 6)));
        assert_eq!(next_folded(6), Some(TextRange::new(0, 3)));
    }

    #[test]
    fn next_starts_at_the_offset_even_inside_an_earlier_overlapping_match() {
        let q = with("aa", true, false, false);
        let next = |from| find_next("aaaaa", &q, from).unwrap();
        assert_eq!(next(1), Some(TextRange::new(1, 3)));
        assert_eq!(next(4), Some(TextRange::new(0, 2)));
    }

    #[test]
    fn replace_all_literal_offsets_and_dollar_signs() {
        let text = "a.b a.b";
        let q = with("a.b", true, false, false);
        let edits = replace_all(text, &q, "$1-xyz").unwrap();
        assert_eq!(
            edits,
            [
                Edit::replace(TextRange::new(0, 3), "$1-xyz"),
                Edit::replace(TextRange::new(4, 7), "$1-xyz")
            ]
        );
        assert_eq!(apply(text, &edits), "$1-xyz $1-xyz");
        let deleted = replace_all("xAx", &query("a"), "").unwrap();
        assert_eq!(deleted, [Edit::delete(TextRange::new(1, 2))]);
    }

    #[test]
    fn replace_all_regex_captures() {
        let text = "2024-05-17 and 1999-12-31";
        let q = with(r"(\d+)-(\d+)-(\d+)", true, false, true);
        let edits = replace_all(text, &q, "$3.$2.$1").unwrap();
        assert_eq!(apply(text, &edits), "17.05.2024 and 31.12.1999");
        assert_eq!(edits[1].range, TextRange::new(15, 25));
        let whole = replace_all("ab", &with("b", true, false, true), "[$0]").unwrap();
        assert_eq!(apply("ab", &whole), "a[b]");
    }

    #[test]
    fn replace_all_regex_template_forms() {
        let q = with("(a)(b)?", true, false, true);
        let check = |text: &str, template: &str, expected: &str| {
            let edits = replace_all(text, &q, template).unwrap();
            assert_eq!(apply(text, &edits), expected, "template {template:?}");
        };
        check("a", "<$2>", "<>"); // group 2 did not participate
        check("ab", "$$1", "$1");
        check("ab", "${1}0", "a0");
        check("ab", "$10", "a0"); // only 3 groups: `$1` then `0`
        check("ab", "$5", "$5"); // no such group: literal
        check("ab", "${5}", "${5}");
        check("ab", "${x}", "${x}");
        check("ab", "${}", "${}");
        check("ab", "${+1}", "${+1}");
        check(
            "ab",
            "${99999999999999999999999}",
            "${99999999999999999999999}",
        );
        check("ab", "${1", "${1");
        check("ab", "$x", "$x");
        check("ab", "cost$", "cost$");
        check("ab", "$", "$");
        check("ab", "ünï$1ç", "ünïaç");
        // Eleven groups: `$10` and `$11` name groups, `$12` is `$1` followed by `2`.
        let many = with("(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)(k)", true, false, true);
        let text = "abcdefghijk";
        let edits = replace_all(text, &many, "$10|$11|$12").unwrap();
        assert_eq!(apply(text, &edits), "j|k|a2");
        let braced = replace_all(text, &many, "${11}${1}0").unwrap();
        assert_eq!(apply(text, &braced), "ka0");
    }

    #[test]
    fn replace_all_regex_empty_matches_insert() {
        let text = "a\nb";
        let q = with("^", true, false, true);
        let edits = replace_all(text, &q, "> ").unwrap();
        assert_eq!(edits[0], Edit::insert(0, "> "));
        assert_eq!(apply(text, &edits), "> a\n> b");
    }

    #[test]
    fn replace_one_replaces_the_next_match_with_wrap() {
        let text = "x1 x2 x3";
        let q = with(r"x(\d)", true, false, true);
        let edit = replace_one(text, &q, "<$1>", 3).unwrap().unwrap();
        assert_eq!(edit, Edit::replace(TextRange::new(3, 5), "<2>"));
        let wrapped = replace_one(text, &q, "<$1>", 7).unwrap().unwrap();
        assert_eq!(wrapped.range, TextRange::new(0, 2));
        let literal = with("x2", true, false, false);
        assert_eq!(
            replace_one(text, &literal, "$1", 0).unwrap(),
            Some(Edit::replace(TextRange::new(3, 5), "$1"))
        );
        assert_eq!(replace_one("abc", &literal, "", 0).unwrap(), None);
        let wrapped = replace_one(text, &literal, "Y", 6).unwrap().unwrap();
        assert_eq!(wrapped, Edit::replace(TextRange::new(3, 5), "Y"));
        let at_offset = replace_one(text, &literal, "Y", 3).unwrap().unwrap();
        assert_eq!(at_offset, Edit::replace(TextRange::new(3, 5), "Y"));
        // The whole-word wrapper keeps its context when the search starts mid-text.
        let word = with("bar", true, true, true);
        let edit = replace_one("foobar bar", &word, "Z", 3).unwrap().unwrap();
        assert_eq!(edit.range, TextRange::new(7, 10));
    }

    #[test]
    fn searcher_is_reusable_and_matches_free_functions() {
        let q = with("é", false, false, false);
        let s = Searcher::new(&q).unwrap();
        let text = "éÉe";
        assert_eq!(s.find_all(text), find_all(text, &q).unwrap());
        assert_eq!(s.find_all(text).len(), 2);
        assert_eq!(s.next(text, 1), Some(TextRange::new(2, 4)));
    }
}
