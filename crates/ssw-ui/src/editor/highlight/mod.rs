//! Dependency-free syntax highlighting for fenced code blocks and for whole plain-text documents.
//!
//! [`Language::from_name`] resolves an info string or a file extension (`rust`, `c++`, `js`,
//! `ps1`, …) to one of the supported [`Language`]s; unknown names are plain code. [`tokenize`]
//! splits a text into [`Token`]s (everything not listed is plain). [`Highlighter`] caches the
//! tokens of every code block, or of a plain-text document as a whole, by line together with the
//! scanner state at the start of each line, so after an edit only the lines whose text or
//! incoming state changed are scanned again. [`SyntaxColors`] maps token kinds to colors derived
//! from the theme tokens.

use std::collections::HashMap;

use crate::geometry::Color;
use crate::text::{FontWeight, StyledRange, TextStyle};
use crate::theme::{Theme, ThemeMode};

mod detect;
mod engine;
mod langs;
mod markup;

pub use detect::detect_language;

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// Text without a role (identifiers, operators, punctuation).
    Plain,
    /// Reserved words.
    Keyword,
    /// Type names, lifetimes, selectors.
    Type,
    /// `true`, `null`, `NULL`, `SOME_CONSTANT`.
    Constant,
    /// Called functions, macros, well-known commands.
    Function,
    /// Numeric literals and colors.
    Number,
    /// String and character literals (without their escapes).
    String,
    /// Escape sequences and character references inside strings and markup.
    Escape,
    /// Line and block comments.
    Comment,
    /// Keys (JSON, YAML, TOML) and CSS properties.
    Property,
    /// Markup tag names.
    Tag,
    /// Attribute names, decorators and options.
    Attribute,
    /// Variables (`$name`).
    Variable,
    /// Added lines of a diff.
    Added,
    /// Removed lines of a diff.
    Removed,
    /// Markdown headings.
    Heading,
}

/// A run of text of one [`TokenKind`]: UTF-8 byte offsets into the highlighted text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    /// First byte.
    pub start: usize,
    /// One past the last byte.
    pub end: usize,
    /// The role of the text.
    pub kind: TokenKind,
}

/// The languages the highlighter knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    /// Rust.
    Rust,
    /// C.
    C,
    /// C++.
    Cpp,
    /// C#.
    CSharp,
    /// Java.
    Java,
    /// JavaScript (also JSX).
    JavaScript,
    /// TypeScript (also TSX).
    TypeScript,
    /// JSON (also with comments).
    Json,
    /// Python.
    Python,
    /// Go.
    Go,
    /// Bash and POSIX shells.
    Bash,
    /// PowerShell.
    PowerShell,
    /// HTML (the bodies of `<script>` and `<style>` are JavaScript, JSON and CSS).
    Html,
    /// XML (also SVG, XAML and XHTML, whose `<script/>` is an empty element, not a raw-text one).
    Xml,
    /// CSS.
    Css,
    /// SQL.
    Sql,
    /// YAML.
    Yaml,
    /// TOML.
    Toml,
    /// Markdown.
    Markdown,
    /// Unified diffs and patches.
    Diff,
}

impl Language {
    /// Every language.
    pub const ALL: [Language; 20] = [
        Language::Rust,
        Language::C,
        Language::Cpp,
        Language::CSharp,
        Language::Java,
        Language::JavaScript,
        Language::TypeScript,
        Language::Json,
        Language::Python,
        Language::Go,
        Language::Bash,
        Language::PowerShell,
        Language::Html,
        Language::Xml,
        Language::Css,
        Language::Sql,
        Language::Yaml,
        Language::Toml,
        Language::Markdown,
        Language::Diff,
    ];

    /// The language named by an info string word: case-insensitive, with the usual aliases
    /// (`rs`, `c++`, `cs`, `js`, `ts`, `py`, `sh`, `ps1`, `yml`, `md`, `patch`, …). Decorations
    /// such as `{.rust}` or `rust,ignore` are ignored. The names double as file extensions, so
    /// none is an alias that names another kind of file (`ps` is PostScript, not PowerShell).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Language> {
        let name = name.trim().trim_start_matches(['{', '.']);
        let name = name
            .split([',', '{', '}', ' ', '=', ':'])
            .next()
            .unwrap_or("");
        let language = match name.to_ascii_lowercase().as_str() {
            "rust" | "rs" => Language::Rust,
            "c" | "h" => Language::C,
            "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hxx" | "h++" => Language::Cpp,
            "csharp" | "c#" | "cs" => Language::CSharp,
            "java" => Language::Java,
            "javascript" | "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "typescript" | "ts" | "tsx" => Language::TypeScript,
            "json" | "jsonc" | "json5" => Language::Json,
            "python" | "py" | "python3" => Language::Python,
            "go" | "golang" => Language::Go,
            "bash" | "sh" | "shell" | "zsh" | "console" => Language::Bash,
            "powershell" | "ps1" | "pwsh" => Language::PowerShell,
            "html" | "htm" => Language::Html,
            "xml" | "xhtml" | "svg" | "xaml" | "xsd" | "xsl" | "plist" => Language::Xml,
            "css" => Language::Css,
            "sql" => Language::Sql,
            "yaml" | "yml" => Language::Yaml,
            "toml" => Language::Toml,
            "markdown" | "md" => Language::Markdown,
            "diff" | "patch" => Language::Diff,
            _ => return None,
        };
        Some(language)
    }

    /// The canonical lowercase name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::CSharp => "csharp",
            Language::Java => "java",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Json => "json",
            Language::Python => "python",
            Language::Go => "go",
            Language::Bash => "bash",
            Language::PowerShell => "powershell",
            Language::Html => "html",
            Language::Xml => "xml",
            Language::Css => "css",
            Language::Sql => "sql",
            Language::Yaml => "yaml",
            Language::Toml => "toml",
            Language::Markdown => "markdown",
            Language::Diff => "diff",
        }
    }
}

/// What the scanner is in the middle of at a line boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Normal,
    /// Inside a block comment (`depth` counts nesting).
    Comment,
    /// Inside a multi-line string of rule `n` (`depth` is 1 for raw strings).
    Str(u8),
    /// Inside a Rust raw string with `depth` hashes.
    RawStr,
    /// Inside an HTML/XML tag.
    Tag,
    /// Inside a quoted attribute value (the quote byte).
    TagStr(u8),
    /// Inside a fenced code block of a Markdown text.
    Fence,
}

/// The scanner state at a line boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct State {
    mode: Mode,
    depth: u8,
    /// HTML only: the `<script>` or `<style>` element being opened or scanned. In its body
    /// `mode` and `depth` belong to the guest language.
    embed: markup::Embed,
}

/// Scans one line (without its terminator); tokens are relative to the line. The markup
/// languages have their own scanners, every other language is a `Spec` for the generic one.
fn scan_line(language: Language, line: &str, state: State, out: &mut Vec<Token>) -> State {
    let spec = match language {
        Language::Rust => &langs::RUST,
        Language::C => &langs::C,
        Language::Cpp => &langs::CPP,
        Language::CSharp => &langs::CSHARP,
        Language::Java => &langs::JAVA,
        Language::JavaScript => &langs::JAVASCRIPT,
        Language::TypeScript => &langs::TYPESCRIPT,
        Language::Json => &langs::JSON,
        Language::Python => &langs::PYTHON,
        Language::Go => &langs::GO,
        Language::Bash => &langs::BASH,
        Language::PowerShell => &langs::POWERSHELL,
        Language::Sql => &langs::SQL,
        Language::Yaml => &langs::YAML,
        Language::Toml => &langs::TOML,
        Language::Html => return markup::scan_markup(line, state, true, out),
        Language::Xml => return markup::scan_markup(line, state, false, out),
        Language::Css => return markup::scan_css(line, state, out),
        Language::Markdown => return markup::scan_markdown(line, state, out),
        Language::Diff => {
            markup::scan_diff(line, out);
            return State::default();
        }
    };
    engine::scan_line(spec, line, state, out)
}

/// Splits `text` into the lines the scanner works on: the content without `\n` / `\r\n`, and
/// the length of the full line including its terminator.
fn lines(text: &str) -> impl Iterator<Item = (&str, usize)> {
    text.split_inclusive('\n').map(|raw| {
        let content = raw.strip_suffix('\n').unwrap_or(raw);
        (content.strip_suffix('\r').unwrap_or(content), raw.len())
    })
}

/// The non-plain tokens of `text` in `language`, in order and without overlap.
#[must_use]
pub fn tokenize(language: Language, text: &str) -> Vec<Token> {
    Highlighter::default().highlight(0, language, text)
}

#[derive(Debug, Clone)]
struct CachedLine {
    text: String,
    state_in: State,
    state_out: State,
    /// Relative to the line.
    tokens: Vec<Token>,
}

#[derive(Debug)]
struct Entry {
    language: Language,
    lines: Vec<CachedLine>,
    stamp: u64,
}

/// Most blocks whose tokens are kept.
const CACHE_LIMIT: usize = 256;

/// Tokenizes code blocks incrementally: every block (identified by a caller-chosen key such as
/// its start offset) remembers its lines, their tokens and the scanner state before each line.
/// Highlighting changed text reuses every line whose content and incoming state are unchanged,
/// so typing in a large block rescans one line (plus the lines whose state it changes, such as
/// the rest of a block comment). Lines are matched by index: inserting or deleting a line break
/// moves every line below it to another index and rescans them.
#[derive(Debug, Default)]
pub struct Highlighter {
    entries: HashMap<usize, Entry>,
    clock: u64,
    scanned: usize,
}

impl Highlighter {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The tokens of `text` (a block with cache key `key`) in `language`.
    pub fn highlight(&mut self, key: usize, language: Language, text: &str) -> Vec<Token> {
        self.clock += 1;
        self.scanned = 0;
        let mut previous: Vec<Option<CachedLine>> = self
            .entries
            .remove(&key)
            .filter(|entry| entry.language == language)
            .map(|entry| entry.lines.into_iter().map(Some).collect())
            .unwrap_or_default();
        let mut cached = Vec::new();
        let mut tokens = Vec::new();
        let mut state = State::default();
        let mut offset = 0;
        for (index, (content, full)) in lines(text).enumerate() {
            let reused = previous
                .get_mut(index)
                .and_then(Option::take)
                .filter(|line| line.state_in == state && line.text == content);
            let line = reused.unwrap_or_else(|| {
                self.scanned += 1;
                let mut relative = Vec::new();
                let state_out = scan_line(language, content, state, &mut relative);
                CachedLine {
                    text: content.to_owned(),
                    state_in: state,
                    state_out,
                    tokens: relative,
                }
            });
            tokens.extend(line.tokens.iter().map(|token| Token {
                start: token.start + offset,
                end: token.end + offset,
                kind: token.kind,
            }));
            state = line.state_out;
            offset += full;
            cached.push(line);
        }
        self.entries.insert(
            key,
            Entry {
                language,
                lines: cached,
                stamp: self.clock,
            },
        );
        if self.entries.len() > CACHE_LIMIT
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.stamp)
                .map(|(key, _)| *key)
        {
            self.entries.remove(&oldest);
        }
        tokens
    }

    /// How many lines the last [`Highlighter::highlight`] call had to scan (the others came from
    /// the cache).
    #[must_use]
    pub fn last_scanned_lines(&self) -> usize {
        self.scanned
    }

    /// The number of blocks whose tokens are cached.
    #[must_use]
    pub fn cached_blocks(&self) -> usize {
        self.entries.len()
    }
}

/// Token colors derived from the theme tokens (callout and accent colors are tuned for contrast
/// on the surface in every theme); high contrast uses the text color for everything but
/// comments. [`SyntaxColors::calm`] is the quieter variant for whole documents.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyntaxColors {
    text: Color,
    muted: Color,
    keyword: Color,
    type_: Color,
    constant: Color,
    function: Color,
    string: Color,
    escape: Color,
    property: Color,
    tag: Color,
    attribute: Color,
    variable: Color,
    added: Color,
    removed: Color,
    heading: Color,
    colored: bool,
    calm: bool,
}

impl SyntaxColors {
    /// The kinds the calm palette colors.
    const CALM_KINDS: [TokenKind; 3] = [TokenKind::Comment, TokenKind::Keyword, TokenKind::String];

    /// The colors of `theme`.
    #[must_use]
    pub fn from_theme(theme: &Theme) -> Self {
        let tokens = &theme.tokens;
        let callout = &tokens.callout;
        Self {
            text: tokens.text,
            muted: tokens.text_muted,
            keyword: callout.important.bar,
            type_: callout.note.bar,
            constant: callout.warning.bar,
            function: tokens.accent,
            string: callout.tip.bar,
            escape: callout.caution.bar,
            property: callout.note.bar,
            tag: callout.important.bar,
            attribute: callout.warning.bar,
            variable: tokens.accent,
            added: tokens.success,
            removed: tokens.error,
            heading: tokens.accent,
            colored: theme.mode != ThemeMode::HighContrast,
            calm: false,
        }
    }

    /// The colors of `theme` for a whole document: a page full of colored types, tags and numbers
    /// is noisy, so only comments, strings and keywords are colored and everything else keeps the
    /// text color and weight (headings are not bold either).
    #[must_use]
    pub fn calm(theme: &Theme) -> Self {
        Self {
            calm: true,
            ..Self::from_theme(theme)
        }
    }

    /// Whether [`SyntaxColors::styled`] can make a token of `kind` look different from plain
    /// text: every kind can in the full palette, only comments, keywords and strings in the calm
    /// one. A caller skips the other tokens instead of styling them as the text they already are.
    #[must_use]
    pub fn colors(&self, kind: TokenKind) -> bool {
        !self.calm || Self::CALM_KINDS.contains(&kind)
    }

    /// The color of `kind`.
    #[must_use]
    pub fn color(&self, kind: TokenKind) -> Color {
        if !self.colored {
            return match kind {
                TokenKind::Comment => self.muted,
                _ => self.text,
            };
        }
        if self.calm && !Self::CALM_KINDS.contains(&kind) {
            return self.text;
        }
        match kind {
            TokenKind::Plain => self.text,
            TokenKind::Keyword => self.keyword,
            TokenKind::Type => self.type_,
            TokenKind::Constant | TokenKind::Number => self.constant,
            TokenKind::Function => self.function,
            TokenKind::String => self.string,
            TokenKind::Escape => self.escape,
            TokenKind::Comment => self.muted,
            TokenKind::Property => self.property,
            TokenKind::Tag => self.tag,
            TokenKind::Attribute => self.attribute,
            TokenKind::Variable => self.variable,
            TokenKind::Added => self.added,
            TokenKind::Removed => self.removed,
            TokenKind::Heading => self.heading,
        }
    }

    /// The style range of `token` over `base` (color, italic comments, bold headings except in
    /// the calm palette, which only changes colors and keeps the italic comments), with offsets
    /// shifted by `shift` into the display text.
    #[must_use]
    pub fn styled(&self, token: &Token, base: &TextStyle, shift: usize) -> StyledRange {
        let mut style = base.clone().with_color(self.color(token.kind));
        match token.kind {
            TokenKind::Comment => style = style.with_italic(true),
            TokenKind::Heading if !self.calm => style = style.with_weight(FontWeight::BOLD),
            _ => {}
        }
        StyledRange {
            range: token.start + shift..token.end + shift,
            style,
        }
    }
}
