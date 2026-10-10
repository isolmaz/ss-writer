//! Search in folder (spec §7): literal or regex search over the files of a workspace.
//!
//! The walk honors `.gitignore` files (never entering `.git`) and the hidden-items setting,
//! skips OneDrive placeholders (decided by a predicate, so nothing is hydrated) and skips binary
//! files. Text is decoded before matching: UTF-16 through its byte order mark, UTF-8 as is, and
//! any other content through an encoding guessed from the start of the file (legacy code pages
//! such as Windows-1254 or Shift_JIS). Results stream to a callback grouped by file; a shared
//! flag cancels the search.

use std::fs::File;
use std::io::{self, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Encoding, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::WalkBuilder;
use thiserror::Error;

/// How much of a file is looked at to guess its encoding.
const SAMPLE_BYTES: u64 = 32 * 1024;

/// Longest preview line kept, in bytes.
pub const MAX_PREVIEW_BYTES: usize = 400;

/// Why a search could not start.
#[derive(Debug, Error)]
pub enum SearchError {
    #[error("the search pattern is empty")]
    EmptyPattern,
    #[error("invalid search pattern: {0}")]
    InvalidPattern(#[from] grep_regex::Error),
}

/// What to search for and where to look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    pub pattern: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    /// Interpret `pattern` as a regular expression (otherwise it is literal text).
    pub regex: bool,
    /// Search hidden items too.
    pub show_hidden: bool,
    /// Stop after this many matching lines in total.
    pub max_matches: usize,
    /// Files larger than this are not searched.
    pub max_file_bytes: u64,
}

impl SearchOptions {
    /// Case-insensitive literal search with the default limits.
    #[must_use]
    pub fn literal(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_owned(),
            case_sensitive: false,
            whole_word: false,
            regex: false,
            show_hidden: false,
            max_matches: 10_000,
            max_file_bytes: 64 * 1024 * 1024,
        }
    }
}

/// A matching line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMatch {
    /// 1-based line number.
    pub line: u64,
    /// Byte offset of the first match in the line (0-based, UTF-8 after decoding).
    pub column: usize,
    /// Byte ranges of every match inside `preview`.
    pub ranges: Vec<Range<usize>>,
    /// The line without its terminator, cut at [`MAX_PREVIEW_BYTES`].
    pub preview: String,
}

/// All matches of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMatches {
    pub path: PathBuf,
    pub matches: Vec<LineMatch>,
}

/// Totals of a finished search.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchSummary {
    pub files_searched: usize,
    pub files_matched: usize,
    pub matches: usize,
    pub binary_skipped: usize,
    pub placeholders_skipped: usize,
    pub unreadable: usize,
    /// `max_matches` was reached.
    pub truncated: bool,
    pub cancelled: bool,
}

/// The encoding to decode `sample` with, when it is neither BOM-marked nor valid UTF-8.
fn guess_encoding(sample: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let has_bom = sample.starts_with(&[0xEF, 0xBB, 0xBF])
        || sample.starts_with(&[0xFF, 0xFE])
        || sample.starts_with(&[0xFE, 0xFF]);
    let utf8_prefix = match std::str::from_utf8(sample) {
        Ok(_) => true,
        // An error without a length is a multi-byte sequence cut off by the sample boundary.
        Err(error) => error.error_len().is_none(),
    };
    if has_bom || utf8_prefix {
        return None;
    }
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    detector.feed(sample, true);
    Some(detector.guess(None, Utf8Detection::Deny))
}

fn read_sample(path: &Path) -> io::Result<Vec<u8>> {
    let mut sample = Vec::new();
    File::open(path)?
        .take(SAMPLE_BYTES)
        .read_to_end(&mut sample)?;
    Ok(sample)
}

struct Collector<'a> {
    matcher: &'a RegexMatcher,
    matches: Vec<LineMatch>,
    binary: bool,
    budget: &'a mut usize,
    cancel: &'a AtomicBool,
}

impl Sink for Collector<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        let text = String::from_utf8_lossy(mat.bytes());
        let line = text.trim_end_matches(['\n', '\r']);
        let mut preview_end = line.len().min(MAX_PREVIEW_BYTES);
        while !line.is_char_boundary(preview_end) {
            preview_end -= 1;
        }
        let mut ranges = Vec::new();
        // The matcher's error type is `NoError`: matching cannot fail.
        let _ = self.matcher.find_iter(line.as_bytes(), |found| {
            ranges.push(found.start()..found.end());
            true
        });
        ranges.retain(|range| range.end <= preview_end);
        self.matches.push(LineMatch {
            line: mat.line_number().unwrap_or_default(),
            column: ranges.first().map_or(0, |range| range.start),
            ranges,
            preview: line[..preview_end].to_owned(),
        });
        *self.budget = self.budget.saturating_sub(1);
        Ok(*self.budget > 0 && !self.cancel.load(Ordering::Relaxed))
    }

    fn binary_data(&mut self, _searcher: &Searcher, _offset: u64) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}

enum FileOutcome {
    Matches(Vec<LineMatch>),
    Binary,
    Unreadable,
}

fn search_file(
    path: &Path,
    matcher: &RegexMatcher,
    budget: &mut usize,
    cancel: &AtomicBool,
) -> FileOutcome {
    let Ok(sample) = read_sample(path) else {
        return FileOutcome::Unreadable;
    };
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0));
    if let Some(encoding) = guess_encoding(&sample) {
        builder.encoding(Encoding::new(encoding.name()).ok());
    }
    let mut searcher = builder.build();
    let mut collector = Collector {
        matcher,
        matches: Vec::new(),
        binary: false,
        budget,
        cancel,
    };
    match searcher.search_path(matcher, path, &mut collector) {
        Err(_) => FileOutcome::Unreadable,
        Ok(()) if collector.binary => FileOutcome::Binary,
        Ok(()) => FileOutcome::Matches(collector.matches),
    }
}

/// Search the files below `root`, calling `on_file` for every file that has matches.
///
/// `is_placeholder` marks cloud placeholder files, which are skipped unread. Setting `cancel`
/// stops the search at the next file (or matching line); the summary then has `cancelled` set.
/// Files that cannot be read or decoded are counted, not reported as errors.
///
/// # Errors
/// [`SearchError`] when the pattern is empty or not a valid regular expression.
pub fn search_folder(
    root: &Path,
    options: &SearchOptions,
    is_placeholder: &dyn Fn(&Path) -> bool,
    cancel: &AtomicBool,
    mut on_file: impl FnMut(FileMatches),
) -> Result<SearchSummary, SearchError> {
    if options.pattern.is_empty() {
        return Err(SearchError::EmptyPattern);
    }
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!options.case_sensitive)
        .word(options.whole_word)
        .fixed_strings(!options.regex)
        .build(&options.pattern)?;
    let walker = WalkBuilder::new(root)
        .hidden(!options.show_hidden)
        .parents(false)
        .ignore(false)
        .git_global(false)
        .git_exclude(false)
        .git_ignore(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    let mut summary = SearchSummary::default();
    let mut budget = options.max_matches;
    for entry in walker.flatten() {
        if cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if entry
            .metadata()
            .is_ok_and(|meta| meta.len() > options.max_file_bytes)
        {
            continue;
        }
        if is_placeholder(entry.path()) {
            summary.placeholders_skipped += 1;
            continue;
        }
        match search_file(entry.path(), &matcher, &mut budget, cancel) {
            FileOutcome::Unreadable => summary.unreadable += 1,
            FileOutcome::Binary => summary.binary_skipped += 1,
            FileOutcome::Matches(matches) => {
                summary.files_searched += 1;
                if !matches.is_empty() {
                    summary.files_matched += 1;
                    summary.matches += matches.len();
                    on_file(FileMatches {
                        path: entry.path().to_path_buf(),
                        matches,
                    });
                }
            }
        }
        if budget == 0 {
            summary.truncated = true;
            break;
        }
    }
    summary.cancelled |= cancel.load(Ordering::Relaxed);
    Ok(summary)
}
