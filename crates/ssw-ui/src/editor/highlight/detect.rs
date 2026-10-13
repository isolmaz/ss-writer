//! Which language a plain-text document is written in.
//!
//! [`detect_language`] tries four cheap, deterministic rules in order and stops at the first
//! that matches: the whole file name (`Dockerfile`, `.bashrc`), the extension, a shebang on the
//! first line and the declaration of a markup file on the first line. Nothing else is guessed:
//! no content statistics and no modelines, so the result never surprises and costs nothing.

use super::Language;

/// Files that are known by their whole name, compared without regard to case; sorted by name.
///
/// A name in this table decides, also when it maps to `None`: the file is known and has no
/// language here, so its first line is not second-guessed.
const FILE_NAMES: &[(&str, Option<Language>)] = &[
    // Shell startup files: shell scripts that have no extension to say so.
    (".bash_aliases", Some(Language::Bash)),
    (".bash_logout", Some(Language::Bash)),
    (".bash_profile", Some(Language::Bash)),
    (".bashrc", Some(Language::Bash)),
    // Editor settings in INI syntax; there is no such language.
    (".editorconfig", None),
    // dotenv files are `KEY=value` lines with `#` comments and shell quoting.
    (".env", Some(Language::Bash)),
    // Pattern lists with `#` comments (and attributes); there is no such language.
    (".gitattributes", None),
    (".gitignore", None),
    // Startup file of login shells.
    (".profile", Some(Language::Bash)),
    // Zsh startup files: shell scripts, and Zsh is close enough to Bash for colouring.
    (".zprofile", Some(Language::Bash)),
    (".zshenv", Some(Language::Bash)),
    (".zshrc", Some(Language::Bash)),
    // CMake's own scripting language.
    ("CMakeLists.txt", None),
    // Container build files: `#` comments, `$VAR`, quoting and `RUN` lines of shell, so Bash is
    // the closest language there is.
    ("Containerfile", Some(Language::Bash)),
    ("Dockerfile", Some(Language::Bash)),
    // Ruby programs without an extension; Ruby is not a language here.
    ("Gemfile", None),
    // Makefiles: `#` comments, `$(VAR)` and shell recipes, so Bash again.
    ("GNUmakefile", Some(Language::Bash)),
    ("Makefile", Some(Language::Bash)),
    ("Rakefile", None),
    ("Vagrantfile", None),
];

/// Interpreters named in a shebang that [`Language::from_name`] does not know by that name.
const INTERPRETERS: &[(&str, Language)] = &[
    // The Almquist shell: BusyBox and Alpine's `/bin/sh`.
    ("ash", Language::Bash),
    // Debian's `/bin/sh`.
    ("dash", Language::Bash),
    // The Korn shell, which Bash extends.
    ("ksh", Language::Bash),
    // Node.js under both of its program names.
    ("node", Language::JavaScript),
    ("nodejs", Language::JavaScript),
];

/// The language of a plain-text document, or `None` when nothing identifies one.
///
/// `name` is the file's name or path (what follows the last `/` or `\` is the name); `head` is
/// the start of the content. Only the first line of `head` is read, so the caller can pass a
/// bounded prefix (a few hundred bytes are plenty) instead of the whole text. The first rule
/// that matches wins:
///
/// 1. The whole file name, case-insensitively: `Dockerfile`, `Makefile`, `.gitignore`,
///    `.bashrc`, … A listed name decides even when it has no language (`.gitignore`).
/// 2. The extension, through [`Language::from_name`] and its aliases: `notes.rs`, `app.tsx`,
///    `data.yml`. A dot that starts the name is not an extension, so `.gitignore` has none.
/// 3. A shebang: `#!`, then the last path component of the interpreter, looking through `env`
///    (its options and `NAME=value` arguments) and dropping a trailing version number, so
///    `#!/usr/bin/env python3.11` is `python`. The result goes through [`Language::from_name`].
/// 4. A markup declaration: a first line that starts with `<?xml` is XML, one that starts with
///    `<!DOCTYPE html` or `<html` (case-insensitively) is HTML.
///
/// A byte order mark in front of `head` is ignored.
#[must_use]
pub fn detect_language(name: &str, head: &str) -> Option<Language> {
    let file_name = last_component(name);
    if let Some(language) = known_file(file_name) {
        return language;
    }
    if let Some(language) = extension(file_name).and_then(Language::from_name) {
        return Some(language);
    }
    let line = first_line(head);
    from_shebang(line).or_else(|| from_markup(line))
}

/// What the table says about `file_name`: `Some(language)` for a listed name, whose language
/// may be none.
fn known_file(file_name: &str) -> Option<Option<Language>> {
    FILE_NAMES
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(file_name))
        .map(|&(_, language)| language)
}

/// What follows the last `/` or `\`.
fn last_component(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// What follows the last dot of `file_name`, unless the dot starts the name.
fn extension(file_name: &str) -> Option<&str> {
    let (stem, extension) = file_name.rsplit_once('.')?;
    (!stem.is_empty()).then_some(extension)
}

/// The first line of `head` without its terminator and without a byte order mark.
fn first_line(head: &str) -> &str {
    let head = head.strip_prefix('\u{feff}').unwrap_or(head);
    head.split(['\n', '\r']).next().unwrap_or(head)
}

/// The language of the interpreter a shebang line names.
fn from_shebang(line: &str) -> Option<Language> {
    let mut words = line.strip_prefix("#!")?.split_whitespace();
    let mut program = last_component(words.next()?);
    if program.eq_ignore_ascii_case("env") {
        program = env_command(words)?;
    }
    let name = program.trim_end_matches(is_version_char);
    Language::from_name(name).or_else(|| interpreter(name))
}

/// The command `env` runs: the first argument that is neither an option nor a `NAME=value`
/// assignment, reduced to its last path component. Options such as `-S` and `-i` are skipped,
/// and so is the argument that `-u`, `-C` and `-P` take.
fn env_command<'a>(mut args: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    while let Some(arg) = args.next() {
        if matches!(arg, "-u" | "--unset" | "-C" | "--chdir" | "-P" | "--path") {
            args.next();
        } else if !arg.starts_with('-') && !arg.contains('=') {
            return Some(last_component(arg));
        }
    }
    None
}

/// What a version number adds to the name of an interpreter (`python3.11`, `ksh93`, `bash-5`).
fn is_version_char(ch: char) -> bool {
    ch.is_ascii_digit() || matches!(ch, '.' | '-')
}

/// The language of an interpreter that `Language::from_name` does not know by name.
fn interpreter(name: &str) -> Option<Language> {
    INTERPRETERS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
        .map(|&(_, language)| language)
}

/// The language a markup file announces on its first line.
fn from_markup(line: &str) -> Option<Language> {
    let starts = |prefix| starts_with_ignore_case(line, prefix);
    if starts("<?xml") {
        Some(Language::Xml)
    } else if starts("<!doctype html") || starts("<html") {
        Some(Language::Html)
    } else {
        None
    }
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    let start = text.get(..prefix.len());
    start.is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}
