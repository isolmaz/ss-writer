//! Inline pass: code spans, escapes, entities, delimiter runs (emphasis family), links,
//! images, wikilinks, footnote references, math, autolinks and breaks.
//!
//! Scanning produces a flat token list; emphasis and bracket resolution only annotate it, and
//! the final nesting is built in one pass, so no token is ever moved.

use smallvec::smallvec;

use crate::markdown::ast::{
    HighlightColor, Image, ImageKind, Inline, InlineKind, Link, LinkKind, Markers, ReferenceForm,
    WikiLink,
};
use crate::markdown::definitions::{Definitions, LabelKind, normalize_label};
use crate::text::TextRange;

use super::scan;
use super::source::Source;

struct Delim {
    ch: u8,
    s: usize,
    orig: usize,
    extra: usize,
    color: Option<HighlightColor>,
    can_open: bool,
    can_close: bool,
    used_open: usize,
    used_close: usize,
    opens: Vec<usize>,
    closes: Vec<usize>,
}

struct WrapInfo {
    kind: InlineKind,
    open: TextRange,
    close: TextRange,
}

enum Tok {
    Text(usize, usize),
    Leaf(Box<Inline>),
    Delim(usize),
    Bracket(usize),
    Open,
    Close(usize),
}

struct Bracket {
    tok: usize,
    image: bool,
    /// Buffer position of `[`.
    pos: usize,
    active: bool,
    delim_bottom: usize,
}

struct Frame {
    children: Vec<Inline>,
    last_text_end: Option<usize>,
}

struct Inl<'a> {
    s: &'a Source,
    b: &'a [u8],
    defs: &'a mut Definitions,
    block: TextRange,
    in_table: bool,
    toks: Vec<Tok>,
    delims: Vec<Delim>,
    stack: Vec<usize>,
    brackets: Vec<Bracket>,
    bstack: Vec<usize>,
    wraps: Vec<WrapInfo>,
    /// Buffer position up to which non-text tokens (leaves, brackets) were emitted.
    barrier: usize,
}

/// Parses the inline content of `source`.
pub(super) fn parse_inlines(
    defs: &mut Definitions,
    block: TextRange,
    source: &Source,
    in_table: bool,
) -> Vec<Inline> {
    let mut p = Inl {
        s: source,
        b: source.buf.as_bytes(),
        defs,
        block,
        in_table,
        toks: Vec::new(),
        delims: Vec::new(),
        stack: Vec::new(),
        brackets: Vec::new(),
        bstack: Vec::new(),
        wraps: Vec::new(),
        barrier: 0,
    };
    p.scan();
    p.process_emphasis(0);
    p.build()
}

fn leaf_tok(node: Inline) -> Tok {
    Tok::Leaf(Box::new(node))
}

fn leaf(range: TextRange, content: TextRange, markers: Markers, kind: InlineKind) -> Inline {
    Inline {
        range,
        content,
        markers,
        kind,
        children: Vec::new(),
    }
}

fn is_special(c: u8) -> bool {
    matches!(
        c,
        b'\\'
            | b'`'
            | b'*'
            | b'_'
            | b'~'
            | b'='
            | b'['
            | b']'
            | b'!'
            | b'<'
            | b'&'
            | b'\n'
            | b'$'
            | b'@'
            | b'h'
            | b'H'
            | b'w'
            | b'W'
            | b'f'
            | b'F'
    )
}

impl<'a> Inl<'a> {
    fn char_before(&self, i: usize) -> Option<char> {
        self.s.buf[..i].chars().next_back()
    }

    fn char_after(&self, i: usize) -> Option<char> {
        self.s.buf[i..].chars().next()
    }

    fn scan(&mut self) {
        let b = self.b;
        let len = b.len();
        let mut i = 0;
        while i < len {
            let c = b[i];
            if !is_special(c) {
                let mut j = i + 1;
                while j < len && !is_special(b[j]) {
                    j += 1;
                }
                i = self.text_run(i, j);
                continue;
            }
            i = match c {
                b'\n' => self.line_break(i, i),
                b'\\' => self.backslash(i),
                b'`' => self.code_span(i),
                b'*' | b'_' | b'~' | b'=' => self.delimiter_run(i),
                b'[' => self.open_bracket(i),
                b'!' => {
                    if b.get(i + 1) == Some(&b'[') {
                        self.push_bracket(i, true)
                    } else {
                        self.push_text(i, i + 1);
                        i + 1
                    }
                }
                b']' => self.close_bracket(i),
                b'<' => self.angle(i),
                b'&' => self.entity(i),
                b'$' => self.math(i),
                b'@' => self.email(i),
                _ => self.maybe_autolink(i),
            };
            if matches!(
                self.toks.last(),
                Some(Tok::Leaf(_) | Tok::Open | Tok::Close(_) | Tok::Bracket(_))
            ) {
                self.barrier = i;
            }
        }
    }

    fn push_text(&mut self, a: usize, b: usize) {
        if a < b {
            self.toks.push(Tok::Text(a, b));
        }
    }

    /// Plain text `[i, j)`; trailing spaces before a newline become a break.
    fn text_run(&mut self, i: usize, j: usize) -> usize {
        if self.b.get(j) == Some(&b'\n') {
            let mut k = j;
            while k > i && self.b[k - 1] == b' ' {
                k -= 1;
            }
            self.push_text(i, k);
            return self.line_break(k, j);
        }
        self.push_text(i, j);
        j
    }

    /// Break whose spaces start at `from` and whose newline is at `nl`.
    fn line_break(&mut self, from: usize, nl: usize) -> usize {
        let spaces = nl - from;
        let range = self.s.range(from, nl + 1);
        if spaces >= 2 {
            let m = self.s.range(from, nl);
            let content = self.s.range(nl, nl + 1);
            self.toks.push(leaf_tok(leaf(
                range,
                content,
                smallvec![m],
                InlineKind::HardBreak,
            )));
        } else {
            self.toks.push(leaf_tok(leaf(
                range,
                range,
                smallvec![],
                InlineKind::SoftBreak,
            )));
        }
        nl + 1
    }

    fn backslash(&mut self, i: usize) -> usize {
        match self.b.get(i + 1) {
            Some(b'\n') => {
                let m = self.s.range(i, i + 1);
                let range = self.s.range(i, i + 2);
                let content = self.s.range(i + 1, i + 2);
                self.toks.push(leaf_tok(leaf(
                    range,
                    content,
                    smallvec![m],
                    InlineKind::HardBreak,
                )));
                i + 2
            }
            Some(c) if c.is_ascii_punctuation() => {
                let range = self.s.range(i, i + 2);
                let m = self.s.range(i, i + 1);
                let content = self.s.range(i + 1, i + 2);
                self.toks.push(leaf_tok(leaf(
                    range,
                    content,
                    smallvec![m],
                    InlineKind::Escape,
                )));
                i + 2
            }
            _ => {
                self.push_text(i, i + 1);
                i + 1
            }
        }
    }

    fn code_span(&mut self, i: usize) -> usize {
        let b = self.b;
        let mut n = 0;
        while b.get(i + n) == Some(&b'`') {
            n += 1;
        }
        let mut j = i + n;
        while j < b.len() {
            if b[j] == b'`' {
                let mut m = 0;
                while b.get(j + m) == Some(&b'`') {
                    m += 1;
                }
                if m == n {
                    let raw = &self.s.buf[i + n..j];
                    let mut text = raw.replace('\n', " ");
                    if self.in_table {
                        text = text.replace("\\|", "|");
                    }
                    if text.len() >= 2
                        && text.starts_with(' ')
                        && text.ends_with(' ')
                        && text.bytes().any(|c| c != b' ')
                    {
                        text = text[1..text.len() - 1].to_string();
                    }
                    let markers: Markers =
                        smallvec![self.s.range(i, i + n), self.s.range(j, j + n)];
                    let node = leaf(
                        self.s.range(i, j + n),
                        self.s.range(i + n, j),
                        markers,
                        InlineKind::Code { text },
                    );
                    self.toks.push(leaf_tok(node));
                    return j + n;
                }
                j += m;
            } else {
                j += 1;
            }
        }
        self.push_text(i, i + n);
        i + n
    }

    fn entity(&mut self, i: usize) -> usize {
        if let Some((decoded, used)) = scan::decode_entity(&self.b[i..]) {
            let r = self.s.range(i, i + used);
            self.toks.push(leaf_tok(leaf(
                r,
                r,
                smallvec![],
                InlineKind::Entity { decoded },
            )));
            i + used
        } else {
            self.push_text(i, i + 1);
            i + 1
        }
    }

    // ------------------------------------------------------------ delimiters

    fn delimiter_run(&mut self, i: usize) -> usize {
        let b = self.b;
        let ch = b[i];
        let mut n = 0;
        while b.get(i + n) == Some(&ch) {
            n += 1;
        }
        let allowed = match ch {
            b'*' | b'_' => true,
            b'~' => n <= 2,
            _ => n == 2,
        };
        if !allowed {
            self.push_text(i, i + n);
            return i + n;
        }
        let mut extra = 0;
        let mut color = None;
        if ch == b'='
            && b.get(i + 2) == Some(&b'{')
            && let Some(close) = b[i + 3..].iter().take(9).position(|&c| c == b'}')
        {
            let name = &self.s.buf[i + 3..i + 3 + close];
            if let Some(c) = HighlightColor::from_name(name) {
                extra = close + 2;
                color = Some(c);
            }
        }
        let before = self.char_before(i);
        let after = self.char_after(i + n + extra);
        let ws_b = before.is_none_or(scan::is_unicode_whitespace);
        let p_b = before.is_some_and(scan::is_punctuation);
        let ws_a = after.is_none_or(scan::is_unicode_whitespace);
        let p_a = after.is_some_and(scan::is_punctuation);
        let left = !ws_a && (!p_a || ws_b || p_b);
        let right = !ws_b && (!p_b || ws_a || p_a);
        let (mut can_open, mut can_close) = if ch == b'_' {
            (left && (!right || p_b), right && (!left || p_a))
        } else {
            (left, right)
        };
        if extra > 0 {
            if can_open {
                can_close = false;
            } else {
                extra = 0;
                color = None;
                let after0 = self.char_after(i + n);
                let ws_a0 = after0.is_none_or(scan::is_unicode_whitespace);
                let p_a0 = after0.is_some_and(scan::is_punctuation);
                can_open = !ws_a0 && (!p_a0 || ws_b || p_b);
                can_close = !ws_b && (!p_b || ws_a0 || p_a0);
            }
        }
        let d = self.delims.len();
        self.delims.push(Delim {
            ch,
            s: i,
            orig: n,
            extra,
            color,
            can_open,
            can_close,
            used_open: 0,
            used_close: 0,
            opens: Vec::new(),
            closes: Vec::new(),
        });
        self.toks.push(Tok::Delim(d));
        if can_open || can_close {
            self.stack.push(d);
        }
        i + n + extra
    }

    fn process_emphasis(&mut self, bottom: usize) {
        let mut closer_pos = bottom;
        while closer_pos < self.stack.len() {
            let c = self.stack[closer_pos];
            if !self.delims[c].can_close {
                closer_pos += 1;
                continue;
            }
            let mut found = None;
            let mut k = closer_pos;
            while k > bottom {
                k -= 1;
                let o = &self.delims[self.stack[k]];
                let cl = &self.delims[c];
                if o.ch != cl.ch || !o.can_open || o.orig - o.used_open - o.used_close == 0 {
                    continue;
                }
                match cl.ch {
                    b'*' | b'_' => {
                        let odd = (cl.can_open || o.can_close)
                            && !cl.orig.is_multiple_of(3)
                            && (o.orig + cl.orig).is_multiple_of(3);
                        if odd {
                            continue;
                        }
                    }
                    _ => {
                        if o.orig != cl.orig || o.used_open != 0 || cl.used_close != 0 {
                            continue;
                        }
                    }
                }
                found = Some(k);
                break;
            }
            let Some(opener_pos) = found else {
                if !self.delims[c].can_open {
                    self.stack.remove(closer_pos);
                } else {
                    closer_pos += 1;
                }
                continue;
            };
            let o = self.stack[opener_pos];
            let (kind, used) = {
                let od = &self.delims[o];
                let cd = &self.delims[c];
                let o_rem = od.orig - od.used_open - od.used_close;
                let c_rem = cd.orig - cd.used_open - cd.used_close;
                match od.ch {
                    b'*' | b'_' => {
                        if o_rem >= 2 && c_rem >= 2 {
                            (InlineKind::Strong, 2)
                        } else {
                            (InlineKind::Emphasis, 1)
                        }
                    }
                    b'~' => (
                        if od.orig == 1 {
                            InlineKind::Underline
                        } else {
                            InlineKind::Strikethrough
                        },
                        od.orig,
                    ),
                    _ => (InlineKind::Highlight { color: od.color }, 2),
                }
            };
            let (open_marker, close_marker) = {
                let od = &self.delims[o];
                let cd = &self.delims[c];
                let end = od.s + od.orig - od.used_open;
                let open_end = if od.extra > 0 { end + od.extra } else { end };
                (
                    self.s.range(end - used, open_end),
                    self.s
                        .range(cd.s + cd.used_close, cd.s + cd.used_close + used),
                )
            };
            let w = self.wraps.len();
            self.wraps.push(WrapInfo {
                kind,
                open: open_marker,
                close: close_marker,
            });
            self.delims[o].used_open += used;
            self.delims[o].opens.push(w);
            self.delims[c].used_close += used;
            self.delims[c].closes.push(w);
            self.stack.drain(opener_pos + 1..closer_pos).for_each(drop);
            closer_pos = opener_pos + 1;
            let o_left = {
                let od = &self.delims[o];
                od.orig - od.used_open - od.used_close
            };
            if o_left == 0 {
                self.stack.remove(opener_pos);
                closer_pos -= 1;
            }
            let c_left = {
                let cd = &self.delims[c];
                cd.orig - cd.used_open - cd.used_close
            };
            if c_left == 0 {
                self.stack.remove(closer_pos);
            }
        }
        self.stack.truncate(bottom);
    }

    // ------------------------------------------------------------ brackets

    fn open_bracket(&mut self, i: usize) -> usize {
        if self.b.get(i + 1) == Some(&b'[')
            && let Some(end) = self.wikilink(i)
        {
            return end;
        }
        self.push_bracket(i, false)
    }

    fn push_bracket(&mut self, i: usize, image: bool) -> usize {
        let pos = if image { i + 1 } else { i };
        let tok = self.toks.len();
        self.brackets.push(Bracket {
            tok,
            image,
            pos,
            active: true,
            delim_bottom: self.stack.len(),
        });
        self.bstack.push(self.brackets.len() - 1);
        self.toks.push(Tok::Bracket(self.brackets.len() - 1));
        pos + 1
    }

    fn wikilink(&mut self, i: usize) -> Option<usize> {
        let b = self.b;
        let mut j = i + 2;
        while j < b.len() {
            match b[j] {
                b']' => break,
                b'[' | b'\n' => return None,
                _ => j += 1,
            }
        }
        if b.get(j) != Some(&b']') || b.get(j + 1) != Some(&b']') || j == i + 2 {
            return None;
        }
        let inner = &self.s.buf[i + 2..j];
        let (left, alias) = match inner.find('|') {
            Some(p) => (&inner[..p], Some(&inner[p + 1..])),
            None => (inner, None),
        };
        if left.is_empty() && alias.is_none() {
            return None;
        }
        let left_end = i + 2 + left.len();
        let (target, heading, target_end) = match left.find('#') {
            Some(p) => (&left[..p], Some(&left[p + 1..]), i + 2 + p),
            None => (left, None, left_end),
        };
        let mut markers: Markers = smallvec![self.s.range(i, i + 2)];
        let alias_info = alias.map(|a| (a, self.s.range(left_end + 1, j)));
        if alias.is_some() {
            markers.push(self.s.range(left_end, left_end + 1));
        }
        markers.push(self.s.range(j, j + 2));
        let heading_range = heading.map(|_| self.s.range(target_end + 1, left_end));
        let content = match alias_info {
            Some((_, r)) => r,
            None => self.s.range(i + 2, left_end),
        };
        let node = leaf(
            self.s.range(i, j + 2),
            content,
            markers,
            InlineKind::WikiLink(WikiLink {
                target: target.to_string(),
                heading: heading.map(str::to_string),
                alias: alias.map(str::to_string),
                target_range: self.s.range(i + 2, target_end),
                heading_range,
                alias_range: alias_info.map(|(_, r)| r),
            }),
        );
        self.toks.push(leaf_tok(node));
        Some(j + 2)
    }

    fn close_bracket(&mut self, p: usize) -> usize {
        let Some(&top_idx) = self.bstack.last() else {
            self.push_text(p, p + 1);
            return p + 1;
        };
        if !self.brackets[top_idx].active {
            self.bstack.pop();
            self.push_text(p, p + 1);
            return p + 1;
        }
        let top = &self.brackets[top_idx];
        let (tok_idx, image, open_pos, delim_bottom) =
            (top.tok, top.image, top.pos, top.delim_bottom);
        let text_start = open_pos + 1;

        if !image && let Some(end) = self.try_footnote(tok_idx, open_pos, p, delim_bottom) {
            return end;
        }
        let Some(resolved) = self.resolve_link(open_pos, p) else {
            self.bstack.pop();
            self.push_text(p, p + 1);
            return p + 1;
        };
        self.bstack.pop();
        self.process_emphasis(delim_bottom);

        let open_start = if image { open_pos - 1 } else { open_pos };
        let open_marker = self.s.range(open_start, open_pos + 1);
        let mut close_start = p;
        let mut alt_end = p;
        let mut width = None;
        let mut height = None;
        if image && let Some(Tok::Text(a, e)) = self.toks.last() {
            let (a, e) = (*a, *e);
            if a >= text_start
                && let Some((idx, w, h)) = size_suffix(&self.s.buf[a..e])
            {
                close_start = a + idx;
                alt_end = close_start;
                width = Some(w);
                height = h;
                let last = self.toks.len() - 1;
                self.toks[last] = Tok::Text(a, a + idx);
            }
        }
        let close_marker = self.s.range(close_start, resolved.end_pos);
        let kind = if image {
            InlineKind::Image(Image {
                dest: resolved.dest,
                title: resolved.title,
                alt: self.s.range(text_start, alt_end),
                width,
                height,
                kind: match resolved.reference {
                    None => ImageKind::Inline {
                        dest_range: resolved.dest_range,
                        title_range: resolved.title_range,
                    },
                    Some((label, form, label_range)) => ImageKind::Reference {
                        label,
                        form,
                        label_range,
                    },
                },
            })
        } else {
            InlineKind::Link(Link {
                dest: resolved.dest,
                title: resolved.title,
                kind: match resolved.reference {
                    None => LinkKind::Inline {
                        dest_range: resolved.dest_range,
                        title_range: resolved.title_range,
                    },
                    Some((label, form, label_range)) => LinkKind::Reference {
                        label,
                        form,
                        label_range,
                    },
                },
            })
        };
        let w = self.wraps.len();
        self.wraps.push(WrapInfo {
            kind,
            open: open_marker,
            close: close_marker,
        });
        self.toks[tok_idx] = Tok::Open;
        self.toks.push(Tok::Close(w));
        if !image {
            for &k in &self.bstack {
                if !self.brackets[k].image {
                    self.brackets[k].active = false;
                }
            }
        }
        resolved.end_pos
    }

    fn try_footnote(
        &mut self,
        tok_idx: usize,
        open_pos: usize,
        p: usize,
        delim_bottom: usize,
    ) -> Option<usize> {
        let inner = &self.s.buf[open_pos + 1..p];
        let label = inner.strip_prefix('^')?;
        if label.is_empty() || label.contains(|c: char| c.is_whitespace() || c == '[' || c == '\n')
        {
            return None;
        }
        let norm = normalize_label(label);
        self.defs.record_use(self.block, LabelKind::Footnote, norm);
        if !self.defs.has_footnote(label) {
            return None;
        }
        let label = label.to_string();
        self.bstack.pop();
        self.stack.truncate(delim_bottom);
        self.toks.truncate(tok_idx);
        let range = self.s.range(open_pos, p + 1);
        let markers: Markers =
            smallvec![self.s.range(open_pos, open_pos + 2), self.s.range(p, p + 1)];
        let content = self.s.range(open_pos + 2, p);
        self.toks.push(leaf_tok(leaf(
            range,
            content,
            markers,
            InlineKind::FootnoteRef { label },
        )));
        Some(p + 1)
    }

    fn resolve_link(&mut self, open_pos: usize, p: usize) -> Option<Resolved> {
        let b = self.b;
        // inline link
        if b.get(p + 1) == Some(&b'(')
            && let Some(r) = self.inline_link(p)
        {
            return Some(r);
        }
        // reference link
        let mut label: Option<(String, ReferenceForm, TextRange)> = None;
        let mut end = p + 1;
        if b.get(p + 1) == Some(&b'[') {
            if b.get(p + 2) == Some(&b']') {
                end = p + 3;
                if scan::link_label_end(b, open_pos) == Some(p + 1) {
                    label = Some((
                        self.s.buf[open_pos + 1..p].to_string(),
                        ReferenceForm::Collapsed,
                        self.s.range(open_pos + 1, p),
                    ));
                }
            } else if let Some(le) = scan::link_label_end(b, p + 1) {
                end = le;
                label = Some((
                    self.s.buf[p + 2..le - 1].to_string(),
                    ReferenceForm::Full,
                    self.s.range(p + 2, le - 1),
                ));
            } else if scan::link_label_end(b, open_pos) == Some(p + 1) {
                label = Some((
                    self.s.buf[open_pos + 1..p].to_string(),
                    ReferenceForm::Shortcut,
                    self.s.range(open_pos + 1, p),
                ));
            }
        } else if scan::link_label_end(b, open_pos) == Some(p + 1) {
            label = Some((
                self.s.buf[open_pos + 1..p].to_string(),
                ReferenceForm::Shortcut,
                self.s.range(open_pos + 1, p),
            ));
        }
        let (label, form, label_range) = label?;
        let norm = normalize_label(&label);
        self.defs
            .record_use(self.block, LabelKind::Link, norm.clone());
        let def = self.defs.link(&label)?;
        let (dest, title) = (def.dest.clone(), def.title.clone());
        let r = TextRange::at(self.s.start(p));
        Some(Resolved {
            dest,
            title,
            end_pos: end,
            dest_range: r,
            title_range: None,
            reference: Some((norm, form, label_range)),
        })
    }

    fn inline_link(&self, p: usize) -> Option<Resolved> {
        let b = self.b;
        let mut i = scan::skip_spaces_and_newline(b, p + 2);
        let ((ds, de), dest_end) = scan::link_destination(b, i)?;
        i = scan::skip_spaces_and_newline(b, dest_end);
        let mut title = None;
        if i > dest_end
            && let Some(((ts, te), tend)) = scan::link_title(b, i)
        {
            title = Some((ts, te));
            i = scan::skip_spaces_and_newline(b, tend);
        }
        if b.get(i) != Some(&b')') {
            return None;
        }
        Some(Resolved {
            dest: scan::unescape(&self.s.buf[ds..de]),
            title: title.map(|(ts, te)| scan::unescape(&self.s.buf[ts..te])),
            end_pos: i + 1,
            dest_range: self.s.range(ds, de),
            title_range: title.map(|(ts, te)| self.s.range(ts, te)),
            reference: None,
        })
    }

    // ------------------------------------------------------------ angle brackets

    fn angle(&mut self, i: usize) -> usize {
        let b = self.b;
        if let Some(end) = autolink_end(b, i) {
            let inner = &self.s.buf[i + 1..end - 1];
            let is_email = !is_uri(inner);
            let dest = if is_email {
                format!("mailto:{inner}")
            } else {
                inner.to_string()
            };
            let content = self.s.range(i + 1, end - 1);
            let text = leaf(content, content, smallvec![], InlineKind::Text);
            let mut node = leaf(
                self.s.range(i, end),
                content,
                smallvec![self.s.range(i, i + 1), self.s.range(end - 1, end)],
                InlineKind::Link(Link {
                    dest,
                    title: None,
                    kind: LinkKind::Autolink { angle: true },
                }),
            );
            node.children.push(text);
            self.toks.push(leaf_tok(node));
            return end;
        }
        if let Some(end) = scan::html_inline_end(b, i) {
            let tag = &b[i..end];
            let range = self.s.range(i, end);
            if let Some(img) = self.html_image(i, tag, range) {
                self.toks.push(leaf_tok(img));
            } else {
                self.toks.push(leaf_tok(leaf(
                    range,
                    range,
                    smallvec![],
                    InlineKind::RawHtml,
                )));
            }
            return end;
        }
        self.push_text(i, i + 1);
        i + 1
    }

    fn html_image(&self, i: usize, tag: &[u8], range: TextRange) -> Option<Inline> {
        if tag.len() < 5 || !tag[1..4].eq_ignore_ascii_case(b"img") || !(is_tag_boundary(tag[4])) {
            return None;
        }
        let (sv, ev) = scan::tag_attribute(tag, "src")?;
        let text = std::str::from_utf8(tag).ok()?;
        let attr = |name: &str| scan::tag_attribute(tag, name);
        let alt = match attr("alt") {
            Some((a, e)) => self.s.range(i + a, i + e),
            None => TextRange::at(range.start),
        };
        let num = |name: &str| attr(name).and_then(|(a, e)| scan::parse_pixels(&text[a..e]));
        Some(leaf(
            range,
            range,
            smallvec![],
            InlineKind::Image(Image {
                dest: scan::unescape(&text[sv..ev]),
                title: attr("title").map(|(a, e)| scan::unescape(&text[a..e])),
                alt,
                width: num("width"),
                height: num("height"),
                kind: ImageKind::Html,
            }),
        ))
    }

    // ------------------------------------------------------------ math

    fn math(&mut self, i: usize) -> usize {
        let b = self.b;
        let mut n = 0;
        while b.get(i + n) == Some(&b'$') {
            n += 1;
        }
        if n > 2 {
            self.push_text(i, i + n);
            return i + n;
        }
        let start = i + n;
        let first_ws = self.char_after(start).is_none_or(char::is_whitespace);
        if n == 1 && first_ws {
            self.push_text(i, i + n);
            return i + n;
        }
        let mut j = start;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'$' => {
                    let mut m = 0;
                    while b.get(j + m) == Some(&b'$') {
                        m += 1;
                    }
                    if m == n && j > start {
                        let prev_ws = self.char_before(j).is_some_and(char::is_whitespace);
                        let next_digit = b.get(j + n).is_some_and(u8::is_ascii_digit);
                        if n == 2 || (!prev_ws && !next_digit) {
                            let markers: Markers =
                                smallvec![self.s.range(i, i + n), self.s.range(j, j + n)];
                            let node = leaf(
                                self.s.range(i, j + n),
                                self.s.range(start, j),
                                markers,
                                InlineKind::InlineMath,
                            );
                            self.toks.push(leaf_tok(node));
                            return j + n;
                        }
                    }
                    if n == 1 && m == 1 {
                        break;
                    }
                    j += m;
                }
                _ => j += 1,
            }
        }
        self.push_text(i, i + n);
        i + n
    }

    // ------------------------------------------------------------ extended autolinks

    fn maybe_autolink(&mut self, i: usize) -> usize {
        let b = self.b;
        if !self.bstack.is_empty() {
            self.push_text(i, i + 1);
            return i + 1;
        }
        let rest = &b[i..];
        let starts = |p: &[u8]| rest.len() >= p.len() && rest[..p.len()].eq_ignore_ascii_case(p);
        let (prefix_len, www) = if starts(b"www.") {
            (4, true)
        } else if starts(b"https://") {
            (8, false)
        } else if starts(b"http://") {
            (7, false)
        } else if starts(b"ftp://") {
            (6, false)
        } else {
            self.push_text(i, i + 1);
            return i + 1;
        };
        let prev = self.char_before(i);
        let prev_ok = if www {
            prev.is_none_or(|c| {
                scan::is_unicode_whitespace(c) || matches!(c, '*' | '_' | '~' | '(')
            })
        } else {
            !prev.is_some_and(|c| c.is_ascii_alphabetic())
        };
        if !prev_ok {
            self.push_text(i, i + 1);
            return i + 1;
        }
        let mut e = i + prefix_len;
        while e < b.len() && !scan::is_html_space(b[e]) && b[e] != b'<' {
            e += 1;
        }
        let text_end = i + trim_autolink(&self.s.buf[i..e]);
        let domain_start = if www { i } else { i + prefix_len };
        let run_len = b[domain_start..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(**c, b'-' | b'_' | b'.'))
            .count();
        let run = &self.s.buf[domain_start..domain_start + run_len];
        let underscore = run.split('.').rev().take(2).any(|seg| seg.contains('_'));
        let valid = !underscore && (!www || run_len > 4);
        if !valid || text_end <= i + prefix_len {
            self.push_text(i, i + 1);
            return i + 1;
        }
        let text = &self.s.buf[i..text_end];
        let dest = if www {
            format!("http://{text}")
        } else {
            text.to_string()
        };
        let content = self.s.range(i, text_end);
        self.push_autolink(content, dest);
        text_end
    }

    fn push_autolink(&mut self, content: TextRange, dest: String) {
        let child = leaf(content, content, smallvec![], InlineKind::Text);
        let mut node = leaf(
            content,
            content,
            smallvec![],
            InlineKind::Link(Link {
                dest,
                title: None,
                kind: LinkKind::Autolink { angle: false },
            }),
        );
        node.children.push(child);
        self.toks.push(leaf_tok(node));
    }

    /// GFM e-mail autolink around the `@` at `i`, including `mailto:` / `xmpp:` prefixes.
    fn email(&mut self, i: usize) -> usize {
        let b = self.b;
        if !self.bstack.is_empty() {
            self.push_text(i, i + 1);
            return i + 1;
        }
        let local_char =
            |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b'+');
        let mut s = i;
        while s > self.barrier && local_char(b[s - 1]) {
            s -= 1;
        }
        let mut j = i + 1;
        let mut dots = 0;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'-' | b'_' | b'.')) {
            if b[j] == b'.' {
                if !b.get(j + 1).is_some_and(u8::is_ascii_alphanumeric) {
                    break;
                }
                dots += 1;
            }
            j += 1;
        }
        if s == i || dots == 0 || matches!(b[j - 1], b'-' | b'_') {
            self.push_text(i, i + 1);
            return i + 1;
        }
        let mut start = s;
        let mut xmpp = false;
        for (prefix, is_xmpp) in [("mailto:", false), ("xmpp:", true)] {
            let n = prefix.len();
            if s >= n
                && b[s - n..s].eq_ignore_ascii_case(prefix.as_bytes())
                && (s == n || !b[s - n - 1].is_ascii_alphanumeric())
            {
                start = s - n;
                xmpp = is_xmpp;
                break;
            }
        }
        let mut end = j;
        if xmpp && b.get(j) == Some(&b'/') {
            let mut x = j;
            while x < b.len() && !scan::is_html_space(b[x]) && b[x] != b'<' {
                x += 1;
            }
            end = (start + trim_autolink(&self.s.buf[start..x])).max(j);
        }
        while let Some(tok) = self.toks.last() {
            let (tok_start, text_end) = match tok {
                Tok::Text(a, e) => (*a, Some(*e)),
                Tok::Delim(d) => (self.delims[*d].s, None),
                _ => break,
            };
            if tok_start >= start {
                self.toks.pop();
                continue;
            }
            if text_end.is_some_and(|e| e > start) {
                let last = self.toks.len() - 1;
                self.toks[last] = Tok::Text(tok_start, start);
            }
            break;
        }
        self.stack.retain(|&d| self.delims[d].s < start);
        let text = &self.s.buf[start..end];
        let dest = if start == s {
            format!("mailto:{text}")
        } else {
            text.to_string()
        };
        let content = self.s.range(start, end);
        self.push_autolink(content, dest);
        end
    }

    // ------------------------------------------------------------ final assembly

    fn emit_text(&self, stack: &mut [Frame], a: usize, b: usize) {
        if a >= b {
            return;
        }
        let Some(frame) = stack.last_mut() else {
            return;
        };
        let range = self.s.range(a, b);
        if frame.last_text_end == Some(a)
            && let Some(last) = frame.children.last_mut()
        {
            last.range = TextRange::new(last.range.start, range.end);
            last.content = last.range;
            frame.last_text_end = Some(b);
            return;
        }
        frame
            .children
            .push(leaf(range, range, smallvec![], InlineKind::Text));
        frame.last_text_end = Some(b);
    }

    fn close_frame(&self, stack: &mut Vec<Frame>, w: usize) {
        if stack.len() < 2 {
            return;
        }
        let Some(frame) = stack.pop() else { return };
        let info = &self.wraps[w];
        let range = TextRange::new(info.open.start, info.close.end.max(info.open.start));
        let content = TextRange::new(info.open.end, info.close.start.max(info.open.end));
        let node = Inline {
            range,
            content,
            markers: smallvec![info.open, info.close],
            kind: info.kind.clone(),
            children: frame.children,
        };
        if let Some(parent) = stack.last_mut() {
            parent.children.push(node);
            parent.last_text_end = None;
        }
    }

    fn build(&self) -> Vec<Inline> {
        let mut stack = vec![Frame {
            children: Vec::new(),
            last_text_end: None,
        }];
        for tok in &self.toks {
            match tok {
                Tok::Text(a, b) => self.emit_text(&mut stack, *a, *b),
                Tok::Leaf(node) => {
                    if let Some(f) = stack.last_mut() {
                        f.children.push((**node).clone());
                        f.last_text_end = None;
                    }
                }
                Tok::Delim(d) => {
                    let d = &self.delims[*d];
                    for &w in &d.closes {
                        self.close_frame(&mut stack, w);
                    }
                    let start = d.s + d.used_close;
                    let mut end = d.s + d.orig - d.used_open;
                    if d.used_open == 0 {
                        end += d.extra;
                    }
                    if end > start {
                        self.emit_text(&mut stack, start, end);
                    }
                    for _ in &d.opens {
                        stack.push(Frame {
                            children: Vec::new(),
                            last_text_end: None,
                        });
                    }
                }
                Tok::Bracket(k) => {
                    let br = &self.brackets[*k];
                    let start = if br.image { br.pos - 1 } else { br.pos };
                    self.emit_text(&mut stack, start, br.pos + 1);
                }
                Tok::Open => stack.push(Frame {
                    children: Vec::new(),
                    last_text_end: None,
                }),
                Tok::Close(w) => self.close_frame(&mut stack, *w),
            }
        }
        while stack.len() > 1 {
            let Some(frame) = stack.pop() else { break };
            if let Some(parent) = stack.last_mut() {
                parent.children.extend(frame.children);
            }
        }
        stack.pop().map(|f| f.children).unwrap_or_default()
    }
}

struct Resolved {
    dest: String,
    title: Option<String>,
    /// Buffer position after the whole link syntax.
    end_pos: usize,
    dest_range: TextRange,
    title_range: Option<TextRange>,
    reference: Option<(String, ReferenceForm, TextRange)>,
}

fn is_tag_boundary(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'/' | b'>' | b'\r')
}

fn is_uri(s: &str) -> bool {
    let b = s.as_bytes();
    let Some(colon) = b.iter().position(|&c| c == b':') else {
        return false;
    };
    (2..=32).contains(&colon)
        && b[0].is_ascii_alphabetic()
        && b[..colon]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-'))
}

/// End of the `<…>` autolink (URI or e-mail) at `i`.
fn autolink_end(b: &[u8], i: usize) -> Option<usize> {
    let close = b[i + 1..].iter().position(|&c| c == b'>')? + i + 1;
    let inner = &b[i + 1..close];
    if inner.iter().any(|&c| c <= 0x20 || c == b'<') {
        return None;
    }
    let text = std::str::from_utf8(inner).ok()?;
    if is_uri(text) {
        return Some(close + 1);
    }
    let at = text.find('@')?;
    let (local, domain) = (&text[..at], &text[at + 1..]);
    let local_ok = !local.is_empty()
        && local
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(&c));
    let domain_ok = !domain.is_empty()
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        });
    (local_ok && domain_ok).then_some(close + 1)
}

/// Length of `s` after dropping the trailing punctuation GFM excludes from autolinks.
fn trim_autolink(s: &str) -> usize {
    let mut end = s.len();
    loop {
        let t = &s[..end];
        let Some(last) = t.chars().next_back() else {
            return end;
        };
        match last {
            '?' | '!' | '.' | ',' | ':' | '*' | '_' | '~' | '\'' | '"' => end -= 1,
            ')' => {
                let open = t.matches('(').count();
                let close = t.matches(')').count();
                if close > open {
                    end -= 1;
                } else {
                    return end;
                }
            }
            ';' => {
                let body = &t[..t.len() - 1];
                match body.rfind('&') {
                    Some(amp)
                        if body[amp + 1..].bytes().all(|c| c.is_ascii_alphanumeric())
                            && amp + 1 < body.len() =>
                    {
                        end = amp;
                    }
                    _ => end -= 1,
                }
            }
            _ => return end,
        }
    }
}

/// `|300` / `|300x200` at the end of `text`: `(index of '|', width, height)`.
fn size_suffix(text: &str) -> Option<(usize, u32, Option<u32>)> {
    let b = text.as_bytes();
    let digits = |end: usize| {
        b[..end]
            .iter()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .count()
    };
    let d1 = digits(b.len());
    if d1 == 0 {
        return None;
    }
    let after_first = b.len() - d1;
    let h: u32 = text[after_first..].parse().ok()?;
    if after_first > 0 && b[after_first - 1] == b'|' {
        return Some((after_first - 1, h, None));
    }
    if after_first > 0 && b[after_first - 1] == b'x' {
        let before_x = after_first - 1;
        let d0 = digits(before_x);
        if d0 > 0 && before_x - d0 > 0 && b[before_x - d0 - 1] == b'|' {
            let w: u32 = text[before_x - d0..before_x].parse().ok()?;
            return Some((before_x - d0 - 1, w, Some(h)));
        }
    }
    None
}
