//! The workspace index (spec §5): file list, headings and links of Markdown files, kept in the
//! state database for Quick Open, wikilink completion, backlinks and rename updates.
//!
//! * [`Store::index_workspace`] walks the root (honoring `.gitignore` files, never `.git`, and
//!   hidden items per [`IndexOptions`]) and only re-reads files whose size or modification time
//!   changed. OneDrive placeholders (decided by the predicate the caller passes) are listed but
//!   never read, so indexing cannot hydrate them.
//! * [`Store::index_apply_changes`] updates single paths from file-system events. A change to a
//!   `.gitignore` file needs a full [`Store::index_workspace`] run.
//! * Links are stored with a normalized target so [`Store::index_backlinks`] and
//!   [`Store::index_links_to`] find every wikilink (`[[name]]`, `[[folder/name]]`, shortest
//!   unambiguous suffix of the path, extension optional) and every relative Markdown link,
//!   reference definition or image that points at a file. All matching is case-insensitive.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rusqlite::{Connection, Transaction, params};
use ssw_core::markdown::ast::{
    BlockKind, ImageKind, Inline, InlineKind, LinkKind, WikiLink, iter_inlines,
};
use ssw_core::markdown::parse_document;
use ssw_core::text::TextRange;

use crate::store::{Store, StoreError, UnixSeconds, key_of, path_text, query_optional, query_rows};

/// Markdown files larger than this are listed but not parsed.
pub const MAX_PARSE_BYTES: u64 = 16 * 1024 * 1024;

const MARKDOWN_EXTENSIONS: [&str; 3] = ["md", "markdown", "mdx"];

/// Steps written per transaction: few enough that the write lock is held for milliseconds, so
/// the interface's own writes to the database never wait for an indexing run.
const WRITE_CHUNK: usize = 256;

/// What to index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexOptions {
    /// Include hidden items (dot-files and files with the hidden attribute).
    pub show_hidden: bool,
    /// Stop after this many files (spec §10 "index limits").
    pub max_files: usize,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            show_hidden: false,
            max_files: 100_000,
        }
    }
}

/// Outcome of an indexing run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Files in the index for this root after the run.
    pub files: usize,
    /// Files that were (re)read and parsed.
    pub parsed: usize,
    /// Files dropped from the index.
    pub removed: usize,
    /// Files listed without reading because the placeholder predicate matched.
    pub placeholders: usize,
    /// The walk stopped at [`IndexOptions::max_files`].
    pub truncated: bool,
}

/// A file-system event for [`Store::index_apply_changes`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexChange {
    /// Created, modified or renamed to this path (a folder is indexed recursively).
    Changed(PathBuf),
    /// Deleted or renamed away (a folder removes everything below it).
    Removed(PathBuf),
}

/// A file of the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedFile {
    pub path: PathBuf,
    /// Path relative to the root, `/` separated.
    pub rel: String,
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub modified_ms: i64,
    pub markdown: bool,
    pub placeholder: bool,
}

/// A heading of a Markdown file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedHeading {
    pub level: u8,
    /// Plain text of the heading.
    pub text: String,
    /// Byte offset of the heading in the file.
    pub offset: usize,
}

/// A place in a file that links to another file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSite {
    /// The file containing the link.
    pub source: PathBuf,
    /// `[[wikilink]]` rather than a Markdown link, reference definition or image.
    pub wiki: bool,
    /// The target as the parser resolved it (wikilink target text, or Markdown destination).
    pub target: String,
    /// The `#fragment` / `[[target#heading]]` part.
    pub heading: Option<String>,
    /// Byte range of the target text in the source file (what a rename update rewrites).
    pub range: TextRange,
}

// ---------------------------------------------------------------------------------------------
// Markdown analysis
// ---------------------------------------------------------------------------------------------

struct HeadingRow {
    level: u8,
    text: String,
    offset: usize,
}

struct LinkRow {
    wiki: bool,
    target: String,
    target_key: String,
    leaf_key: String,
    heading: Option<String>,
    range: TextRange,
}

#[derive(Default)]
struct Analysis {
    headings: Vec<HeadingRow>,
    links: Vec<LinkRow>,
}

fn is_markdown_extension(ext: &str) -> bool {
    MARKDOWN_EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(ext))
}

fn is_markdown_path(rel: &str) -> bool {
    rel.rsplit_once('.')
        .is_some_and(|(_, ext)| is_markdown_extension(ext))
}

/// `name` without a Markdown extension (other names are returned unchanged).
fn strip_markdown_extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && is_markdown_extension(ext) => stem,
        _ => name,
    }
}

/// `name` without its last extension (dot-files keep their name).
fn file_stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(high), Some(low)) = (
                bytes.get(i + 1).and_then(|b| hex_value(*b)),
                bytes.get(i + 2).and_then(|b| hex_value(*b)),
            )
        {
            out.push(high * 16 + low);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// True for `scheme:` prefixes (`https:`, `mailto:`, `file:`, drive letters like `C:`).
fn has_scheme(dest: &str) -> bool {
    dest.find(':').is_some_and(|colon| {
        let scheme = &dest[..colon];
        scheme
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    })
}

/// Resolve a Markdown destination written in the file `source_rel` (root-relative, `/`
/// separated) to a root-relative path and optional `#fragment`. `None` for URLs, same-file
/// anchors, absolute paths outside the workspace and paths escaping the root.
fn resolve_destination(source_rel: &str, dest: &str) -> Option<(String, Option<String>)> {
    if dest.is_empty() || dest.starts_with('#') || dest.starts_with("//") || has_scheme(dest) {
        return None;
    }
    let (path_part, fragment) = match dest.split_once('#') {
        Some((path, fragment)) => (path, Some(percent_decode(fragment))),
        None => (dest, None),
    };
    let path_part = path_part.split('?').next().unwrap_or(path_part);
    let decoded = percent_decode(path_part).replace('\\', "/");
    let mut parts: Vec<&str> = if decoded.starts_with('/') {
        Vec::new()
    } else {
        let mut dir: Vec<&str> = source_rel.split('/').collect();
        dir.pop();
        dir
    };
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            name => parts.push(name),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some((parts.join("/"), fragment))
}

fn markdown_link(source_rel: &str, dest: &str, range: TextRange) -> Option<LinkRow> {
    resolve_destination(source_rel, dest).map(|(path, heading)| {
        let name = path.rsplit('/').next().unwrap_or(&path);
        LinkRow {
            wiki: false,
            target: dest.to_owned(),
            target_key: key_of(&path),
            leaf_key: key_of(file_stem(name)),
            heading,
            range,
        }
    })
}

fn wiki_link(link: &WikiLink) -> Option<LinkRow> {
    let target = link.target.trim().replace('\\', "/");
    if target.is_empty() {
        return None;
    }
    let without_extension = strip_markdown_extension(&target);
    let target_key = key_of(without_extension);
    let leaf_key = target_key
        .rsplit('\\')
        .next()
        .unwrap_or(&target_key)
        .to_owned();
    Some(LinkRow {
        wiki: true,
        target: link.target.clone(),
        target_key,
        leaf_key,
        heading: link.heading.clone(),
        range: link.target_range,
    })
}

/// The text a reader sees in a heading.
fn plain_text(inlines: &[Inline], src: &str, out: &mut String) {
    for inline in inlines {
        match &inline.kind {
            InlineKind::Text => out.push_str(inline.range.get(src).unwrap_or_default()),
            InlineKind::Escape | InlineKind::InlineMath | InlineKind::WikiLink(_) => {
                out.push_str(inline.content.get(src).unwrap_or_default());
            }
            InlineKind::Entity { decoded } => out.push_str(decoded),
            InlineKind::Code { text } => out.push_str(text),
            InlineKind::SoftBreak | InlineKind::HardBreak => out.push(' '),
            InlineKind::Emphasis
            | InlineKind::Strong
            | InlineKind::Underline
            | InlineKind::Strikethrough
            | InlineKind::Highlight { .. }
            | InlineKind::Link(_) => plain_text(&inline.children, src, out),
            InlineKind::Image(_) | InlineKind::FootnoteRef { .. } | InlineKind::RawHtml => {}
        }
    }
}

fn analyze(src: &str, source_rel: &str) -> Analysis {
    let document = parse_document(src);
    let mut analysis = Analysis::default();
    for block in document.iter_blocks() {
        match &block.kind {
            BlockKind::Heading { level, .. } => {
                let mut text = String::new();
                plain_text(&block.inlines, src, &mut text);
                analysis.headings.push(HeadingRow {
                    level: *level,
                    text: text.trim().to_owned(),
                    offset: block.range.start,
                });
            }
            BlockKind::LinkReferenceDefinition(definition) => analysis.links.extend(markdown_link(
                source_rel,
                &definition.dest,
                definition.dest_range,
            )),
            _ => {}
        }
        for group in block.inline_groups() {
            for inline in iter_inlines(group) {
                let row = match &inline.kind {
                    InlineKind::Link(link) => match &link.kind {
                        LinkKind::Inline { dest_range, .. } => {
                            markdown_link(source_rel, &link.dest, *dest_range)
                        }
                        LinkKind::Reference { .. } | LinkKind::Autolink { .. } => None,
                    },
                    InlineKind::Image(image) => match &image.kind {
                        ImageKind::Inline { dest_range, .. } => {
                            markdown_link(source_rel, &image.dest, *dest_range)
                        }
                        ImageKind::Reference { .. } | ImageKind::Html => None,
                    },
                    InlineKind::WikiLink(link) => wiki_link(link),
                    _ => None,
                };
                analysis.links.extend(row);
            }
        }
    }
    analysis
}

// ---------------------------------------------------------------------------------------------
// Walking
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Found {
    rel: String,
    path: PathBuf,
    size: i64,
    mtime: i64,
}

fn modified_millis(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// `path` relative to `root` (case-insensitive), `/` separated.
fn relative_path(root: &str, path: &str) -> Option<String> {
    let root = root.replace('\\', "/");
    let root = root.trim_end_matches('/');
    let path = path.replace('\\', "/");
    path.get(..root.len())
        .filter(|head| key_of(head) == key_of(root))
        .and_then(|_| path[root.len()..].strip_prefix('/'))
        .filter(|rest| !rest.is_empty())
        .map(str::to_owned)
}

fn found_from(root: &str, entry: &ignore::DirEntry) -> Option<Found> {
    entry
        .path()
        .to_str()
        .and_then(|path| relative_path(root, path))
        .zip(entry.metadata().ok())
        .map(|(rel, meta)| Found {
            rel,
            path: entry.path().to_path_buf(),
            size: meta.len() as i64,
            mtime: modified_millis(&meta),
        })
}

fn walk(
    root: &Path,
    root_text: &str,
    opts: &IndexOptions,
    cancel: &AtomicBool,
) -> Result<(Vec<Found>, bool), StoreError> {
    let walker = WalkBuilder::new(root)
        .hidden(!opts.show_hidden)
        .parents(false)
        .ignore(false)
        .git_global(false)
        .git_exclude(false)
        .git_ignore(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    let mut files = Vec::new();
    for entry in walker.flatten() {
        if cancel.load(Ordering::Relaxed) {
            return Err(StoreError::Cancelled);
        }
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if files.len() >= opts.max_files {
            return Ok((files, true));
        }
        files.extend(found_from(root_text, &entry));
    }
    Ok((files, false))
}

fn is_hidden_item(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    path.file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."))
        || path
            .metadata()
            .is_ok_and(|meta| meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
}

fn gitignore_in(dir: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(dir);
    // A missing or partly invalid .gitignore simply contributes fewer rules.
    let _ = builder.add(dir.join(".gitignore"));
    builder.build().unwrap_or(Gitignore::empty())
}

/// Whether the walk of `root` would have listed the file `rel` (root-relative, `/` separated);
/// used for single-path updates.
fn is_included(root: &Path, rel: &str, opts: &IndexOptions) -> bool {
    let components: Vec<&str> = rel.split('/').collect();
    let mut dir = root.to_path_buf();
    let mut matchers = vec![gitignore_in(&dir)];
    for (index, component) in components.iter().enumerate() {
        let child = dir.join(component);
        let is_dir = index + 1 < components.len();
        if *component == ".git" || (!opts.show_hidden && is_hidden_item(&child)) {
            return false;
        }
        let ignored = matchers
            .iter()
            .rev()
            .map(|matcher| matcher.matched(&child, is_dir))
            .find(|verdict| !verdict.is_none())
            .is_some_and(|verdict| verdict.is_ignore());
        if ignored {
            return false;
        }
        if is_dir {
            matchers.push(gitignore_in(&child));
            dir = child;
        }
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------------------------

struct Root {
    id: i64,
    text: String,
}

fn root_identity(root: &Path) -> Result<(String, String), StoreError> {
    let text = path_text(root)?.trim_end_matches(['\\', '/']).to_owned();
    let key = key_of(&text);
    Ok((text, key))
}

fn find_root(conn: &Connection, key: &str) -> Result<Option<Root>, StoreError> {
    let row: Option<(i64, String)> = query_optional(
        conn,
        "SELECT id, path FROM index_roots WHERE key = ?1",
        [key],
    )?;
    Ok(row.map(|(id, text)| Root { id, text }))
}

/// Create the row of the root `key` or refresh its path and time; returns its id.
fn upsert_root(
    conn: &Connection,
    key: &str,
    text: &str,
    now: UnixSeconds,
) -> Result<i64, StoreError> {
    Ok(conn.query_row(
        "INSERT INTO index_roots (key, path, indexed_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET path = excluded.path, indexed_at = excluded.indexed_at
         RETURNING id",
        params![key, text, now],
        |row| row.get(0),
    )?)
}

/// Size, modification time and placeholder flag of every stored file of the root `key`, by
/// relative key.
fn stored_files(
    conn: &Connection,
    key: &str,
) -> Result<HashMap<String, (i64, i64, bool)>, StoreError> {
    let rows: Vec<(String, i64, i64, bool)> = query_rows(
        conn,
        "SELECT f.rel_key, f.size, f.mtime, f.placeholder
         FROM index_files f JOIN index_roots r ON r.id = f.root_id WHERE r.key = ?1",
        [key],
    )?;
    Ok(rows
        .into_iter()
        .map(|(rel_key, size, mtime, placeholder)| (rel_key, (size, mtime, placeholder)))
        .collect())
}

/// Delete the file `rel_key`; with `children` also everything below it (it was a folder).
fn remove_path(
    tx: &Transaction<'_>,
    root_id: i64,
    rel_key: &str,
    children: bool,
) -> Result<usize, StoreError> {
    let mut removed = tx.execute(
        "DELETE FROM index_files WHERE root_id = ?1 AND rel_key = ?2",
        params![root_id, rel_key],
    )?;
    if children {
        removed += tx.execute(
            "DELETE FROM index_files WHERE root_id = ?1
             AND substr(rel_key, 1, length(?2)) = ?2",
            params![root_id, format!("{rel_key}\\")],
        )?;
    }
    Ok(removed)
}

fn read_markdown(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| encoding_rs::UTF_8.decode(&bytes).0.into_owned())
}

/// A file read and analysed before its rows are written, so that no transaction waits for the
/// disk or the parser.
struct Prepared {
    found: Found,
    placeholder: bool,
    /// `None` when the file is not parsed: not Markdown, a placeholder, too large or unreadable.
    analysis: Option<Analysis>,
}

fn prepare(found: Found, placeholder: bool) -> Prepared {
    let analysis = if is_markdown_path(&found.rel)
        && !placeholder
        && (found.size as u64) <= MAX_PARSE_BYTES
    {
        read_markdown(&found.path).map(|text| analyze(&text, &found.rel))
    } else {
        None
    };
    Prepared {
        found,
        placeholder,
        analysis,
    }
}

/// One write of an update from file-system events.
enum Step {
    /// Drop the file `key` (a relative key); with `children` also everything below it (it was a
    /// folder).
    Remove { key: String, children: bool },
    /// Store the file unless the limit on indexed files stops a new one.
    Store(Prepared),
}

/// Writes the rows of `file`; returns whether the file was parsed.
fn store_file(tx: &Transaction<'_>, root_id: i64, file: &Prepared) -> Result<bool, StoreError> {
    let found = &file.found;
    let markdown = is_markdown_path(&found.rel);
    let unparsed = Analysis::default();
    let analysis = file.analysis.as_ref().unwrap_or(&unparsed);
    let file_id: i64 = tx
        .prepare_cached(
            "INSERT INTO index_files (root_id, rel, rel_key, size, mtime, markdown, placeholder)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(root_id, rel_key) DO UPDATE SET
                 rel = excluded.rel, size = excluded.size, mtime = excluded.mtime,
                 markdown = excluded.markdown, placeholder = excluded.placeholder
             RETURNING id",
        )?
        .query_row(
            params![
                root_id,
                found.rel,
                key_of(&found.rel),
                found.size,
                found.mtime,
                markdown,
                file.placeholder
            ],
            |row| row.get(0),
        )?;
    tx.prepare_cached("DELETE FROM index_headings WHERE file_id = ?1")?
        .execute([file_id])?;
    tx.prepare_cached("DELETE FROM index_links WHERE file_id = ?1")?
        .execute([file_id])?;
    for (ord, heading) in analysis.headings.iter().enumerate() {
        tx.prepare_cached(
            "INSERT INTO index_headings (file_id, ord, level, text, pos) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![
            file_id,
            ord as i64,
            heading.level,
            heading.text,
            heading.offset as i64
        ])?;
    }
    for (ord, link) in analysis.links.iter().enumerate() {
        tx.prepare_cached(
            "INSERT INTO index_links
                 (file_id, ord, wiki, target, target_key, leaf_key, heading, range_start, range_end)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?
        .execute(params![
            file_id,
            ord as i64,
            link.wiki,
            link.target,
            link.target_key,
            link.leaf_key,
            link.heading,
            link.range.start as i64,
            link.range.end as i64
        ])?;
    }
    Ok(file.analysis.is_some())
}

fn count_files(conn: &Connection, root_id: i64) -> Result<usize, StoreError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM index_files WHERE root_id = ?1",
        [root_id],
        |row| row.get(0),
    )?;
    Ok(count as usize)
}

fn file_keys(rel_key: &str) -> (String, String, String) {
    let name = rel_key.rsplit('\\').next().unwrap_or(rel_key);
    (
        file_stem(name).to_owned(),
        name.to_owned(),
        strip_markdown_extension(rel_key).to_owned(),
    )
}

impl Store {
    /// Build or refresh the index of the workspace `root`.
    ///
    /// Files whose size and modification time are unchanged are not re-read; files that
    /// disappeared (or are now ignored / hidden / beyond the limit) are dropped. Files are read
    /// and parsed outside any transaction and written in short ones, so the rest of the
    /// application keeps writing to the database during a run. A failure or cancellation can
    /// leave the files written so far in the index; every file is stored whole and the next run
    /// completes the rest.
    ///
    /// # Errors
    /// [`StoreError::Cancelled`] when `cancel` was set, [`StoreError::NonUnicodePath`] and
    /// [`StoreError::Sqlite`].
    pub fn index_workspace(
        &mut self,
        root: &Path,
        opts: &IndexOptions,
        is_placeholder: &dyn Fn(&Path) -> bool,
        cancel: &AtomicBool,
        now: UnixSeconds,
    ) -> Result<IndexStats, StoreError> {
        let (root_text, root_key) = root_identity(root)?;
        let (found, truncated) = walk(root, &root_text, opts, cancel)?;
        let existing = stored_files(&self.conn, &root_key)?;
        let mut stats = IndexStats {
            truncated,
            ..IndexStats::default()
        };
        let mut seen = HashSet::new();
        for chunk in found.chunks(WRITE_CHUNK) {
            let mut changed = Vec::new();
            for file in chunk {
                if cancel.load(Ordering::Relaxed) {
                    return Err(StoreError::Cancelled);
                }
                let rel_key = key_of(&file.rel);
                let placeholder = is_placeholder(&file.path);
                stats.placeholders += usize::from(placeholder);
                let unchanged = existing
                    .get(&rel_key)
                    .is_some_and(|state| *state == (file.size, file.mtime, placeholder));
                if !unchanged {
                    changed.push(prepare(file.clone(), placeholder));
                }
                seen.insert(rel_key);
            }
            if !changed.is_empty() {
                stats.parsed += self.index_store(&root_key, &root_text, now, &changed)?;
            }
        }
        let root_id = upsert_root(&self.conn, &root_key, &root_text, now)?;
        let gone: Vec<&String> = existing.keys().filter(|k| !seen.contains(*k)).collect();
        for keys in gone.chunks(WRITE_CHUNK) {
            stats.removed += self.in_transaction(|tx| {
                let mut removed = 0;
                for key in keys {
                    removed += remove_path(tx, root_id, key, false)?;
                }
                Ok(removed)
            })?;
        }
        // The count reads the table the statements above wrote, so it cannot fail where they
        // succeeded; a failure would simply travel on.
        count_files(&self.conn, root_id).map(|files| IndexStats { files, ..stats })
    }

    /// Write `files` and the row of their root in one transaction; returns how many were
    /// parsed.
    fn index_store(
        &mut self,
        root_key: &str,
        root_text: &str,
        now: UnixSeconds,
        files: &[Prepared],
    ) -> Result<usize, StoreError> {
        self.in_transaction(|tx| {
            let root_id = upsert_root(tx, root_key, root_text, now)?;
            let mut parsed = 0;
            for file in files {
                parsed += usize::from(store_file(tx, root_id, file)?);
            }
            Ok(parsed)
        })
    }

    /// Apply file-system events to the index of `root` (see [`IndexChange`]). Does nothing
    /// when `root` has not been indexed yet. Files are read and parsed outside any transaction
    /// and written in short ones, like [`Store::index_workspace`] does.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] and [`StoreError::Sqlite`].
    pub fn index_apply_changes(
        &mut self,
        root: &Path,
        changes: &[IndexChange],
        opts: &IndexOptions,
        is_placeholder: &dyn Fn(&Path) -> bool,
    ) -> Result<IndexStats, StoreError> {
        let (_, root_key) = root_identity(root)?;
        let Some(found_root) = find_root(&self.conn, &root_key)? else {
            return Ok(IndexStats::default());
        };
        let root_id = found_root.id;
        let mut stats = IndexStats::default();
        let mut count = count_files(&self.conn, root_id)?;
        let mut steps = Vec::new();
        for change in changes {
            if steps.len() >= WRITE_CHUNK {
                self.index_write(root_id, opts, &mut steps, &mut count, &mut stats)?;
            }
            let (path, removed) = match change {
                IndexChange::Changed(path) => (path, false),
                IndexChange::Removed(path) => (path, true),
            };
            let Some(rel) = relative_path(&found_root.text, path_text(path)?) else {
                continue;
            };
            let metadata = match std::fs::metadata(path) {
                Ok(metadata) if !removed => metadata,
                _ => {
                    steps.push(Step::Remove {
                        key: key_of(&rel),
                        children: true,
                    });
                    continue;
                }
            };
            let mut candidates = Vec::new();
            if metadata.is_dir() {
                for entry in WalkBuilder::new(path)
                    .standard_filters(false)
                    .build()
                    .flatten()
                {
                    if entry.file_type().is_some_and(|kind| kind.is_file()) {
                        candidates.extend(found_from(&found_root.text, &entry));
                    }
                }
            } else {
                candidates.push(Found {
                    rel: rel.clone(),
                    path: path.clone(),
                    size: metadata.len() as i64,
                    mtime: modified_millis(&metadata),
                });
            }
            for candidate in candidates {
                if steps.len() >= WRITE_CHUNK {
                    self.index_write(root_id, opts, &mut steps, &mut count, &mut stats)?;
                }
                if is_included(Path::new(&found_root.text), &candidate.rel, opts) {
                    let placeholder = is_placeholder(&candidate.path);
                    steps.push(Step::Store(prepare(candidate, placeholder)));
                } else {
                    steps.push(Step::Remove {
                        key: key_of(&candidate.rel),
                        children: false,
                    });
                }
            }
        }
        self.index_write(root_id, opts, &mut steps, &mut count, &mut stats)?;
        // The first count above read this table already, so this one cannot fail; a failure
        // would simply travel on.
        count_files(&self.conn, root_id).map(|files| IndexStats { files, ..stats })
    }

    /// Carry out and clear `steps` in one transaction. `count` is the number of files in the
    /// index, kept up to date for the limit of [`IndexOptions::max_files`].
    fn index_write(
        &mut self,
        root_id: i64,
        opts: &IndexOptions,
        steps: &mut Vec<Step>,
        count: &mut usize,
        stats: &mut IndexStats,
    ) -> Result<(), StoreError> {
        self.in_transaction(|tx| {
            for step in steps.drain(..) {
                let file = match step {
                    Step::Remove { key, children } => {
                        stats.removed += remove_path(tx, root_id, &key, children)?;
                        continue;
                    }
                    Step::Store(file) => file,
                };
                let known: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM index_files WHERE root_id = ?1 AND rel_key = ?2)",
                    params![root_id, key_of(&file.found.rel)],
                    |row| row.get(0),
                )?;
                if !known && *count >= opts.max_files {
                    stats.truncated = true;
                    continue;
                }
                *count += usize::from(!known);
                stats.placeholders += usize::from(file.placeholder);
                if store_file(tx, root_id, &file)? {
                    stats.parsed += 1;
                }
            }
            Ok(())
        })
    }

    /// Drop the index of `root`.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] and [`StoreError::Sqlite`].
    pub fn index_forget(&self, root: &Path) -> Result<(), StoreError> {
        let (_, root_key) = root_identity(root)?;
        self.conn
            .execute("DELETE FROM index_roots WHERE key = ?1", [root_key])?;
        Ok(())
    }

    /// Every indexed file of `root` ordered by path (for Quick Open). Empty for an unknown root.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] and [`StoreError::Sqlite`].
    pub fn index_files(&self, root: &Path) -> Result<Vec<IndexedFile>, StoreError> {
        let (_, root_key) = root_identity(root)?;
        let Some(found_root) = find_root(&self.conn, &root_key)? else {
            return Ok(Vec::new());
        };
        let rows: Vec<(String, i64, i64, bool, bool)> = query_rows(
            &self.conn,
            "SELECT rel, size, mtime, markdown, placeholder FROM index_files
             WHERE root_id = ?1 ORDER BY rel_key",
            [found_root.id],
        )?;
        Ok(rows
            .into_iter()
            .map(
                |(rel, size, modified_ms, markdown, placeholder)| IndexedFile {
                    path: PathBuf::from(format!("{}\\{}", found_root.text, rel.replace('/', "\\"))),
                    rel,
                    size: size as u64,
                    modified_ms,
                    markdown,
                    placeholder,
                },
            )
            .collect())
    }

    /// Headings of `file` in document order. Empty when the file is unknown or not parsed.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] and [`StoreError::Sqlite`].
    pub fn index_headings(
        &self,
        root: &Path,
        file: &Path,
    ) -> Result<Vec<IndexedHeading>, StoreError> {
        let (_, root_key) = root_identity(root)?;
        let Some(found_root) = find_root(&self.conn, &root_key)? else {
            return Ok(Vec::new());
        };
        let Some(rel) = relative_path(&found_root.text, path_text(file)?) else {
            return Ok(Vec::new());
        };
        let rows: Vec<(u8, String, i64)> = query_rows(
            &self.conn,
            "SELECT h.level, h.text, h.pos FROM index_headings h
             JOIN index_files f ON f.id = h.file_id
             WHERE f.root_id = ?1 AND f.rel_key = ?2 ORDER BY h.ord",
            params![found_root.id, key_of(&rel)],
        )?;
        Ok(rows
            .into_iter()
            .map(|(level, text, offset)| IndexedHeading {
                level,
                text,
                offset: offset as usize,
            })
            .collect())
    }

    /// Every link site in `root` that points at `target`: wikilinks, relative Markdown links,
    /// reference definitions and images (for rename updates). Sorted by source path and
    /// position.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] and [`StoreError::Sqlite`].
    pub fn index_links_to(&self, root: &Path, target: &Path) -> Result<Vec<LinkSite>, StoreError> {
        let (_, root_key) = root_identity(root)?;
        let Some(found_root) = find_root(&self.conn, &root_key)? else {
            return Ok(Vec::new());
        };
        let Some(rel) = relative_path(&found_root.text, path_text(target)?) else {
            return Ok(Vec::new());
        };
        let target_key = key_of(&rel);
        let (stem_key, name_key, wiki_key) = file_keys(&target_key);
        // (rel, wiki, target, target_key, heading, range_start, range_end)
        type LinkRow = (String, bool, String, String, Option<String>, i64, i64);
        let rows: Vec<LinkRow> = query_rows(
            &self.conn,
            "SELECT f.rel, l.wiki, l.target, l.target_key, l.heading, l.range_start, l.range_end
             FROM index_links l JOIN index_files f ON f.id = l.file_id
             WHERE f.root_id = ?1 AND l.leaf_key IN (?2, ?3)
             ORDER BY f.rel_key, l.ord",
            params![found_root.id, stem_key, name_key],
        )?;
        let wiki_suffix = format!("\\{wiki_key}");
        Ok(rows
            .into_iter()
            .filter(|(_, wiki, _, link_key, ..)| {
                if *wiki {
                    *link_key == wiki_key || wiki_suffix.ends_with(&format!("\\{link_key}"))
                } else {
                    *link_key == target_key
                }
            })
            .map(|(rel, wiki, target, _, heading, start, end)| LinkSite {
                source: PathBuf::from(format!("{}\\{}", found_root.text, rel.replace('/', "\\"))),
                wiki,
                target,
                heading,
                range: TextRange::new(start as usize, end as usize),
            })
            .collect())
    }

    /// The files that link to `file`, without duplicates and without `file` itself, sorted by
    /// path (the Inspector's backlinks).
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] and [`StoreError::Sqlite`].
    pub fn index_backlinks(&self, root: &Path, file: &Path) -> Result<Vec<PathBuf>, StoreError> {
        let own = key_of(path_text(file)?);
        let mut sources: Vec<PathBuf> = Vec::new();
        for site in self.index_links_to(root, file)? {
            let is_own = path_text(&site.source).is_ok_and(|text| key_of(text) == own);
            if !is_own && !sources.contains(&site.source) {
                sources.push(site.source);
            }
        }
        Ok(sources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_what_a_reader_sees_of_every_kind_of_inline() {
        // (Markdown paragraph, what a reader reads). Headings are ATX lines, so line breaks
        // cannot occur in them; the paragraphs here are the way to reach every kind.
        let cases = [
            ("plain", "plain"),
            ("a\\*b", "a*b"),
            ("x $m$ y", "x m y"),
            ("see [[Note]] now", "see Note now"),
            ("1 &amp; 2", "1 & 2"),
            ("`code` here", "code here"),
            ("one\ntwo", "one two"),
            ("one  \ntwo", "one two"),
            ("one\\\ntwo", "one two"),
            ("**s** *e* ~u~ ~~x~~ ==h==", "s e u x h"),
            ("a [l *m*](n.md) b", "a l m b"),
            ("a![o](p.png)b", "ab"),
            ("a[^1]b\n\n[^1]: note\n", "ab"),
            ("a<s>b", "ab"),
        ];
        for (src, expected) in cases {
            let document = parse_document(src);
            let block = document.iter_blocks().next().unwrap();
            let mut out = String::new();
            plain_text(&block.inlines, src, &mut out);
            assert_eq!(out, expected, "{src:?}");
        }
    }
}
