//! RTF → Markdown.
//!
//! A tokenizer feeds an interpreter that keeps a stack of group states (character formatting,
//! paragraph properties, destination). Text is decoded through the active code page
//! (`\ansicpg`, per-font `\fcharset`, `\'hh`) or `\uN` with `\uc` fallback skipping (lone
//! surrogates become U+FFFD). Unknown starred destinations are skipped. The reader never
//! panics on malformed input: unbalanced groups, truncated escapes and absurd nesting are
//! tolerated.

use std::collections::HashMap;

use crate::doc::md::flank;
use crate::import::{Asset, ImportError, ImportResult, LossKind};

/// Group nesting beyond this depth is ignored (the groups are still balanced).
const MAX_DEPTH: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dest {
    Body,
    Skip,
    FontTable,
    Stylesheet,
    Pict,
    Footnote,
    FldInst,
    ListTable,
    ListOverrides,
    PnText,
}

#[derive(Debug, Clone)]
struct State {
    dest: Dest,
    bold: bool,
    italic: bool,
    ul: bool,
    strike: bool,
    hidden: bool,
    uc: usize,
    font: i32,
    group_start: bool,
    star: bool,
    outline: Option<u8>,
    style: Option<i32>,
    ls: Option<i32>,
    ilvl: usize,
    intbl: bool,
    /// Destination of the HYPERLINK field whose result this group is part of.
    link: Option<String>,
}

impl State {
    fn root() -> Self {
        State {
            dest: Dest::Body,
            bold: false,
            italic: false,
            ul: false,
            strike: false,
            hidden: false,
            uc: 1,
            font: -1,
            group_start: false,
            star: false,
            outline: None,
            style: None,
            ls: None,
            ilvl: 0,
            intbl: false,
            link: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    bold: bool,
    italic: bool,
    ul: bool,
    strike: bool,
    link: Option<String>,
    raw: bool,
    text: String,
}

enum Token<'a> {
    Open,
    Close,
    Word(&'a str, Option<i32>),
    Byte(u8),
    Symbol(char),
    Star,
    Text(&'a [u8]),
}

struct Tokenizer<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Tokenizer<'a> {
    fn next_token(&mut self) -> Option<Token<'a>> {
        loop {
            let b = *self.data.get(self.pos)?;
            match b {
                b'\r' | b'\n' => self.pos += 1,
                b'{' => {
                    self.pos += 1;
                    return Some(Token::Open);
                }
                b'}' => {
                    self.pos += 1;
                    return Some(Token::Close);
                }
                b'\\' => return Some(self.control()),
                _ => {
                    let start = self.pos;
                    while let Some(&c) = self.data.get(self.pos) {
                        if matches!(c, b'{' | b'}' | b'\\' | b'\r' | b'\n') {
                            break;
                        }
                        self.pos += 1;
                    }
                    return Some(Token::Text(&self.data[start..self.pos]));
                }
            }
        }
    }

    fn control(&mut self) -> Token<'a> {
        self.pos += 1;
        let Some(&c) = self.data.get(self.pos) else {
            return Token::Text(&[]);
        };
        if c.is_ascii_alphabetic() {
            let start = self.pos;
            while self.data.get(self.pos).is_some_and(u8::is_ascii_alphabetic) {
                self.pos += 1;
            }
            let name = std::str::from_utf8(&self.data[start..self.pos]).unwrap_or("");
            let mut param = None;
            let neg = self.data.get(self.pos) == Some(&b'-');
            let digits_at = self.pos + usize::from(neg);
            let mut end = digits_at;
            while self.data.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if end > digits_at {
                let text = std::str::from_utf8(&self.data[digits_at..end]).unwrap_or("0");
                let value = text
                    .parse::<i64>()
                    .unwrap_or(0)
                    .clamp(0, i64::from(i32::MAX)) as i32;
                param = Some(if neg { -value } else { value });
                self.pos = end;
            }
            if self.data.get(self.pos) == Some(&b' ') {
                self.pos += 1;
            }
            return Token::Word(name, param);
        }
        self.pos += 1;
        match c {
            b'\'' => {
                let hi = self.data.get(self.pos).copied().and_then(hex_value);
                let lo = self.data.get(self.pos + 1).copied().and_then(hex_value);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        self.pos += 2;
                        Token::Byte(h * 16 + l)
                    }
                    _ => Token::Text(&[]),
                }
            }
            b'*' => Token::Star,
            b'~' => Token::Symbol('\u{a0}'),
            b'-' => Token::Symbol('\u{ad}'),
            b'_' => Token::Symbol('\u{2011}'),
            b'\\' | b'{' | b'}' => Token::Symbol(char::from(c)),
            b'\r' | b'\n' => Token::Word("par", None),
            _ => Token::Text(&[]),
        }
    }
}

fn hex_value(b: u8) -> Option<u8> {
    char::from(b).to_digit(16).map(|d| d as u8)
}

fn codepage_for_charset(charset: i32) -> Option<u32> {
    Some(match charset {
        77 => 10000,
        128 => 932,
        129 => 949,
        134 => 936,
        136 => 950,
        161 => 1253,
        162 => 1254,
        163 => 1258,
        177 => 1255,
        178 => 1256,
        186 => 1257,
        204 => 1251,
        222 => 874,
        238 => 1250,
        _ => return None,
    })
}

fn decode_bytes(codepage: u32, bytes: &[u8]) -> String {
    let label = match codepage {
        874 => "windows-874",
        932 => "shift_jis",
        936 => "gbk",
        949 => "euc-kr",
        950 => "big5",
        1250 => "windows-1250",
        1251 => "windows-1251",
        1253 => "windows-1253",
        1254 => "windows-1254",
        1255 => "windows-1255",
        1256 => "windows-1256",
        1257 => "windows-1257",
        1258 => "windows-1258",
        10000 => "macintosh",
        65001 => "utf-8",
        _ => "windows-1252",
    };
    let enc =
        encoding_rs::Encoding::for_label(label.as_bytes()).unwrap_or(encoding_rs::WINDOWS_1252);
    enc.decode_without_bom_handling(bytes).0.into_owned()
}

/// Escapes Markdown-significant characters in plain text.
fn escape_md(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '`' | '[' | ']' | '<' | '>' | '~' | '$' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Escapes a leading character that would start a block construct.
fn escape_line_start(text: &str) -> String {
    let trimmed = text.trim_start();
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    let after_digits = trimmed[digits..].chars().next();
    if trimmed.starts_with('#')
        || trimmed.starts_with("- ")
        || trimmed.starts_with("+ ")
        || trimmed.starts_with("\\>")
    {
        return format!("\\{trimmed}");
    }
    if digits > 0 && matches!(after_digits, Some('.' | ')')) {
        return format!("{}\\{}", &trimmed[..digits], &trimmed[digits..]);
    }
    text.to_string()
}

#[derive(Debug, Default, Clone)]
struct ListLevels {
    /// `\levelnfc` of each level (23 is a bullet).
    nfc: Vec<Option<i32>>,
}

#[derive(Debug)]
enum Block {
    Heading(u8, String),
    Para(String),
    Item {
        level: usize,
        marker: String,
        text: String,
    },
    Table(Vec<Vec<String>>),
    PageBreak,
}

struct Reader {
    stack: Vec<State>,
    overflow: usize,
    result: ImportResult,
    ansi_cp: u32,
    font_charset: HashMap<i32, i32>,
    font_def: i32,
    styles: HashMap<i32, String>,
    style_def: Option<i32>,
    style_name: String,
    lists: HashMap<i32, ListLevels>,
    cur_levels: ListLevels,
    list_of_ls: HashMap<i32, i32>,
    cur_override_list: Option<i32>,
    runs: Vec<Run>,
    saved_runs: Vec<Vec<Run>>,
    pending_bytes: Vec<u8>,
    pending_codepage: u32,
    pending_high: Option<u16>,
    skip: usize,
    pn_text: String,
    fldinst: String,
    /// Values (0-15) of the hex digits of the picture data seen so far.
    pict_nibbles: Vec<u8>,
    pict_kind: Option<&'static str>,
    pict_unsupported: bool,
    image_count: usize,
    footnotes: Vec<String>,
    blocks: Vec<Block>,
    cells: Vec<String>,
    rows: Vec<Vec<String>>,
    cell_text: String,
    /// The current row is a header row (`\\trhdr`).
    row_header: bool,
    counters: Vec<u32>,
    /// `\ls` the counters belong to; another list starts numbering again.
    counters_ls: Option<i32>,
    subsup: usize,
}

impl Reader {
    fn new() -> Self {
        Reader {
            stack: vec![State::root()],
            overflow: 0,
            result: ImportResult::default(),
            ansi_cp: 1252,
            font_charset: HashMap::new(),
            font_def: -1,
            styles: HashMap::new(),
            style_def: None,
            style_name: String::new(),
            lists: HashMap::new(),
            cur_levels: ListLevels::default(),
            list_of_ls: HashMap::new(),
            cur_override_list: None,
            runs: Vec::new(),
            saved_runs: Vec::new(),
            pending_bytes: Vec::new(),
            pending_codepage: 1252,
            pending_high: None,
            skip: 0,
            pn_text: String::new(),
            fldinst: String::new(),
            pict_nibbles: Vec::new(),
            pict_kind: None,
            pict_unsupported: false,
            image_count: 0,
            footnotes: Vec::new(),
            blocks: Vec::new(),
            cells: Vec::new(),
            rows: Vec::new(),
            cell_text: String::new(),
            row_header: false,
            counters: Vec::new(),
            counters_ls: None,
            subsup: 0,
        }
    }

    fn state(&mut self) -> &mut State {
        // The stack always holds the root state: `pop` is guarded by `len() > 1`.
        let last = self.stack.len() - 1;
        &mut self.stack[last]
    }

    fn cur(&self) -> &State {
        &self.stack[self.stack.len() - 1]
    }

    fn run(mut self, data: &[u8]) -> ImportResult {
        let mut tok = Tokenizer { data, pos: 0 };
        while let Some(t) = tok.next_token() {
            match t {
                Token::Open => self.open(),
                Token::Close => self.close(),
                Token::Word(name, param) => {
                    self.flush_bytes();
                    self.skip = 0;
                    self.word(name, param);
                }
                Token::Byte(b) => self.byte(b),
                Token::Symbol(c) => {
                    self.flush_bytes();
                    self.state().group_start = false;
                    self.emit_char(c);
                }
                Token::Star => {
                    self.flush_bytes();
                    self.state().star = true;
                }
                Token::Text(bytes) => self.text(bytes),
            }
        }
        self.finish()
    }

    fn open(&mut self) {
        self.flush_bytes();
        self.skip = 0;
        if self.stack.len() >= MAX_DEPTH {
            self.overflow += 1;
            return;
        }
        let mut next = self.cur().clone();
        next.group_start = true;
        next.star = false;
        self.stack.push(next);
    }

    fn close(&mut self) {
        self.flush_bytes();
        self.skip = 0;
        if self.overflow > 0 {
            self.overflow -= 1;
            return;
        }
        if self.stack.len() <= 1 {
            return;
        }
        let popped = self.stack.pop().map(|s| s.dest);
        let parent = self.cur().dest;
        if let Some(dest) = popped
            && dest != parent
        {
            self.leave(dest);
        }
    }

    fn leave(&mut self, dest: Dest) {
        match dest {
            Dest::Pict => self.finish_pict(),
            Dest::Footnote => {
                let inner = std::mem::take(&mut self.runs);
                self.runs = self.saved_runs.pop().unwrap_or_default();
                self.footnotes
                    .push(render_runs(&inner, false).trim().replace('\n', " "));
                let n = self.footnotes.len();
                self.push_run(format!("[^{n}]"), true);
            }
            Dest::FldInst => {
                // The enclosing `\field` group is current again; its result inherits the link.
                let instr = self.fldinst.trim().to_string();
                let link = parse_hyperlink(&instr);
                if link.is_none() && !instr.is_empty() {
                    self.result.record_loss(LossKind::Fields, 1, None);
                }
                self.state().link = link;
            }
            _ => {}
        }
    }

    fn finish_pict(&mut self) {
        let nibbles = std::mem::take(&mut self.pict_nibbles);
        let kind = self.pict_kind.take();
        let unsupported = std::mem::take(&mut self.pict_unsupported);
        // A trailing odd digit is dropped.
        let bytes: Vec<u8> = nibbles
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| pair[0] * 16 + pair[1])
            .collect();
        match kind {
            Some(ext) if !bytes.is_empty() => {
                self.image_count += 1;
                let path = format!("assets/image-{}.{ext}", self.image_count);
                self.result.assets.push(Asset {
                    relative_path: path.clone(),
                    bytes,
                });
                self.push_run(format!("![]({path})"), true);
            }
            _ => {
                if unsupported || !nibbles.is_empty() {
                    self.result
                        .record_loss(LossKind::UnsupportedImages, 1, None);
                }
            }
        }
    }

    fn word(&mut self, name: &str, param: Option<i32>) {
        let first = std::mem::take(&mut self.state().group_start);
        let star = self.cur().star;
        if first {
            if let Some(dest) = self.dest_for(name) {
                self.state().dest = dest;
                self.enter(name);
            } else if star {
                self.state().dest = Dest::Skip;
            }
        }
        match self.cur().dest {
            Dest::FontTable => match name {
                "f" => self.font_def = param.unwrap_or(-1),
                "fcharset" => {
                    self.font_charset.insert(self.font_def, param.unwrap_or(0));
                }
                _ => {}
            },
            Dest::Stylesheet => {
                if name == "s" {
                    self.style_def = param;
                }
            }
            Dest::ListTable => match name {
                "list" => self.cur_levels = ListLevels::default(),
                "listlevel" => self.cur_levels.nfc.push(None),
                "levelnfc" => {
                    if let Some(last) = self.cur_levels.nfc.last_mut() {
                        *last = param;
                    }
                }
                "listid" => {
                    if let Some(id) = param {
                        self.lists.insert(id, std::mem::take(&mut self.cur_levels));
                    }
                }
                _ => {}
            },
            Dest::ListOverrides => match name {
                "listoverride" => self.cur_override_list = None,
                "listid" => self.cur_override_list = param,
                "ls" => {
                    if let (Some(ls), Some(list)) = (param, self.cur_override_list) {
                        self.list_of_ls.insert(ls, list);
                    }
                }
                _ => {}
            },
            Dest::Pict => match name {
                "pngblip" => self.pict_kind = Some("png"),
                "jpegblip" => self.pict_kind = Some("jpg"),
                "emfblip" | "wmetafile" | "macpict" | "dibitmap" | "wbitmap" => {
                    self.pict_unsupported = true
                }
                _ => {}
            },
            Dest::Skip | Dest::FldInst | Dest::PnText => {}
            Dest::Body | Dest::Footnote => {}
        }
        if self.cur().dest == Dest::Skip {
            return;
        }
        self.format_word(name, param);
    }

    fn dest_for(&self, name: &str) -> Option<Dest> {
        Some(match name {
            "fonttbl" => Dest::FontTable,
            "stylesheet" => Dest::Stylesheet,
            "pict" => Dest::Pict,
            "footnote" => Dest::Footnote,
            "fldinst" => Dest::FldInst,
            "listtable" => Dest::ListTable,
            "listoverridetable" => Dest::ListOverrides,
            "pntext" | "listtext" => Dest::PnText,
            "colortbl" | "info" | "header" | "footer" | "headerl" | "headerr" | "headerf"
            | "footerl" | "footerr" | "footerf" | "nonshppict" | "shp" | "annotation"
            | "object" | "xe" | "tc" | "txe" | "pnseclvl" | "private" | "themedata"
            | "datastore" | "latentstyles" | "rsidtbl" | "generator" | "xmlnstbl"
            | "colorschememapping" | "mmathpr" | "fldtype" | "bkmkstart" | "bkmkend" | "atnid"
            | "atnauthor" | "atnicn" | "atntime" | "atrfstart" | "atrfend" | "pn" | "falt"
            | "panose" | "fname" | "fontemb" | "ftnsep" | "ftnsepc" | "aftnsep" | "aftnsepc" => {
                Dest::Skip
            }
            _ => return None,
        })
    }

    fn enter(&mut self, name: &str) {
        match name {
            "header" | "footer" | "headerl" | "headerr" | "headerf" | "footerl" | "footerr"
            | "footerf" => {
                self.result.record_loss(LossKind::HeadersFooters, 1, None);
            }
            "annotation" => self.result.record_loss(LossKind::Comments, 1, None),
            "shp" => self.result.record_loss(LossKind::VectorShapes, 1, None),
            "footnote" => {
                self.saved_runs.push(std::mem::take(&mut self.runs));
            }
            "fldinst" => self.fldinst.clear(),
            "pict" => {
                self.pict_nibbles.clear();
                self.pict_kind = None;
                self.pict_unsupported = false;
            }
            "pntext" | "listtext" => self.pn_text.clear(),
            "listtable" => self.lists.clear(),
            "listoverridetable" => self.list_of_ls.clear(),
            _ => {}
        }
    }

    fn format_word(&mut self, name: &str, param: Option<i32>) {
        let on = param != Some(0);
        match name {
            "ansicpg" => self.ansi_cp = param.map_or(1252, |p| p.max(0) as u32),
            "uc" => self.state().uc = param.map_or(1, |p| p.max(0) as usize),
            "b" => self.state().bold = on,
            "i" => self.state().italic = on,
            "ul" | "uld" | "uldb" | "ulw" | "ulth" | "ulwave" => self.state().ul = on,
            "ulnone" => self.state().ul = false,
            "strike" | "striked" => self.state().strike = on,
            "v" => {
                if on {
                    self.result.record_loss(LossKind::HiddenText, 1, None);
                }
                self.state().hidden = on;
            }
            "super" | "sub" => {
                if on {
                    self.subsup += 1;
                }
            }
            "plain" => {
                let s = self.state();
                s.bold = false;
                s.italic = false;
                s.ul = false;
                s.strike = false;
                s.hidden = false;
            }
            "f" => {
                if let Some(p) = param {
                    self.state().font = p;
                }
            }
            "pard" => {
                let s = self.state();
                s.outline = None;
                s.style = None;
                s.ls = None;
                s.ilvl = 0;
                s.intbl = false;
            }
            "s" => self.state().style = param,
            "outlinelevel" => self.state().outline = param.and_then(|p| u8::try_from(p).ok()),
            "ls" => self.state().ls = param,
            "ilvl" => self.state().ilvl = param.map_or(0, |p| p.max(0) as usize),
            "intbl" => self.state().intbl = true,
            "itap" => {
                if param.is_some_and(|p| p > 1) {
                    self.result.record_loss(LossKind::NestedTables, 1, None);
                }
            }
            "clmgf" | "clmrg" | "clvmgf" | "clvmrg" => {
                self.result.record_loss(LossKind::MergedCells, 1, None)
            }
            "cols" => {
                if param.is_some_and(|p| p > 1) {
                    self.result.record_loss(LossKind::Columns, 1, None);
                }
            }
            "par" => self.end_paragraph(),
            "trowd" => self.row_header = false,
            "trhdr" => self.row_header = true,
            "cell" | "nestcell" => self.end_cell(),
            "row" | "nestrow" => self.end_row(),
            "line" => self.push_run("\n".to_string(), true),
            "tab" => self.emit_char(' '),
            "page" => {
                self.end_paragraph();
                self.flush_table();
                self.blocks.push(Block::PageBreak);
            }
            "emdash" => self.emit_char('\u{2014}'),
            "endash" => self.emit_char('\u{2013}'),
            "bullet" => self.emit_char('\u{2022}'),
            "lquote" => self.emit_char('\u{2018}'),
            "rquote" => self.emit_char('\u{2019}'),
            "ldblquote" => self.emit_char('\u{201c}'),
            "rdblquote" => self.emit_char('\u{201d}'),
            "u" => {
                if let Some(p) = param {
                    self.unicode(p);
                }
            }
            _ => {}
        }
    }

    fn unicode(&mut self, p: i32) {
        let unit = (p as i64).rem_euclid(65536) as u16;
        self.skip = self.cur().uc;
        match (self.pending_high.take(), unit) {
            (Some(high), 0xDC00..=0xDFFF) => {
                let c = 0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
                self.emit_char(char::from_u32(c).unwrap_or('\u{fffd}'));
            }
            (high, 0xD800..=0xDBFF) => {
                if high.is_some() {
                    self.emit_char('\u{fffd}');
                }
                self.pending_high = Some(unit);
            }
            (high, _) => {
                if high.is_some() {
                    self.emit_char('\u{fffd}');
                }
                let c = if (0xDC00..=0xDFFF).contains(&unit) {
                    '\u{fffd}'
                } else {
                    char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}')
                };
                self.emit_char(c);
            }
        }
    }

    fn byte(&mut self, b: u8) {
        self.state().group_start = false;
        if self.skip > 0 {
            self.skip -= 1;
            return;
        }
        self.byte_raw(b);
    }

    fn codepage(&self) -> u32 {
        self.font_charset
            .get(&self.cur().font)
            .and_then(|cs| codepage_for_charset(*cs))
            .unwrap_or(self.ansi_cp)
    }

    fn flush_bytes(&mut self) {
        if self.pending_bytes.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut self.pending_bytes);
        let text = decode_bytes(self.pending_codepage, &bytes);
        self.emit_str(&text);
    }

    fn text(&mut self, bytes: &[u8]) {
        self.state().group_start = false;
        let mut bytes = bytes;
        if self.skip > 0 {
            let n = self.skip.min(bytes.len());
            bytes = &bytes[n..];
            self.skip -= n;
        }
        if bytes.is_empty() {
            return;
        }
        if bytes.is_ascii() {
            self.flush_bytes();
            if self.pending_high.is_some() {
                self.emit_char('\u{fffd}');
                self.pending_high = None;
            }
            let s = String::from_utf8_lossy(bytes).into_owned();
            self.emit_str(&s);
        } else {
            for &b in bytes {
                self.byte_raw(b);
            }
        }
    }

    fn byte_raw(&mut self, b: u8) {
        let cp = self.codepage();
        if cp != self.pending_codepage {
            self.flush_bytes();
            self.pending_codepage = cp;
        }
        self.pending_bytes.push(b);
    }

    fn emit_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.emit_str(c.encode_utf8(&mut buf));
    }

    fn emit_str(&mut self, s: &str) {
        match self.cur().dest {
            Dest::Body | Dest::Footnote => {
                if self.cur().hidden {
                    return;
                }
                self.push_run(s.to_string(), false);
            }
            Dest::FldInst => self.fldinst.push_str(s),
            Dest::PnText => self.pn_text.push_str(s),
            Dest::Pict => self.pict_nibbles.extend(s.bytes().filter_map(hex_value)),
            Dest::Stylesheet => {
                for c in s.chars() {
                    if c == ';' {
                        if let Some(n) = self.style_def.take() {
                            self.styles.insert(n, self.style_name.trim().to_string());
                        }
                        self.style_name.clear();
                    } else {
                        self.style_name.push(c);
                    }
                }
            }
            Dest::Skip | Dest::FontTable | Dest::ListTable | Dest::ListOverrides => {}
        }
    }

    fn push_run(&mut self, text: String, raw: bool) {
        let s = self.cur();
        let run = Run {
            bold: s.bold,
            italic: s.italic,
            ul: s.ul,
            strike: s.strike,
            link: self.link_for_run(),
            raw,
            text,
        };
        self.runs.push(run);
    }

    /// A field result is a link while its group is open and a HYPERLINK instruction was seen.
    fn link_for_run(&self) -> Option<String> {
        self.cur().link.clone()
    }

    fn end_paragraph(&mut self) {
        self.flush_bytes();
        let state = self.cur().clone();
        if state.dest == Dest::Footnote {
            // Paragraphs of a footnote join into the note's single line.
            if !self.runs.is_empty() {
                self.push_run(" ".to_string(), true);
            }
            return;
        }
        if state.intbl {
            let runs = std::mem::take(&mut self.runs);
            let text = self.cell_text_of(runs);
            if !text.trim().is_empty() {
                if !self.cell_text.is_empty() {
                    self.cell_text.push(' ');
                    self.result
                        .record_loss(LossKind::MultiParagraphCells, 1, None);
                }
                self.cell_text.push_str(text.trim());
            }
            return;
        }
        self.flush_table();
        let runs = std::mem::take(&mut self.runs);
        let pn = std::mem::take(&mut self.pn_text);
        let heading = state
            .style
            .and_then(|s| self.styles.get(&s))
            .and_then(|n| heading_level_from_name(n))
            .or_else(|| state.outline.filter(|o| *o <= 5).map(|o| o + 1));
        let is_list =
            state.ls.is_some_and(|ls| ls > 0 && state.ls != Some(2047)) || !pn.trim().is_empty();
        if heading.is_none() && is_list {
            let text = render_runs(&runs, false);
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            let text = hard_breaks(text);
            let ordered = if !pn.trim().is_empty() {
                pn.trim_start().starts_with(|c: char| c.is_alphanumeric())
            } else {
                self.list_is_ordered(state.ls, state.ilvl)
            };
            let level = state.ilvl.min(8);
            if self.counters_ls != state.ls {
                self.counters.clear();
                self.counters_ls = state.ls;
            }
            self.counters.truncate(level + 1);
            self.counters.resize(level + 1, 0);
            self.counters[level] += 1;
            let marker = if ordered {
                format!("{}.", self.counters[level])
            } else {
                "-".to_string()
            };
            let (marker, text) = split_task(marker, &text);
            self.blocks.push(Block::Item {
                level,
                marker,
                text,
            });
            return;
        }
        self.counters.clear();
        let text = render_runs(&runs, heading.is_some());
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        match heading {
            Some(level) => self
                .blocks
                .push(Block::Heading(level, text.replace('\n', " "))),
            None => {
                let lines: Vec<String> = text.split('\n').map(escape_line_start).collect();
                self.blocks
                    .push(Block::Para(hard_breaks(&lines.join("\n"))));
            }
        }
    }

    fn list_is_ordered(&self, ls: Option<i32>, level: usize) -> bool {
        ls.and_then(|ls| self.list_of_ls.get(&ls))
            .and_then(|id| self.lists.get(id))
            .and_then(|levels| levels.nfc.get(level).copied().flatten())
            .is_some_and(|nfc| nfc != 23 && nfc != 255)
    }

    /// Markdown of one cell paragraph. In a header row, bold that covers the whole cell is the
    /// header styling (a Markdown header is bold by itself), so it is not written as `**…**`.
    fn cell_text_of(&self, mut runs: Vec<Run>) -> String {
        if self.row_header
            && runs
                .iter()
                .filter(|r| !r.text.trim().is_empty())
                .all(|r| r.bold)
        {
            for run in &mut runs {
                run.bold = false;
            }
        }
        render_runs(&runs, false).replace('\n', "<br>")
    }

    fn end_cell(&mut self) {
        self.flush_bytes();
        let runs = std::mem::take(&mut self.runs);
        let text = self.cell_text_of(runs);
        if !text.trim().is_empty() {
            if !self.cell_text.is_empty() {
                self.cell_text.push(' ');
                self.result
                    .record_loss(LossKind::MultiParagraphCells, 1, None);
            }
            self.cell_text.push_str(text.trim());
        }
        self.cells.push(std::mem::take(&mut self.cell_text));
    }

    fn end_row(&mut self) {
        self.flush_bytes();
        if !self.runs.is_empty() || !self.cell_text.is_empty() {
            self.end_cell();
        }
        self.row_header = false;
        let cells = std::mem::take(&mut self.cells);
        if !cells.is_empty() {
            self.rows.push(cells);
        }
    }

    fn flush_table(&mut self) {
        if !self.rows.is_empty() {
            let rows = std::mem::take(&mut self.rows);
            self.blocks.push(Block::Table(rows));
        }
    }

    fn finish(mut self) -> ImportResult {
        self.flush_bytes();
        if self.pending_high.take().is_some() {
            self.emit_char('\u{fffd}');
        }
        self.stack.truncate(1);
        // Trailing text without a closing \par is a paragraph unless a table row is pending.
        if !self.cells.is_empty() || !self.cell_text.is_empty() {
            self.end_row();
        }
        self.end_paragraph();
        self.flush_table();
        if self.subsup > 0 {
            let n = self.subsup;
            self.result.record_loss(LossKind::SubSuperscript, n, None);
        }
        let mut out = String::new();
        let mut prev_item = false;
        for block in &self.blocks {
            let text = match block {
                Block::Heading(level, t) => format!("{} {t}", "#".repeat(usize::from(*level))),
                Block::Para(t) => t.clone(),
                Block::Item {
                    level,
                    marker,
                    text,
                } => format!("{}{marker} {text}", "    ".repeat(*level)),
                Block::Table(rows) => render_table(rows),
                Block::PageBreak => "<!-- pagebreak -->".to_string(),
            };
            let is_item = matches!(block, Block::Item { .. });
            if !out.is_empty() {
                out.push_str(if is_item && prev_item { "\n" } else { "\n\n" });
            }
            out.push_str(&text);
            prev_item = is_item;
        }
        for (i, note) in self.footnotes.iter().enumerate() {
            out.push_str(&format!("\n\n[^{}]: {}", i + 1, note));
        }
        if !out.is_empty() {
            out.push('\n');
        }
        self.result.markdown = out;
        self.result
    }
}

/// A line break (`\line`) inside a paragraph is a Markdown hard break: backslash, newline.
fn hard_breaks(text: &str) -> String {
    text.replace('\n', "\\\n")
}

fn split_task(marker: String, text: &str) -> (String, String) {
    for (glyph, box_text) in [
        ('\u{2610}', "[ ] "),
        ('\u{2611}', "[x] "),
        ('\u{2612}', "[x] "),
    ] {
        if let Some(rest) = text.strip_prefix(glyph) {
            return (marker, format!("{box_text}{}", rest.trim_start()));
        }
    }
    (marker, text.to_string())
}

fn heading_level_from_name(name: &str) -> Option<u8> {
    let lower = name.trim().to_ascii_lowercase();
    if lower == "title" {
        return Some(1);
    }
    let digits = lower.strip_prefix("heading ")?;
    digits
        .trim()
        .parse::<u8>()
        .ok()
        .filter(|n| (1..=6).contains(n))
}

fn parse_hyperlink(instr: &str) -> Option<String> {
    let rest = instr.trim_start().strip_prefix("HYPERLINK")?;
    let quoted = rest.split('"').nth(1).map(str::to_string);
    let anchor = rest.contains("\\l ") || rest.trim_start().starts_with("\\l");
    match (quoted, anchor) {
        (Some(q), true) => Some(format!("#{q}")),
        (Some(q), false) => Some(q),
        (None, _) => None,
    }
}

fn render_table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let line = |cells: &[String]| -> String {
        let mut s = String::from("|");
        for i in 0..cols {
            s.push(' ');
            s.push_str(cells.get(i).map_or("", String::as_str));
            s.push_str(" |");
        }
        s
    };
    let mut out = vec![line(&rows[0])];
    out.push(format!("|{}", "---|".repeat(cols)));
    for row in &rows[1..] {
        out.push(line(row));
    }
    out.join("\n")
}

/// Renders runs as Markdown inline text.
fn render_runs(runs: &[Run], heading: bool) -> String {
    let mut merged: Vec<Run> = Vec::new();
    for r in runs {
        match merged.last_mut() {
            Some(last)
                if last.bold == r.bold
                    && last.italic == r.italic
                    && last.ul == r.ul
                    && last.strike == r.strike
                    && last.link == r.link
                    && last.raw == r.raw =>
            {
                last.text.push_str(&r.text);
            }
            _ => merged.push(r.clone()),
        }
    }
    let mut out = String::new();
    let mut i = 0;
    while i < merged.len() {
        let run = &merged[i];
        if let Some(url) = &run.link {
            let mut j = i;
            let mut label = String::new();
            while j < merged.len() && merged[j].link.as_ref() == Some(url) {
                label.push_str(&render_neighbored(&merged, j, heading, true));
                j += 1;
            }
            out.push('[');
            out.push_str(&label);
            out.push_str("](");
            out.push_str(&url.replace(' ', "%20").replace(')', "%29"));
            out.push(')');
            i = j;
        } else {
            out.push_str(&render_neighbored(&merged, i, heading, false));
            i += 1;
        }
    }
    out
}

/// Renders `runs[at]` with the characters of its neighbours in the same link (or outside any
/// link) that touch its marks.
fn render_neighbored(runs: &[Run], at: usize, heading: bool, in_link: bool) -> String {
    let run = &runs[at];
    let touching = |neighbor: Option<&Run>, before: bool| {
        let other = neighbor.filter(|n| n.link == run.link)?;
        if before {
            other.text.chars().next_back()
        } else {
            other.text.chars().next()
        }
    };
    let before = touching(at.checked_sub(1).map(|p| &runs[p]), true);
    let after = touching(runs.get(at + 1), false);
    render_run(run, heading, in_link, before, after)
}

/// Renders one run; `before` and `after` are the text characters next to it.
fn render_run(
    run: &Run,
    heading: bool,
    in_link: bool,
    before: Option<char>,
    after: Option<char>,
) -> String {
    let text = if run.raw {
        run.text.clone()
    } else {
        escape_md(&run.text)
    };
    let trimmed = text.trim();
    if trimmed.is_empty() || run.raw {
        return text;
    }
    let lead = &text[..text.len() - text.trim_start().len()];
    let trail = &text[text.trim_end().len()..];
    let mut open = String::new();
    let mut close = String::new();
    if run.ul && !in_link {
        open.push('~');
        close.insert(0, '~');
    }
    if run.strike {
        open.push_str("~~");
        close.insert_str(0, "~~");
    }
    if run.bold && !heading {
        open.push_str("**");
        close.insert_str(0, "**");
    }
    if run.italic {
        open.push('*');
        close.insert(0, '*');
    }
    if open.is_empty() {
        return text;
    }
    let before = if lead.is_empty() { before } else { None };
    let after = if trail.is_empty() { after } else { None };
    let (pre, inner, post, marked) = flank(trimmed, false, before, after);
    if !marked {
        return text;
    }
    format!("{lead}{pre}{open}{inner}{close}{post}{trail}")
}

/// Imports RTF bytes into Markdown, assets and a loss report.
pub fn import_rtf(data: &[u8]) -> Result<ImportResult, ImportError> {
    let head = &data[..data.len().min(64)];
    let start = head
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(0);
    if !data[start..].starts_with(b"{\\rtf") {
        return Err(ImportError::WrongFormat {
            expected: "RTF",
            detected: "unknown",
        });
    }
    Ok(Reader::new().run(&data[start..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(rtf: &str) -> String {
        import_rtf(rtf.as_bytes()).unwrap().markdown
    }

    #[test]
    fn rejects_non_rtf() {
        for bad in [&b""[..], b"plain text", b"{\\par}", b"PK\x03\x04"] {
            assert_eq!(
                import_rtf(bad),
                Err(ImportError::WrongFormat {
                    expected: "RTF",
                    detected: "unknown"
                })
            );
        }
    }

    #[test]
    fn plain_text_and_escapes() {
        let table = [
            ("{\\rtf1\\ansi Hello\\par World\\par}", "Hello\n\nWorld\n"),
            ("{\\rtf1 a \\{b\\} \\\\ c\\par}", "a {b} \\\\ c\n"),
            ("{\\rtf1 \\'e4\\'f6\\par}", "\u{e4}\u{f6}\n"),
            ("{\\rtf1\\ansicpg1250 \\'bf\\'f3\\par}", "\u{17c}\u{f3}\n"),
            ("{\\rtf1\\ansicpg932 \\'82\\'a0\\par}", "\u{3042}\n"),
            (
                "{\\rtf1 \\u8364?x\\u-10179?\\u-8704?\\par}",
                "\u{20ac}x\u{1f600}\n",
            ),
            ("{\\rtf1\\uc0 \\u-10240 here\\par}", "\u{fffd}here\n"),
            (
                "{\\rtf1\\uc0 \\u-9216\\u-10240 x\\par}",
                "\u{fffd}\u{fffd}x\n",
            ),
            (
                "{\\rtf1 a\\emdash b\\endash c\\bullet d\\lquote\\rquote\\ldblquote\\rdblquote\\~z\\par}",
                "a\u{2014}b\u{2013}c\u{2022}d\u{2018}\u{2019}\u{201c}\u{201d}\u{a0}z\n",
            ),
            (
                "{\\rtf1 1. not a list\\par# not heading\\par- dash\\par}",
                "1\\. not a list\n\n\\# not heading\n\n\\- dash\n",
            ),
            // only digits followed by `.` or `)` start a list in Markdown
            (
                "{\\rtf1 2) paren\\par 2024 was a year\\par}",
                "2\\) paren\n\n2024 was a year\n",
            ),
            // a high surrogate that ends the input is replaced
            ("{\\rtf1\\uc0 \\u-10240}", "\u{fffd}\n"),
            (
                "{\\rtf1 a*b_c`d[e]f<g>h~i$j|k\\par}",
                "a\\*b\\_c\\`d\\[e\\]f\\<g\\>h\\~i\\$j\\|k\n",
            ),
            ("{\\rtf1 line\\line two\\par}", "line\\\ntwo\n"),
            // optional and non-breaking hyphens, a backslash before a line end, an unknown symbol
            (
                "{\\rtf1 a\\-b\\_c\\\nd\\!e\\par}",
                "a\u{ad}b\u{2011}c\n\nde\n",
            ),
            // a high surrogate followed by another high surrogate, or by a BMP character
            (
                "{\\rtf1\\uc0 \\u-10240\\u-10179\\u-8704 x\\par}",
                "\u{fffd}\u{1f600}x\n",
            ),
            ("{\\rtf1\\uc0 \\u-10240\\u65 x\\par}", "\u{fffd}Ax\n"),
        ];
        for (rtf, expected) in table {
            assert_eq!(md(rtf), expected, "{rtf}");
        }
    }

    #[test]
    fn character_formatting() {
        let table = [
            (
                "{\\rtf1 {\\b bold} {\\i it} {\\ul un} {\\strike st}\\par}",
                "**bold** *it* ~un~ ~~st~~\n",
            ),
            ("{\\rtf1 \\b bold \\b0 plain\\par}", "**bold** plain\n"),
            (
                "{\\rtf1 \\b\\i both\\plain  none\\par}",
                "***both*** none\n",
            ),
            (
                "{\\rtf1 {\\b  spaced }x\\par}",
                " **spaced** x\n".trim_start(),
            ),
            ("{\\rtf1 \\ulnone a\\ul b\\ulnone c\\par}", "a~b~c\n"),
            ("{\\rtf1 hide{\\v secret}show\\par}", "hideshow\n"),
        ];
        for (rtf, expected) in table {
            assert_eq!(md(rtf), expected, "{rtf}");
        }
        let r = import_rtf(b"{\\rtf1 hide{\\v secret}x\\par}").unwrap();
        assert!(r.losses.iter().any(|l| l.kind == LossKind::HiddenText));
    }

    #[test]
    fn emphasis_next_to_punctuation_parses_back() {
        use crate::doc::md::probe::{expect, runs};
        // (rtf, markdown, runs of equal emphasis the editor's parser finds in the markdown)
        type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)]);
        let table: [Case; 10] = [
            (
                "{\\rtf1 x{\\b  bold }y\\par}",
                "x **bold** y\n",
                &[("x ", ""), ("bold", "b"), (" y", "")],
            ),
            (
                "{\\rtf1 a{\\b bold.}b\\par}",
                "a**bold**.b\n",
                &[("a", ""), ("bold", "b"), (".b", "")],
            ),
            (
                "{\\rtf1 a{\\b (bold)}b\\par}",
                "a(**bold**)b\n",
                &[("a(", ""), ("bold", "b"), (")b", "")],
            ),
            (
                "{\\rtf1 a{\\b b}c\\par}",
                "a**b**c\n",
                &[("a", ""), ("b", "b"), ("c", "")],
            ),
            (
                "{\\rtf1 a{\\i b}c\\par}",
                "a*b*c\n",
                &[("a", ""), ("b", "i"), ("c", "")],
            ),
            ("{\\rtf1 a{\\b --}b\\par}", "a--b\n", &[("a--b", "")]),
            (
                "{\\rtf1 {\\b Note:}text\\par}",
                "**Note**:text\n",
                &[("Note", "b"), (":text", "")],
            ),
            (
                "{\\rtf1 a{\\b\\i b.}c\\par}",
                "a***b***.c\n",
                &[("a", ""), ("b", "bi"), (".c", "")],
            ),
            // the delimiters of a marked run next to another marked run still see the letter
            (
                "{\\rtf1 {\\b x}{\\i (y)}\\par}",
                "**x**(*y)*\n",
                &[("x", "b"), ("(", ""), ("y)", "i")],
            ),
            // a run inside a link sees only its neighbours in the same link
            (
                "{\\rtf1 {\\field{\\*\\fldinst HYPERLINK \"http://u\"}{\\fldrslt {\\b a.}b}}\\par}",
                "[**a**.b](http://u)\n",
                &[("a", "b"), (".b", "")],
            ),
        ];
        for (rtf, want, spans) in table {
            let markdown = md(rtf);
            assert_eq!(markdown, want, "{rtf}");
            assert_eq!(runs(&markdown), expect(spans), "{rtf}");
        }
    }

    #[test]
    fn font_charsets_map_to_windows_code_pages() {
        let cases = [
            (77, Some(10000)),
            (128, Some(932)),
            (129, Some(949)),
            (134, Some(936)),
            (136, Some(950)),
            (161, Some(1253)),
            (162, Some(1254)),
            (163, Some(1258)),
            (177, Some(1255)),
            (178, Some(1256)),
            (186, Some(1257)),
            (204, Some(1251)),
            (222, Some(874)),
            (238, Some(1250)),
            (0, None),
            (1, None),
        ];
        for (charset, codepage) in cases {
            assert_eq!(codepage_for_charset(charset), codepage, "charset {charset}");
        }
    }

    #[test]
    fn bytes_are_decoded_with_their_code_page() {
        let cases: [(u32, &[u8], &str); 15] = [
            (874, &[0xA1], "\u{e01}"),
            (932, &[0x82, 0xA0], "\u{3042}"),
            (936, &[0xB0, 0xA1], "\u{554a}"),
            (949, &[0xB0, 0xA1], "\u{ac00}"),
            (950, &[0xA4, 0x40], "\u{4e00}"),
            (1250, &[0xBF], "\u{17c}"),
            (1251, &[0xE9], "\u{439}"),
            (1253, &[0xE1], "\u{3b1}"),
            (1254, &[0xFE], "\u{15f}"),
            (1255, &[0xE0], "\u{5d0}"),
            (1256, &[0xC7], "\u{627}"),
            (1257, &[0xE0], "\u{105}"),
            (1258, &[0xD0], "\u{110}"),
            (10000, &[0x80], "\u{c4}"),
            (65001, &[0xC3, 0xA9], "\u{e9}"),
        ];
        for (codepage, bytes, expected) in cases {
            assert_eq!(
                decode_bytes(codepage, bytes),
                expected,
                "code page {codepage}"
            );
        }
        // an unknown code page is read as windows-1252
        assert_eq!(decode_bytes(12345, &[0xE9]), "\u{e9}");
    }

    #[test]
    fn headings_by_outline_level_and_style_names() {
        let rtf = "{\\rtf1{\\stylesheet{\\s1 heading 1;}{\\s2 Title;}{\\s3 Normal;}}\\pard\\s1 One\\par\\pard\\s2 T\\par\\pard\\s3 body\\par\\pard\\outlinelevel2 Third\\par\\pard\\outlinelevel9 x\\par}";
        assert_eq!(md(rtf), "# One\n\n# T\n\nbody\n\n### Third\n\nx\n");
    }

    #[test]
    fn heading_drops_bold_markers() {
        assert_eq!(
            md("{\\rtf1\\pard\\outlinelevel0 {\\b Head}\\par}"),
            "# Head\n"
        );
    }

    #[test]
    fn lists_by_list_table_pntext_and_levels() {
        let rtf = "{\\rtf1{\\*\\listtable{\\list{\\listlevel\\levelnfc23}\\listid1}{\\list{\\listlevel\\levelnfc0}\\listid2}}{\\*\\listoverridetable{\\listoverride\\listid1\\ls1}{\\listoverride\\listid2\\ls2}}\\pard\\ls1\\ilvl0 a\\par\\pard\\ls1\\ilvl1 b\\par\\pard\\ls2\\ilvl0 c\\par\\pard\\ls2\\ilvl0 d\\par\\pard after\\par\\pard{\\pntext\\f0 1.\\tab}x\\par\\pard{\\pntext\\f0\\'b7\\tab}y\\par}";
        assert_eq!(md(rtf), "- a\n    - b\n1. c\n2. d\n\nafter\n\n1. x\n- y\n");
    }

    #[test]
    fn task_glyphs_become_checkboxes() {
        let rtf = "{\\rtf1\\pard{\\pntext\\u9744?\\tab}\\u9744? open\\par\\pard{\\pntext\\u9745?\\tab}\\u9745? done\\par}";
        assert_eq!(md(rtf), "- [ ] open\n- [x] done\n");
    }

    #[test]
    fn header_row_bold_covering_a_whole_cell_is_styling_not_emphasis() {
        let rtf = "{\\rtf1\\trowd\\trhdr\\cellx1000\\cellx2000\\pard\\intbl\\b a\\cell\\b0 plain \\b b\\b0  part\\cell\\row\\trowd\\cellx1000\\cellx2000\\pard\\intbl\\b c\\b0\\cell d\\cell\\row\\pard x\\par}";
        // Header: a fully bold cell loses the markers, a partly bold one keeps them; body rows are untouched.
        assert_eq!(
            md(rtf),
            "| a | plain **b** part |\n|---|---|\n| **c** | d |\n\nx\n"
        );
    }

    #[test]
    fn line_breaks_follow_the_context() {
        let table = [
            // Paragraph: hard break; the next line is protected like a paragraph start.
            (
                "{\\rtf1 a\\line - b\\line 1. c\\par}",
                "a\\\n\\- b\\\n1\\. c\n",
            ),
            // List item.
            (
                "{\\rtf1\\pard{\\pntext 1.\\tab}a\\line b\\par}",
                "1. a\\\nb\n",
            ),
            // Heading: a space.
            ("{\\rtf1\\pard\\outlinelevel0 a\\line b\\par}", "# a b\n"),
            // Table cell: <br>.
            (
                "{\\rtf1\\trowd\\cellx1000\\pard\\intbl a\\line b\\cell\\row}",
                "| a<br>b |\n|---|\n",
            ),
            // Footnote: stays on the note's line.
            (
                "{\\rtf1 x{\\footnote a\\line b}\\par}",
                "x[^1]\n\n[^1]: a b\n",
            ),
            // A break at the very end disappears with the trailing white space.
            ("{\\rtf1 a\\line\\par}", "a\n"),
        ];
        for (rtf, expected) in table {
            assert_eq!(md(rtf), expected, "{rtf}");
        }
    }

    #[test]
    fn text_without_a_closing_par_is_a_paragraph_and_an_unfinished_row_is_a_table() {
        assert_eq!(md("{\\rtf1 tail without par}"), "tail without par\n");
        assert_eq!(
            md("{\\rtf1\\trowd\\cellx1000\\pard\\intbl a\\cell b}"),
            "| a | b |\n|---|---|\n"
        );
    }

    #[test]
    fn listtext_labels_are_not_duplicated_into_the_item_text() {
        let rtf = "{\\rtf1\\pard{\\listtext\\pard\\plain  1.\\tab}\\ilvl0\\ls1 one\\par\\pard{\\listtext\\pard\\plain  2.\\tab}\\ilvl0\\ls1 two\\par\\pard{\\listtext\\pard\\plain \\u8211?\\tab}\\ilvl0\\ls2 dash\\par}";
        assert_eq!(md(rtf), "1. one\n2. two\n- dash\n");
    }

    #[test]
    fn tables_and_losses() {
        let rtf = "{\\rtf1\\trowd\\cellx1000\\cellx2000\\pard\\intbl a\\cell b|c\\cell\\row\\trowd\\clmgf\\cellx1000\\cellx2000\\pard\\intbl d\\par e\\cell \\cell\\row\\pard after\\par}";
        let r = import_rtf(rtf.as_bytes()).unwrap();
        assert_eq!(
            r.markdown,
            "| a | b\\|c |\n|---|---|\n| d e |  |\n\nafter\n"
        );
        let kinds: Vec<LossKind> = r.losses.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&LossKind::MergedCells));
        assert!(kinds.contains(&LossKind::MultiParagraphCells));
    }

    #[test]
    fn hyperlinks_and_other_fields() {
        let rtf = "{\\rtf1 see {\\field{\\*\\fldinst{HYPERLINK \"http://x.org/a b\"}}{\\fldrslt{\\ul text}}} and {\\field{\\*\\fldinst HYPERLINK \\\\l \"bm\"}{\\fldrslt local}} {\\field{\\*\\fldinst PAGE}{\\fldrslt 3}}\\par}";
        let r = import_rtf(rtf.as_bytes()).unwrap();
        assert_eq!(
            r.markdown,
            "see [text](http://x.org/a%20b) and [local](#bm) 3\n"
        );
        assert!(r.losses.iter().any(|l| l.kind == LossKind::Fields));
    }

    #[test]
    fn footnotes_and_annotations() {
        let rtf = "{\\rtf1 text{\\super\\chftn}{\\footnote\\pard {\\super\\chftn} the note\\par} more{\\*\\annotation{\\*\\atnid x}comment body}\\par}";
        let r = import_rtf(rtf.as_bytes()).unwrap();
        assert_eq!(r.markdown, "text[^1] more\n\n[^1]: the note\n");
        assert!(r.losses.iter().any(|l| l.kind == LossKind::Comments));
    }

    #[test]
    fn pictures_become_assets_and_unsupported_ones_are_reported() {
        let rtf = "{\\rtf1 a{\\pict\\pngblip\\picw1 89504E47}b{\\*\\shppict{\\pict\\jpegblip ffd8}}{\\nonshppict{\\pict\\wmetafile8 0102}}c{\\pict\\emfblip 0102}\\par}";
        let r = import_rtf(rtf.as_bytes()).unwrap();
        assert_eq!(
            r.markdown,
            "a![](assets/image-1.png)b![](assets/image-2.jpg)c\n"
        );
        assert_eq!(r.assets[0].bytes, vec![0x89, 0x50, 0x4E, 0x47]);
        assert_eq!(r.assets[1].bytes, vec![0xFF, 0xD8]);
        assert!(
            r.losses
                .iter()
                .any(|l| l.kind == LossKind::UnsupportedImages)
        );
    }

    #[test]
    fn headers_shapes_columns_nested_tables_and_subsuper_are_reported() {
        let rtf = "{\\rtf1\\cols2{\\header head}{\\footer foot}{\\shp{\\*\\shpinst x}}\\pard\\itap2 x{\\super 2}{\\sub 1}\\par}";
        let r = import_rtf(rtf.as_bytes()).unwrap();
        let kinds: Vec<LossKind> = r.sorted_losses().iter().map(|l| l.kind).collect();
        for k in [
            LossKind::HeadersFooters,
            LossKind::VectorShapes,
            LossKind::Columns,
            LossKind::NestedTables,
            LossKind::SubSuperscript,
        ] {
            assert!(kinds.contains(&k), "{k:?}");
        }
        assert_eq!(r.markdown, "x21\n");
    }

    #[test]
    fn page_breaks_and_unknown_destinations() {
        assert_eq!(
            md("{\\rtf1 a\\par\\page b\\par{\\*\\unknown skipped}{\\colortbl;\\red0;}}"),
            "a\n\n<!-- pagebreak -->\n\nb\n"
        );
    }

    #[test]
    fn corrupt_input_is_tolerated() {
        let table: &[&[u8]] = &[
            b"{\\rtf1",
            b"{\\rtf1 }}}} text",
            b"{\\rtf1 \\'zz \\'4 \\",
            b"{\\rtf1 \\u99999999999 \\uc-5 \\u-1 x\\par",
            b"{\\rtf1 {\\pict\\pngblip 8}",
            b"{\\rtf1 \x80\x81\x82 \xff\\par}",
            b"{\\rtf1 \\f0\\fcharset999 \\'e4\\par}",
        ];
        for data in table {
            assert!(import_rtf(data).is_ok(), "{data:?}");
        }
        let deep = format!(
            "{{\\rtf1 {} x {}\\par}}",
            "{".repeat(1000),
            "}".repeat(1000)
        );
        assert!(import_rtf(deep.as_bytes()).unwrap().markdown.contains('x'));
    }

    #[test]
    fn font_charset_selects_code_page() {
        let rtf = "{\\rtf1\\ansicpg1252{\\fonttbl{\\f0\\fcharset0 A;}{\\f1\\fcharset238 B;}}\\f0 \\'bf \\f1 \\'bf\\par}";
        assert_eq!(md(rtf), "\u{bf} \u{17c}\n");
    }

    #[test]
    fn picture_data_keeps_only_its_hex_digits_and_empty_pictures_vanish() {
        // Characters that are not hex digits are skipped; an odd trailing digit is not a byte.
        let r = import_rtf(b"{\\rtf1 a{\\pict\\pngblip 89 50 zz4E 4Q7 f}b\\par}").unwrap();
        assert_eq!(r.markdown, "a![](assets/image-1.png)b\n");
        assert_eq!(r.assets[0].bytes, vec![0x89, 0x50, 0x4E, 0x47]);
        assert!(r.losses.is_empty());
        for rtf in [
            "{\\rtf1 a{\\pict}b\\par}",
            "{\\rtf1 a{\\pict\\pngblip}b\\par}",
        ] {
            let r = import_rtf(rtf.as_bytes()).unwrap();
            assert_eq!(r.markdown, "ab\n", "{rtf}");
            assert!(r.assets.is_empty() && r.losses.is_empty(), "{rtf}");
        }
        // A single digit is no picture, but it was data: reported.
        let r = import_rtf(b"{\\rtf1 a{\\pict\\pngblip 8}b\\par}").unwrap();
        assert_eq!(r.markdown, "ab\n");
        assert!(r.assets.is_empty());
        assert_eq!(
            r.losses.iter().map(|l| l.kind).collect::<Vec<_>>(),
            [LossKind::UnsupportedImages]
        );
    }

    #[test]
    fn list_table_entries_without_their_parameters_are_ignored() {
        // `\levelnfc` before any level, `\listid` and `\ls` without a number, `\ls` before a list id.
        let rtf = "{\\rtf1{\\*\\listtable{\\list\\levelnfc0\\listid}{\\list{\\listlevel\\levelnfc0}\\listid2}}{\\*\\listoverridetable{\\listoverride\\ls1}{\\listoverride\\listid2\\ls}{\\listoverride\\listid2\\ls2}}\\pard\\ls1 a\\par\\pard\\ls2 b\\par}";
        assert_eq!(md(rtf), "- a\n1. b\n");
    }

    #[test]
    fn switched_off_or_parameterless_words_change_nothing() {
        // (rtf, markdown, losses)
        type Case<'a> = (&'a str, &'a str, Vec<(LossKind, usize)>);
        let table: [Case; 5] = [
            (
                "{\\rtf1 a{\\v hid\\v0 shown}b\\par}",
                "ashownb\n",
                vec![(LossKind::HiddenText, 1)],
            ),
            ("{\\rtf1 a{\\super0 x}{\\sub0 y}b\\par}", "axyb\n", vec![]),
            ("{\\rtf1 a\\f b\\par}", "ab\n", vec![]),
            ("{\\rtf1 a\\itap1\\cols1 b\\par}", "ab\n", vec![]),
            ("{\\rtf1 a\\u b\\par}", "ab\n", vec![]),
        ];
        for (rtf, expected, losses) in table {
            let r = import_rtf(rtf.as_bytes()).unwrap();
            assert_eq!(r.markdown, expected, "{rtf}");
            let got: Vec<(LossKind, usize)> = r.losses.iter().map(|l| (l.kind, l.count)).collect();
            assert_eq!(got, losses, "{rtf}");
        }
    }

    #[test]
    fn empty_paragraphs_and_rows_add_nothing() {
        let table = [
            // an empty paragraph of a footnote
            (
                "{\\rtf1 x{\\footnote\\par note}\\par}",
                "x[^1]\n\n[^1]: note\n",
            ),
            // a row without cells
            ("{\\rtf1\\trowd\\row a\\par}", "a\n"),
            // a list label without any text
            ("{\\rtf1\\pard{\\pntext 1.\\tab}\\par x\\par}", "x\n"),
        ];
        for (rtf, expected) in table {
            assert_eq!(md(rtf), expected, "{rtf}");
        }
    }

    #[test]
    fn paragraphs_of_one_table_cell_join_into_a_line() {
        let rtf = "{\\rtf1\\trowd\\cellx1000\\pard\\intbl a\\par\\par b\\par c\\cell\\row\\pard after\\par}";
        let r = import_rtf(rtf.as_bytes()).unwrap();
        assert_eq!(r.markdown, "| a b c |\n|---|\n\nafter\n");
        let got: Vec<(LossKind, usize)> = r.losses.iter().map(|l| (l.kind, l.count)).collect();
        assert_eq!(got, [(LossKind::MultiParagraphCells, 2)]);
    }

    #[test]
    fn hyperlink_instruction_parsing() {
        let table = [
            ("HYPERLINK \"http://a\"", Some("http://a")),
            ("  HYPERLINK \"http://a\" \\o \"tip\"", Some("http://a")),
            ("HYPERLINK \\l \"mark\"", Some("#mark")),
            ("HYPERLINK", None),
            ("PAGE", None),
        ];
        for (instr, expected) in table {
            assert_eq!(parse_hyperlink(instr).as_deref(), expected, "{instr}");
        }
    }
}
