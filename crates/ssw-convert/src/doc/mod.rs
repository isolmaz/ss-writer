//! Legacy Word 97–2003 (`.doc`, [MS-DOC]) → Markdown.
//!
//! The reader works directly on the compound file: the FIB locates the piece table (text), the
//! stylesheet, the list tables and the FKP pages (paragraph / character properties). A text
//! walk over the CP stream then rebuilds headings, nested lists, tables, footnotes, hyperlinks
//! (field state machine), inline formatting and inline pictures. Everything that cannot be
//! represented is counted in the loss report of the [`ImportResult`].
//!
//! Word 6/95/2.0, encrypted files and corrupt containers are refused with a precise
//! [`ImportError`]; files that are not OLE at all are sniffed ([`sniff::sniff`]) so the caller
//! can route them to the right importer.

mod fib;
mod fields;
mod lists;
pub(crate) mod md;
mod pictures;
mod pieces;
mod props;
pub mod sniff;
mod styles;
mod tables;

use std::collections::HashMap;

use crate::import::{Asset, ImportError, ImportResult, LossKind};

use fib::{
    IDX_CLX, IDX_PLCF_BTE_CHPX, IDX_PLCF_BTE_PAPX, IDX_PLCFEND_REF, IDX_PLCFEND_TXT,
    IDX_PLCFFND_REF, IDX_PLCFFND_TXT,
};
use fib::{IDX_PLCF_SED, IDX_PLF_LFO, IDX_PLF_LST, IDX_STSHF, slice, u32_at};
use fields::FieldKind;
use lists::{Lists, Marker, Numbering};
use md::{Align, Block, Ctx, Fmt, Inline};
use pictures::Picture;
use pieces::{Pieces, SKIP};
use props::{Chp, FkpSource, Kind, Pap, Run, toggle};
use styles::Stylesheet;
use tables::{TableLoss, Tables};

/// The Markdown column alignment of a paragraph justification (`jc`): 0 left, 1 center, 2 right;
/// justified text and the other values have none.
fn jc_align(jc: Option<u8>) -> Align {
    match jc {
        Some(0) => Align::Left,
        Some(1) => Align::Center,
        Some(2) => Align::Right,
        _ => Align::None,
    }
}

/// Converts the bytes of a `.doc` file to Markdown.
///
/// `assets_dir` is the folder (relative to the Markdown file, `/`-separated) that receives the
/// extracted pictures; an empty string puts them next to the Markdown file.
pub fn import(bytes: &[u8], assets_dir: &str) -> Result<ImportResult, ImportError> {
    let (fib, streams) = fib::load(bytes)?;
    let mut res = ImportResult::default();
    let mut counts = Counts::default();

    let (clx_fc, clx_lcb) = fib.pair(IDX_CLX);
    let pieces = Pieces::parse(&streams.table, clx_fc, clx_lcb)?;
    let (text, truncated) = pieces.read_text(&streams.word, fib.text_codepage())?;
    counts.corrupt += truncated as usize;

    let (stsh_fc, stsh_lcb) = fib.pair(IDX_STSHF);
    let styles = Stylesheet::parse(&streams.table, stsh_fc, stsh_lcb).unwrap_or_else(|_| {
        counts.corrupt += 1;
        Stylesheet::default()
    });
    let lists = Lists::parse(&streams.table, fib.pair(IDX_PLF_LST), fib.pair(IDX_PLF_LFO))
        .unwrap_or_else(|_| {
            counts.corrupt += 1;
            Lists::default()
        });
    let source = FkpSource {
        word: &streams.word,
        table: &streams.table,
        data: &streams.data,
        pieces: &pieces,
    };
    let mut bad_pages = 0;
    let (pf, pl) = fib.pair(IDX_PLCF_BTE_PAPX);
    let paps = props::read_runs(Kind::Papx, &source, pf, pl, &mut bad_pages)?;
    let (cf, cl) = fib.pair(IDX_PLCF_BTE_CHPX);
    let chps = props::read_runs(Kind::Chpx, &source, cf, cl, &mut bad_pages)?;
    counts.corrupt += bad_pages as usize;

    // Story boundaries (CP): main, footnotes, headers/footers, comments, endnotes, text boxes.
    let total = text.len() as u32;
    let ftn0 = fib.ccp_text;
    let hdd0 = ftn0.saturating_add(fib.ccp_ftn);
    let atn0 = hdd0.saturating_add(fib.ccp_hdd);
    let edn0 = atn0.saturating_add(fib.ccp_atn);
    let txbx0 = edn0.saturating_add(fib.ccp_edn);
    let hdr_txbx0 = txbx0.saturating_add(fib.ccp_txbx);
    counts.headers_footers = visible_paragraphs(&text, hdd0, atn0);
    counts.comments = visible_paragraphs(&text, atn0, edn0);
    counts.text_boxes =
        visible_paragraphs(&text, txbx0, hdr_txbx0.saturating_add(fib.ccp_hdr_txbx));
    counts.sections = section_count(&streams.table, fib.pair(IDX_PLCF_SED));

    let (ftn_ref, ftn_txt) = note_tables(
        &streams.table,
        fib.pair(IDX_PLCFFND_REF),
        fib.pair(IDX_PLCFFND_TXT),
    );
    let (edn_ref, edn_txt) = note_tables(
        &streams.table,
        fib.pair(IDX_PLCFEND_REF),
        fib.pair(IDX_PLCFEND_TXT),
    );
    let n_ftn = ftn_ref.len().min(ftn_txt.len().saturating_sub(1));
    let n_edn = edn_ref.len().min(edn_txt.len().saturating_sub(1));
    let doc = Doc {
        paps,
        chps,
        styles,
        lists,
        data: streams.data,
        ftn_ref: ftn_ref
            .iter()
            .take(n_ftn)
            .enumerate()
            .map(|(i, &cp)| (cp, i + 1))
            .collect(),
        edn_ref: edn_ref
            .iter()
            .take(n_edn)
            .enumerate()
            .map(|(i, &cp)| (cp, i + 1))
            .collect(),
        text,
    };

    let mut cv = Conv {
        d: &doc,
        assets_dir,
        res: &mut res,
        counts,
        fields: Vec::new(),
        numbering: Numbering::default(),
        tables: Tables::default(),
        table_loss: TableLoss::default(),
        serial: 0,
        sub: Sub::Main,
        pending_break: false,
        images: 0,
    };
    let blocks = cv.render_range(0, ftn0.min(total), Sub::Main);
    let mut markdown = md::blocks_to_string(&blocks);

    let mut defs = String::new();
    for (label, base, txt, n, sub) in [
        ("", ftn0, &ftn_txt, n_ftn, Sub::Footnote),
        ("e", edn0, &edn_txt, n_edn, Sub::Endnote),
    ] {
        for i in 0..n {
            let a = base.saturating_add(txt[i]);
            let b = base.saturating_add(txt[i + 1]).min(total);
            if a >= b {
                continue;
            }
            let blocks = cv.render_range(a, b, sub);
            let body = md::blocks_to_string(&blocks);
            defs.push_str(&md::footnote_definition(
                &format!("{label}{}", i + 1),
                &body,
            ));
        }
    }
    if !defs.is_empty() {
        if !markdown.is_empty() {
            markdown.push_str("\n\n");
        }
        markdown.push_str(defs.trim_end());
    }
    if !markdown.is_empty() {
        markdown.push('\n');
    }
    cv.finish();
    res.markdown = markdown;
    Ok(res)
}

/// Number of paragraphs of `text[a..b)` that contain a visible character.
fn visible_paragraphs(text: &[u16], a: u32, b: u32) -> usize {
    let (a, b) = (a as usize, (b as usize).min(text.len()));
    if a >= b {
        return 0;
    }
    text[a..b]
        .split(|&c| c == 0x0D)
        .filter(|p| p.iter().any(|&c| c > 0x20 && c != 0xA0 && c != SKIP))
        .count()
}

/// Number of sections in the PlcfSed (CPs `n + 1`, 12-byte Sed entries).
fn section_count(table: &[u8], (fc, lcb): (u32, u32)) -> usize {
    if lcb < 4 || slice(table, fc as usize, lcb as usize).is_err() {
        return 0;
    }
    (lcb as usize - 4) / 16
}

/// `(reference CPs in the main story, text CPs relative to the note story)` of a footnote or
/// endnote PLC pair.
fn note_tables(table: &[u8], refp: (u32, u32), txtp: (u32, u32)) -> (Vec<u32>, Vec<u32>) {
    let read = |(fc, lcb): (u32, u32), count: fn(usize) -> usize| -> Vec<u32> {
        if lcb < 4 {
            return Vec::new();
        }
        let Ok(b) = slice(table, fc as usize, lcb as usize) else {
            return Vec::new();
        };
        (0..count(b.len()))
            .filter_map(|i| u32_at(b, i * 4).ok())
            .collect()
    };
    // PlcffndRef: n CPs + 1 terminator + n 2-byte FRD; PlcffndTxt: only CPs.
    (read(refp, |len| (len - 4) / 6), read(txtp, |len| len / 4))
}

// ---- the converter -----------------------------------------------------------------------

/// Immutable decoded document.
struct Doc {
    text: Vec<u16>,
    paps: Vec<Run>,
    chps: Vec<Run>,
    styles: Stylesheet,
    lists: Lists,
    data: Vec<u8>,
    ftn_ref: HashMap<u32, usize>,
    edn_ref: HashMap<u32, usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sub {
    Main,
    Footnote,
    Endnote,
}

/// Occurrence counts behind the loss report.
#[derive(Default)]
struct Counts {
    alignment: usize,
    underline: usize,
    sub_super: usize,
    symbols: usize,
    hidden: usize,
    deleted: usize,
    floating: usize,
    vector_images: usize,
    unreadable_images: usize,
    ole_objects: usize,
    suppressed_fields: usize,
    internal_links: usize,
    list_formats: usize,
    corrupt: usize,
    headers_footers: usize,
    comments: usize,
    text_boxes: usize,
    sections: usize,
}

struct FieldCtx {
    instr: Vec<u16>,
    in_result: bool,
    link: Option<String>,
    suppress: bool,
    /// Index in the paragraph's inline list where the result starts.
    start: usize,
    /// Paragraph serial of the field result start: links must not span paragraphs.
    serial: u32,
}

struct ParaInfo {
    pap: Pap,
    heading: Option<u8>,
    /// Paragraph-style character properties (so headings are not wrapped in `**`).
    style_chp: Chp,
}

struct Eff {
    fmt: Fmt,
    underline: bool,
    sub_super: bool,
    vanish: bool,
    del: bool,
    spec: bool,
    data: bool,
    ole2: bool,
    pic_loc: Option<u32>,
    symbol: Option<(u16, u16)>,
}

struct Conv<'a> {
    d: &'a Doc,
    assets_dir: &'a str,
    res: &'a mut ImportResult,
    counts: Counts,
    fields: Vec<FieldCtx>,
    numbering: Numbering,
    tables: Tables,
    table_loss: TableLoss,
    serial: u32,
    sub: Sub,
    /// A hard page break was seen in the current paragraph.
    pending_break: bool,
    images: usize,
}

impl Conv<'_> {
    fn ch(&self, cp: u32) -> u16 {
        self.d.text.get(cp as usize).copied().unwrap_or(0x0D)
    }

    /// Does the character at `cp` end a paragraph or cell? A page break ends one only when the
    /// PAPX run ends right after it.
    fn is_term(&self, cp: u32) -> bool {
        match self.ch(cp) {
            0x0D | 0x07 => true,
            0x0C => props::find(&self.d.paps, cp).is_some_and(|r| r.cp1 == cp + 1),
            _ => false,
        }
    }

    fn para_info(&self, term_cp: u32) -> ParaInfo {
        let d = self.d;
        let run = props::find(&d.paps, term_cp);
        let istd = run.map_or(0, |r| r.istd);
        let mut pap = d.styles.style_pap(istd);
        if let Some(r) = run {
            pap.apply(&r.grpprl);
        }
        let heading = d
            .styles
            .heading_level(istd)
            .or_else(|| d.styles.is_title(istd).then_some(1))
            .or_else(|| pap.outlvl.filter(|&o| o < 9).map(|o| o + 1));
        ParaInfo {
            pap,
            heading,
            style_chp: d.styles.style_chp(istd),
        }
    }

    /// Effective character formatting of a run: paragraph style, character style, direct.
    fn eff(&self, grpprl: &[u8], pi: &ParaInfo) -> Eff {
        let mut dir = Chp::default();
        dir.apply(grpprl);
        let cst = dir
            .istd
            .map(|i| self.d.styles.style_chp(i))
            .unwrap_or_default();
        let pst = &pi.style_chp;
        let resolve =
            |p: Option<u8>, c: Option<u8>, r: Option<u8>| toggle(toggle(toggle(false, p), c), r);
        let heading = pi.heading.is_some();
        // a heading's own bold/italic is part of the heading, not of the text
        let bold = resolve(pst.bold, cst.bold, dir.bold) && !(heading && toggle(false, pst.bold));
        let italic =
            resolve(pst.italic, cst.italic, dir.italic) && !(heading && toggle(false, pst.italic));
        let strike = resolve(pst.strike, cst.strike, dir.strike)
            || resolve(pst.dstrike, cst.dstrike, dir.dstrike);
        let iss = dir.iss.or(cst.iss).or(pst.iss).unwrap_or(0);
        Eff {
            fmt: Fmt {
                bold,
                italic,
                strike,
                ..Fmt::default()
            },
            underline: dir.kul.or(cst.kul).or(pst.kul).unwrap_or(0) != 0,
            sub_super: iss == 1 || iss == 2,
            vanish: resolve(pst.vanish, cst.vanish, dir.vanish),
            del: dir.del == Some(1),
            spec: dir.spec == Some(1),
            data: dir.data == Some(1),
            ole2: dir.ole2 == Some(1),
            pic_loc: dir.pic_loc,
            symbol: dir.symbol,
        }
    }

    /// Text is hidden while inside the result of a suppressed field.
    fn hidden_in_field(&self) -> bool {
        self.fields.iter().any(|f| f.in_result && f.suppress)
    }

    /// Inline content of one paragraph `[a, b)`.
    fn render_inline(&mut self, a: u32, b: u32, pi: &ParaInfo) -> Vec<Inline> {
        let d = self.d;
        let mut inl: Vec<Inline> = Vec::new();
        let mut pending: Vec<u16> = Vec::new();
        let mut cp = a;
        while cp < b {
            let idx = props::first_at_or_after(&d.chps, cp);
            let (grp, run_end): (&[u8], u32) = match d.chps.get(idx) {
                Some(r) if r.cp0 <= cp => (&r.grpprl, r.cp1.min(b)),
                Some(r) => (&[], r.cp0.min(b)),
                None => (&[], b),
            };
            let eff = self.eff(grp, pi);
            let fmt = eff.fmt;
            while cp < run_end {
                let c = self.ch(cp);
                match c {
                    0x13 => {
                        flush(&mut inl, &mut pending, fmt);
                        self.fields.push(FieldCtx {
                            instr: Vec::new(),
                            in_result: false,
                            link: None,
                            suppress: false,
                            start: 0,
                            serial: self.serial,
                        });
                    }
                    0x14 => {
                        flush(&mut inl, &mut pending, fmt);
                        self.field_separator(inl.len());
                    }
                    0x15 => {
                        flush(&mut inl, &mut pending, fmt);
                        self.field_end(&mut inl);
                    }
                    // inside a field instruction (any ancestor): collect, never render
                    _ if self.fields.iter().any(|f| !f.in_result) => {
                        let open = self.fields.iter_mut().find(|f| !f.in_result);
                        open.expect("the guard found an open instruction")
                            .instr
                            .push(c);
                    }
                    _ if self.hidden_in_field() => {}
                    _ if eff.vanish => self.counts.hidden += 1,
                    _ if eff.del => self.counts.deleted += 1,
                    _ => self.character(c, &eff, cp, &mut inl, &mut pending),
                }
                cp += 1;
            }
            flush(&mut inl, &mut pending, fmt);
        }
        inl
    }

    /// `0x14`: the instruction is complete; classify the innermost open field.
    fn field_separator(&mut self, inline_len: usize) {
        let serial = self.serial;
        let Some(i) = self.fields.iter().rposition(|f| !f.in_result) else {
            return;
        };
        let instr = String::from_utf16_lossy(&self.fields[i].instr);
        let (link, suppress) = match fields::classify(&instr) {
            FieldKind::Hyperlink(url) => (Some(url), false),
            FieldKind::InternalLink => {
                self.counts.internal_links += 1;
                (None, false)
            }
            FieldKind::Suppressed => {
                self.counts.suppressed_fields += 1;
                (None, true)
            }
            FieldKind::Plain => (None, false),
        };
        let f = &mut self.fields[i];
        f.in_result = true;
        f.link = link;
        f.suppress = suppress;
        f.start = inline_len;
        f.serial = serial;
    }

    /// `0x15`: close the innermost field; a hyperlink wraps the inline nodes of its result.
    fn field_end(&mut self, inl: &mut Vec<Inline>) {
        let Some(f) = self.fields.pop() else { return };
        let Some(url) = f
            .link
            .filter(|_| f.in_result && f.serial == self.serial && f.start <= inl.len())
        else {
            return;
        };
        let kids = inl.split_off(f.start);
        if !kids.is_empty() {
            inl.push(Inline::Link { url, kids });
        }
    }

    /// One ordinary character (not hidden, not inside an instruction).
    fn character(
        &mut self,
        c: u16,
        eff: &Eff,
        cp: u32,
        inl: &mut Vec<Inline>,
        pending: &mut Vec<u16>,
    ) {
        match (c, eff.symbol) {
            (0x01, _) => {
                flush(inl, pending, eff.fmt);
                if eff.spec
                    && let Some(raw) = self.picture(eff)
                {
                    inl.push(Inline::Raw(raw));
                }
            }
            (0x02, _) => {
                flush(inl, pending, eff.fmt);
                if self.sub == Sub::Main {
                    if let Some(n) = self.d.ftn_ref.get(&cp) {
                        inl.push(Inline::Raw(format!("[^{n}]")));
                    } else if let Some(n) = self.d.edn_ref.get(&cp) {
                        inl.push(Inline::Raw(format!("[^e{n}]")));
                    }
                }
            }
            (0x08, _) => self.counts.floating += 1,
            (0x09, _) => {
                pending.push(0x20);
                self.note_fmt(eff);
            }
            (0x0B, _) => {
                flush(inl, pending, eff.fmt);
                inl.push(Inline::Break);
            }
            (0x0C, _) => self.pending_break = true,
            (0x1E, _) => pending.push(u16::from(b'-')),
            (0x28, Some((_, ch))) => {
                self.counts.symbols += 1;
                pending.push(fields::symbol_to_unicode(ch));
            }
            // annotation / field-data marks, cell marks, paragraph marks, optional hyphen and
            // every other control character carry no text
            (c, _) if c < 0x20 || c == SKIP => {}
            (0xF0B7 | 0xF0A7 | 0xF0D8 | 0xF076 | 0xF0FC, _) => {
                self.counts.symbols += 1;
                pending.push(fields::symbol_to_unicode(c));
            }
            (c, _) => {
                if (0xF020..=0xF0FF).contains(&c) {
                    self.counts.symbols += 1;
                }
                pending.push(c);
                self.note_fmt(eff);
            }
        }
    }

    fn note_fmt(&mut self, e: &Eff) {
        self.counts.underline += usize::from(e.underline);
        self.counts.sub_super += usize::from(e.sub_super);
    }

    /// Markdown for the picture character of `e`, extracting the image as an asset.
    fn picture(&mut self, e: &Eff) -> Option<String> {
        if e.data {
            return None; // form-field data, not a picture
        }
        if e.ole2 {
            self.counts.ole_objects += 1;
            return Some("*[embedded object]*".into());
        }
        let loc = e.pic_loc?;
        match pictures::extract(&self.d.data, loc) {
            Picture::Image { ext, bytes } => {
                self.images += 1;
                let name = format!("image-{}.{ext}", self.images);
                let path = if self.assets_dir.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{name}", self.assets_dir)
                };
                self.res.assets.push(Asset {
                    relative_path: path.clone(),
                    bytes,
                });
                Some(format!(
                    "![image-{}]({})",
                    self.images,
                    fields::clean_url(&path)
                ))
            }
            Picture::Vector => {
                self.counts.vector_images += 1;
                Some("*[image omitted]*".into())
            }
            Picture::Unreadable => {
                self.counts.unreadable_images += 1;
                Some("*[image omitted]*".into())
            }
        }
    }

    // ---- block structure: paragraphs, lists, tables ---------------------------------------

    fn render_range(&mut self, cp0: u32, cp1: u32, sub: Sub) -> Vec<Block> {
        self.sub = sub;
        self.fields.clear();
        let mut blocks = Vec::new();
        let mut start = cp0;
        while start < cp1 {
            let mut term = start;
            while term < cp1 && !self.is_term(term) {
                term += 1;
            }
            let has_term = term < cp1;
            let term_cp = if has_term { term } else { cp1 - 1 };
            let tch = if has_term { self.ch(term) } else { 0x0D };
            let end = if has_term { term } else { cp1 };
            self.unit(start, end, term_cp, tch, &mut blocks);
            start = term + 1;
        }
        self.close_tables(0, &mut blocks);
        blocks
    }

    /// One paragraph (or table cell / row mark) `[a, b)` terminated at `term_cp`.
    fn unit(&mut self, a: u32, b: u32, term_cp: u32, tch: u16, blocks: &mut Vec<Block>) {
        self.serial += 1;
        self.pending_break = false;
        let pi = self.para_info(term_cp);
        let depth = (pi.pap.itap_eff() as usize).min(tables::MAX_DEPTH);
        let is_row_end = depth > 0 && (pi.pap.ttp == Some(true) || pi.pap.inner_ttp == Some(true));
        let is_cell_end =
            depth > 0 && !is_row_end && (tch == 0x07 || pi.pap.inner_cell == Some(true));

        self.close_tables(depth, blocks);
        self.tables.open_to(depth);

        if is_row_end {
            self.tables
                .end_row(depth, pi.pap.cells.as_deref(), &mut self.table_loss);
            return;
        }

        let inl = self.render_inline(a, b, &pi);
        let ctx = match (depth, pi.heading) {
            (0, Some(_)) => Ctx::Heading,
            (0, None) => Ctx::Body,
            _ => Ctx::Cell,
        };
        let mut text = md::render_inlines(&inl, ctx);
        let text_empty = text.trim().is_empty();
        // centered and right-aligned text in a table cell becomes a column alignment
        let column_aligned = depth > 0 && matches!(pi.pap.jc, Some(1 | 2));
        if !text_empty && !column_aligned && matches!(pi.pap.jc, Some(1..=3)) {
            self.counts.alignment += 1;
        }
        let marker = self.list_marker(&pi.pap);

        if depth > 0 {
            if !text_empty {
                text = cell_item_text(marker.as_ref(), text);
                self.tables.push_paragraph(depth, text);
                self.tables.align_cell(depth, jc_align(pi.pap.jc));
            }
            if is_cell_end {
                self.tables.end_cell(depth, &mut self.table_loss);
            }
            return;
        }

        let break_after = self.pending_break || tch == 0x0C;
        if pi.pap.page_break_before {
            push_break(blocks);
        }
        if !text_empty {
            if let Some(level) = pi.heading {
                // direct bold over the whole heading is redundant with `#`
                let mut t = text;
                if let Some(inner) = t
                    .strip_prefix("**")
                    .and_then(|s| s.strip_suffix("**"))
                    .filter(|i| !i.contains("**"))
                {
                    t = inner.to_string();
                }
                blocks.push(Block::Heading(level.clamp(1, 6), t));
            } else if let Some((level, marker)) = marker {
                blocks.push(Block::Item {
                    level,
                    marker,
                    text,
                });
            } else {
                blocks.push(Block::Para(text));
            }
        }
        if break_after {
            push_break(blocks);
        }
    }

    /// Closes the tables deeper than `keep`; a finished top-level table becomes a block.
    fn close_tables(&mut self, keep: usize, blocks: &mut Vec<Block>) {
        if let Some(rows) = self.tables.close_to(keep, &mut self.table_loss) {
            let align = self.tables.take_alignments();
            blocks.push(Block::Table { rows, align });
        }
    }

    /// `(nesting level, marker)` of a list paragraph; `-` for bullets, `N.` for numbers.
    fn list_marker(&mut self, pap: &Pap) -> Option<(u8, String)> {
        let ilfo = pap.ilfo?;
        let lvl = pap.ilvl.unwrap_or(0).min(8);
        match self.d.lists.marker(ilfo, lvl, &mut self.numbering) {
            Marker::None => None,
            Marker::Dangling => {
                self.counts.corrupt += 1;
                None
            }
            Marker::Bullet => Some((lvl, "-".to_string())),
            Marker::Number { n, format } => {
                // Markdown only has arabic numbering
                self.counts.list_formats += usize::from(format != 0);
                Some((lvl, format!("{n}.")))
            }
        }
    }

    /// Writes the aggregated losses to the result.
    fn finish(&mut self) {
        let c = &self.counts;
        let t = self.table_loss;
        let list = [
            (LossKind::Alignment, c.alignment),
            (LossKind::Underline, c.underline),
            (LossKind::SubSuperscript, c.sub_super),
            (LossKind::SymbolFontCharacters, c.symbols),
            (LossKind::HiddenText, c.hidden),
            (LossKind::TrackedChanges, c.deleted),
            (LossKind::VectorShapes, c.floating),
            (
                LossKind::UnsupportedImages,
                c.vector_images + c.unreadable_images,
            ),
            (LossKind::UnsupportedEquations, c.ole_objects),
            (LossKind::Fields, c.suppressed_fields + c.internal_links),
            (LossKind::ListNumberingStyle, c.list_formats),
            (LossKind::CorruptParts, c.corrupt),
            (LossKind::HeadersFooters, c.headers_footers),
            (LossKind::Comments, c.comments),
            (LossKind::TextBoxes, c.text_boxes),
            (LossKind::SectionBreaks, c.sections.saturating_sub(1)),
            (LossKind::MergedCells, t.merged_cells),
            (LossKind::MultiParagraphCells, t.multi_paragraph_cells),
            (LossKind::NestedTables, t.nested_tables),
            (LossKind::CorruptParts, t.truncated_tables),
        ];
        for (kind, count) in list {
            self.res.record_loss(kind, count, None);
        }
    }
}

/// Appends the pending text as one inline node and clears it.
fn flush(inl: &mut Vec<Inline>, pending: &mut Vec<u16>, fmt: Fmt) {
    if pending.is_empty() {
        return;
    }
    inl.push(Inline::Text(String::from_utf16_lossy(pending), fmt));
    pending.clear();
}

/// Table cells cannot hold Markdown list syntax, so the marker of a list paragraph inside a
/// cell becomes a text prefix (a bullet for `-`).
fn cell_item_text(marker: Option<&(u8, String)>, text: String) -> String {
    match marker {
        None => text,
        Some((_, m)) => format!("{} {text}", if m == "-" { "\u{2022}" } else { m }),
    }
}

fn push_break(blocks: &mut Vec<Block>) {
    if !matches!(blocks.last(), Some(Block::PageBreak) | None) {
        blocks.push(Block::PageBreak);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn counts_visible_paragraphs_in_a_story() {
        let text: Vec<u16> = "ab\r \r\u{a0}\rc\r\r".encode_utf16().collect();
        assert_eq!(visible_paragraphs(&text, 0, text.len() as u32), 2);
        assert_eq!(visible_paragraphs(&text, 3, 4), 0, "blank paragraph");
        assert_eq!(
            visible_paragraphs(&text, 5, 1000),
            1,
            "range is clamped to the text"
        );
        assert_eq!(visible_paragraphs(&text, 9, 3), 0, "inverted range");
        assert_eq!(visible_paragraphs(&[SKIP, 0x0D], 0, 2), 0);
    }

    #[test]
    fn justification_maps_to_column_alignments() {
        let cases = [
            (None, Align::None),
            (Some(0), Align::Left),
            (Some(1), Align::Center),
            (Some(2), Align::Right),
            (Some(3), Align::None),
            (Some(4), Align::None),
        ];
        for (jc, want) in cases {
            assert_eq!(jc_align(jc), want, "{jc:?}");
        }
    }

    #[test]
    fn counts_sections() {
        let table = vec![0u8; 100];
        assert_eq!(section_count(&table, (0, 36)), 2);
        assert_eq!(section_count(&table, (0, 4)), 0);
        assert_eq!(section_count(&table, (0, 3)), 0);
        assert_eq!(
            section_count(&table, (90, 36)),
            0,
            "outside the table stream"
        );
    }

    #[test]
    fn reads_note_tables() {
        let mut table = Vec::new();
        // PlcffndRef: 2 reference CPs + terminator + 2 FRD
        for v in [5u32, 9, 20] {
            table.extend_from_slice(&v.to_le_bytes());
        }
        table.extend_from_slice(&[0; 4]);
        // PlcffndTxt: 3 CPs
        for v in [0u32, 4, 7] {
            table.extend_from_slice(&v.to_le_bytes());
        }
        let (refs, txt) = note_tables(&table, (0, 16), (16, 12));
        assert_eq!((refs, txt), (vec![5, 9], vec![0, 4, 7]));
        assert_eq!(note_tables(&table, (0, 3), (100, 12)), (vec![], vec![]));
    }

    // ---- `import` over a hand-built compound file --------------------------------------------

    /// A compound file holding the two streams `import` needs.
    fn ole(word: &[u8], table: &[u8]) -> Vec<u8> {
        let mut cf = cfb::CompoundFile::create(std::io::Cursor::new(Vec::new())).unwrap();
        for (name, data) in [("/WordDocument", word), ("/0Table", table)] {
            let mut stream = cf.create_stream(name).unwrap();
            stream.write_all(data).unwrap();
        }
        cf.flush().unwrap();
        cf.into_inner().into_inner()
    }

    /// A compound file whose text is `main` followed by the footnote and the endnote story. Each
    /// note argument is `(story text, reference CPs in the main story, CP at which each note
    /// text starts - relative to its story - with the end CP last)`. The style and list tables
    /// lie beyond the end of the table stream (two corrupt parts).
    fn notes_doc(
        main: &[u8],
        ftn: (&[u8], &[u32], &[u32]),
        edn: (&[u8], &[u32], &[u32]),
    ) -> Vec<u8> {
        let main_len = main.len() as u32;
        let text = [main, ftn.0, edn.0].concat();
        // PlcfndRef: n CPs, the end CP, n 2-byte FRDs; PlcfndTxt: only CPs
        let ref_plc = |refs: &[u32]| -> Vec<u8> {
            refs.iter()
                .copied()
                .chain([main_len])
                .flat_map(u32::to_le_bytes)
                .chain(vec![0u8; refs.len() * 2])
                .collect()
        };
        let txt_plc =
            |cps: &[u32]| -> Vec<u8> { cps.iter().copied().flat_map(u32::to_le_bytes).collect() };
        // one 8-bit piece whose text starts at byte 1024 of the WordDocument stream
        let clx = pieces::tests::clx(&[], &[(0, text.len() as u32, 0x4000_0000 | (1024 * 2), 0)]);
        let mut table = clx.clone();
        let mut pairs = vec![(0u32, 0u32); IDX_PLF_LFO + 1];
        pairs[IDX_CLX] = (0, clx.len() as u32);
        for (ref_index, txt_index, (_, refs, cps)) in [
            (IDX_PLCFFND_REF, IDX_PLCFFND_TXT, ftn),
            (IDX_PLCFEND_REF, IDX_PLCFEND_TXT, edn),
        ] {
            let (refs, cps) = (ref_plc(refs), txt_plc(cps));
            pairs[ref_index] = (table.len() as u32, refs.len() as u32);
            table.extend_from_slice(&refs);
            pairs[txt_index] = (table.len() as u32, cps.len() as u32);
            table.extend_from_slice(&cps);
        }
        pairs[IDX_STSHF] = (100_000, 4);
        pairs[IDX_PLF_LST] = (100_000, 10);
        let mut word = fib::tests::fib_bytes(0xA5EC, 0x00C1, 0, 14, 22, &pairs);
        // FibRgLw97 starts at byte 64: ccpText, ccpFtn, ccpHdd, reserved, ccpAtn, ccpEdn, ...
        for (at, ccp) in [
            (76, main_len),
            (80, ftn.0.len() as u32),
            (84, 0),
            (92, 0),
            (96, edn.0.len() as u32),
            (100, 0),
            (104, 0),
        ] {
            word[at..at + 4].copy_from_slice(&ccp.to_le_bytes());
        }
        word.resize(1024, 0);
        word.extend_from_slice(&text);
        ole(&word, &table)
    }

    #[test]
    fn unreadable_style_and_list_tables_are_counted_and_empty_notes_skipped() {
        // main story "A[ref]B[ref]\r" (5 characters), then the footnote story: an empty first
        // note and a second one reading "Second\r" (7 characters)
        let text = b"A\x02B\x02\rSecond\r";
        let ref_plc: Vec<u8> = [1u32, 3, 5]
            .iter()
            .flat_map(|cp| cp.to_le_bytes())
            .chain([0u8; 4])
            .collect();
        let txt_plc: Vec<u8> = [0u32, 0, 7]
            .iter()
            .flat_map(|cp| cp.to_le_bytes())
            .collect();
        // one 8-bit piece whose text starts at byte 1024 of the WordDocument stream
        let clx = pieces::tests::clx(&[], &[(0, 12, 0x4000_0000 | (1024 * 2), 0)]);
        let ftn_ref_at = clx.len() as u32;
        let mut table = clx.clone();
        table.extend_from_slice(&ref_plc);
        table.extend_from_slice(&txt_plc);

        let mut pairs = vec![(0u32, 0u32); IDX_PLF_LFO + 1];
        pairs[IDX_CLX] = (0, clx.len() as u32);
        pairs[IDX_PLCFFND_REF] = (ftn_ref_at, ref_plc.len() as u32);
        pairs[IDX_PLCFFND_TXT] = (ftn_ref_at + ref_plc.len() as u32, txt_plc.len() as u32);
        // both structures lie far beyond the end of the table stream
        pairs[IDX_STSHF] = (100_000, 4);
        pairs[IDX_PLF_LST] = (100_000, 10);
        let mut word = fib::tests::fib_bytes(0xA5EC, 0x00C1, 0, 14, 22, &pairs);
        // FibRgLw97 starts at byte 64: ccpText, ccpFtn, then every other story empty
        word[76..80].copy_from_slice(&5u32.to_le_bytes());
        word[80..84].copy_from_slice(&7u32.to_le_bytes());
        for at in [84, 92, 96, 100, 104] {
            word[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
        }
        word.resize(1024, 0);
        word.extend_from_slice(text);

        let res = import(&ole(&word, &table), "assets").unwrap();
        // the empty first note has no definition; the second keeps its number
        assert_eq!(res.markdown, "A[^1]B[^2]\n\n[^2]: Second\n");
        assert!(res.assets.is_empty());
        assert_eq!(losses(&res), vec![(LossKind::CorruptParts, 2)]);
    }

    // ---- the paragraph walker over hand-built runs -------------------------------------------

    const CFRMARKDEL: u16 = 0x0800;
    const CFDATA: u16 = 0x0806;
    const CFSPEC: u16 = 0x0855;
    const CFOLE2: u16 = 0x0856;
    const CPICLOCATION: u16 = 0x6A03;
    const CSYMBOL: u16 = 0x6A09;
    const PFINTABLE: u16 = 0x2416;
    const PFTTP: u16 = 0x2417;
    const PILFO: u16 = 0x460B;

    fn sprm(op: u16, operand: &[u8]) -> Vec<u8> {
        let mut v = op.to_le_bytes().to_vec();
        v.extend_from_slice(operand);
        v
    }

    fn run(cp0: u32, cp1: u32, grpprl: Vec<u8>) -> Run {
        Run {
            cp0,
            cp1,
            istd: 0,
            grpprl,
        }
    }

    fn losses(res: &ImportResult) -> Vec<(LossKind, usize)> {
        res.sorted_losses()
            .iter()
            .map(|l| (l.kind, l.count))
            .collect()
    }

    /// Runs the main story of a decoded document (one CP per UTF-16 unit of `text`) through the
    /// paragraph walker exactly as `import` does.
    fn convert(
        text: &str,
        paps: Vec<Run>,
        chps: Vec<Run>,
        lists: Lists,
        data: Vec<u8>,
        assets_dir: &str,
    ) -> ImportResult {
        let doc = Doc {
            text: text.encode_utf16().collect(),
            paps,
            chps,
            styles: Stylesheet::default(),
            lists,
            data,
            ftn_ref: HashMap::new(),
            edn_ref: HashMap::new(),
        };
        convert_story(&doc, assets_dir, Sub::Main)
    }

    /// Runs the whole text of `doc` through the paragraph walker as story `sub`.
    fn convert_story(doc: &Doc, assets_dir: &str, sub: Sub) -> ImportResult {
        let mut res = ImportResult::default();
        let mut cv = Conv {
            d: doc,
            assets_dir,
            res: &mut res,
            counts: Counts::default(),
            fields: Vec::new(),
            numbering: Numbering::default(),
            tables: Tables::default(),
            table_loss: TableLoss::default(),
            serial: 0,
            sub,
            pending_break: false,
            images: 0,
        };
        let blocks = cv.render_range(0, doc.text.len() as u32, sub);
        let markdown = md::blocks_to_string(&blocks);
        cv.finish();
        res.markdown = markdown;
        res
    }

    fn convert_plain(text: &str, chps: Vec<Run>) -> ImportResult {
        convert(text, vec![], chps, Lists::default(), vec![], "")
    }

    #[test]
    fn tracked_deletions_are_dropped_and_counted() {
        let res = convert_plain("keep gone\r", vec![run(5, 9, sprm(CFRMARKDEL, &[1]))]);
        assert_eq!(res.markdown, "keep");
        assert_eq!(losses(&res), vec![(LossKind::TrackedChanges, 4)]);
    }

    #[test]
    fn a_field_separator_without_a_field_is_ignored() {
        let res = convert_plain("a\u{14}b\r", vec![]);
        assert_eq!(res.markdown, "ab");
        assert!(res.losses.is_empty());
    }

    #[test]
    fn line_breaks_and_non_breaking_hyphens() {
        let res = convert_plain("one\u{0B}two\u{1E}three\r", vec![]);
        assert_eq!(res.markdown, "one\\\ntwo-three");
        assert!(res.losses.is_empty());
    }

    #[test]
    fn symbol_characters_map_to_unicode_and_are_reported() {
        // sprmCSymbol on the "(" at CP 1: font 1, character U+F0B7 (the Symbol-font bullet)
        let symbol = sprm(CSYMBOL, &[1, 0, 0xB7, 0xF0]);
        let res = convert_plain("x(y\r", vec![run(1, 2, symbol)]);
        assert_eq!(res.markdown, "x\u{2022}y");
        assert_eq!(losses(&res), vec![(LossKind::SymbolFontCharacters, 1)]);

        // the private-use bullets of Symbol/Wingdings, and a private-use code point that has no
        // mapping and is kept as is
        let res = convert_plain("\u{F0B7}\u{F0A7}\u{F0D8}\u{F076}\u{F0FC}\u{F021}\r", vec![]);
        assert_eq!(
            res.markdown,
            "\u{2022}\u{25AA}\u{27A2}\u{2756}\u{2713}\u{F021}"
        );
        assert_eq!(losses(&res), vec![(LossKind::SymbolFontCharacters, 6)]);
    }

    /// PICFAndOfficeArtData holding one BLIP record (`ty`, `inst`, one UID) with `payload`.
    fn picture_record(ty: u16, inst: u16, payload: &[u8]) -> Vec<u8> {
        let mut blip = Vec::new();
        blip.extend_from_slice(&(inst << 4).to_le_bytes());
        blip.extend_from_slice(&ty.to_le_bytes());
        blip.extend_from_slice(&((16 + 1 + payload.len()) as u32).to_le_bytes());
        blip.extend_from_slice(&[0; 17]);
        blip.extend_from_slice(payload);
        let mut v = Vec::new();
        v.extend_from_slice(&(8 + blip.len() as u32).to_le_bytes());
        v.extend_from_slice(&8u16.to_le_bytes());
        v.extend_from_slice(&0x64u16.to_le_bytes());
        v.extend_from_slice(&blip);
        v
    }

    #[test]
    fn picture_characters_become_images_objects_or_nothing() {
        // a form-field data mark is not a picture
        let data_mark = [sprm(CFSPEC, &[1]), sprm(CFDATA, &[1])].concat();
        let res = convert_plain("a\u{1}b\r", vec![run(1, 2, data_mark)]);
        assert_eq!(res.markdown, "ab");
        assert!(res.losses.is_empty() && res.assets.is_empty());

        // an embedded OLE object is replaced by a placeholder and reported
        let ole_mark = [sprm(CFSPEC, &[1]), sprm(CFOLE2, &[1])].concat();
        let res = convert_plain("\u{1}\r", vec![run(0, 1, ole_mark)]);
        assert_eq!(res.markdown, "*[embedded object]*");
        assert_eq!(losses(&res), vec![(LossKind::UnsupportedEquations, 1)]);

        // a raster picture is extracted next to the Markdown file, or into the assets folder
        let payload = [0x89, b'P', b'N', b'G', 1, 2];
        let pic = [sprm(CFSPEC, &[1]), sprm(CPICLOCATION, &[0; 4])].concat();
        for (dir, path) in [("", "image-1.png"), ("pics", "pics/image-1.png")] {
            let res = convert(
                "\u{1}\r",
                vec![],
                vec![run(0, 1, pic.clone())],
                Lists::default(),
                picture_record(0xF01E, 0x6E0, &payload),
                dir,
            );
            assert_eq!(res.markdown, format!("![image-1]({path})"));
            assert_eq!(
                res.assets,
                vec![Asset {
                    relative_path: path.into(),
                    bytes: payload.to_vec()
                }]
            );
            assert!(res.losses.is_empty());
        }

        // a picture mark without a stored location has nothing to extract
        let no_location = sprm(CFSPEC, &[1]);
        let res = convert_plain("a\u{1}b\r", vec![run(1, 2, no_location)]);
        assert_eq!(res.markdown, "ab");
        assert!(res.losses.is_empty() && res.assets.is_empty());
    }

    #[test]
    fn pictures_that_cannot_be_converted_are_omitted_and_reported() {
        let pic = [sprm(CFSPEC, &[1]), sprm(CPICLOCATION, &[0; 4])].concat();
        // an EMF record (vector format) and a DIB record whose bytes are not a bitmap
        let cases = [
            picture_record(0xF01A, 0x3D4, &[1, 2, 3]),
            picture_record(0xF01F, 0x7A8, &[1, 2, 3]),
        ];
        for data in cases {
            let res = convert(
                "\u{1}\r",
                vec![],
                vec![run(0, 1, pic.clone())],
                Lists::default(),
                data,
                "pics",
            );
            assert_eq!(res.markdown, "*[image omitted]*");
            assert!(res.assets.is_empty());
            assert_eq!(losses(&res), vec![(LossKind::UnsupportedImages, 1)]);
        }
    }

    #[test]
    fn internal_links_and_suppressed_fields_keep_or_drop_their_result() {
        let text = "\u{13} HYPERLINK \\l \"bm\" \u{14}jump\u{15}\r\
                    x\u{13} PAGEREF _Toc1 \u{14}12\u{15}y\r";
        let res = convert_plain(text, vec![]);
        assert_eq!(res.markdown, "jump\n\nxy");
        assert_eq!(losses(&res), vec![(LossKind::Fields, 2)]);
    }

    #[test]
    fn list_markers_inside_table_cells() {
        let nine = |first: (u32, u8)| {
            let mut v = vec![first];
            v.extend((1..9).map(|_| (1u32, 0u8)));
            v
        };
        let (t, lst, lfo) = lists::tests::tables(
            &[
                (10, false, nine((1, 0x17))),
                (20, false, nine((5, 0))),
                (40, false, nine((1, 0xFF))),
            ],
            &[10, 20, 40],
        );
        let lists = Lists::parse(&t, lst, lfo).unwrap();
        let in_table =
            |ilfo: u16| [sprm(PFINTABLE, &[1]), sprm(PILFO, &ilfo.to_le_bytes())].concat();
        // three cells (bullet, number starting at 5, a list level without any marker) and the
        // row end mark
        let paps = vec![
            run(0, 2, in_table(1)),
            run(2, 4, in_table(2)),
            run(4, 6, in_table(3)),
            run(6, 7, [sprm(PFINTABLE, &[1]), sprm(PFTTP, &[1])].concat()),
        ];
        let res = convert("a\x07b\x07c\x07\x07", paps, vec![], lists, vec![], "");
        assert_eq!(
            res.markdown,
            "| \u{2022} a | 5. b | c |\n| --- | --- | --- |"
        );
        assert!(res.losses.is_empty());
    }

    #[test]
    fn control_characters_and_field_kinds_in_a_paragraph() {
        let table = [
            // floating object anchor, tab, annotation mark, note mark without a note
            ("a\u{8}b\r", "ab"),
            ("a\tb\r", "a b"),
            ("a\u{5}b\r", "ab"),
            ("a\u{2}b\r", "ab"),
            ("a\u{c}b\r", "ab\n\n<!-- pagebreak -->"),
            // a break before any content, or right after another break, adds nothing
            ("\u{c}\r", ""),
            ("a\u{c}\r\u{c}\r", "a\n\n<!-- pagebreak -->"),
            // a picture mark without the special-character flag is not a picture
            ("a\u{1}b\r", "ab"),
            // a field end without a field, and a hyperlink whose result is empty
            ("a\u{15}b\r", "ab"),
            ("\u{13} HYPERLINK \"http://x.org\" \u{14}\u{15}\r", ""),
            (
                "\u{13} HYPERLINK \"http://x.org\" \u{14}link\u{15} \u{13} DATE \u{14}today\u{15}\r",
                "[link](http://x.org) today",
            ),
        ];
        for (text, expected) in table {
            assert_eq!(convert_plain(text, vec![]).markdown, expected, "{text:?}");
        }
        assert_eq!(
            losses(&convert_plain("a\u{8}b\r", vec![])),
            vec![(LossKind::VectorShapes, 1)]
        );
        // hidden text (sprmCFVanish) is dropped and counted
        let hidden = convert_plain("a b\r", vec![run(1, 2, sprm(0x083C, &[1]))]);
        assert_eq!(hidden.markdown, "ab");
        assert_eq!(losses(&hidden), vec![(LossKind::HiddenText, 1)]);
    }

    #[test]
    fn note_marks_are_labelled_by_kind_and_only_in_the_main_story() {
        // a footnote mark at CP 1, an endnote mark at CP 3, and a mark with no note at CP 5
        let doc = Doc {
            text: "a\u{2}b\u{2}c\u{2}d\r".encode_utf16().collect(),
            paps: vec![],
            chps: vec![],
            styles: Stylesheet::default(),
            lists: Lists::default(),
            data: vec![],
            ftn_ref: HashMap::from([(1, 1)]),
            edn_ref: HashMap::from([(3, 2)]),
        };
        assert_eq!(convert_story(&doc, "", Sub::Main).markdown, "a[^1]b[^e2]cd");
        // inside a note itself the marks are dropped
        for sub in [Sub::Footnote, Sub::Endnote] {
            assert_eq!(convert_story(&doc, "", sub).markdown, "abcd");
        }
    }

    #[test]
    fn notes_and_main_text_are_joined_only_when_both_exist() {
        // (main story, footnote story + reference CPs + text CPs, endnote story likewise)
        type Story<'a> = (&'a [u8], &'a [u32], &'a [u32]);
        let none: Story<'_> = (b"", &[], &[]);
        let cases: [(&[u8], Story<'_>, Story<'_>, &str); 4] = [
            // no notes at all
            (b"Plain\r", none, none, "Plain\n"),
            // nothing at all
            (b"\r", none, none, ""),
            // notes of both kinds around a mark that has no note
            (
                b"A\x02B\x02C\x02\r",
                (b"Foot\r", &[1], &[0, 5]),
                (b"End\r", &[3], &[0, 4]),
                "A[^1]B[^e1]C\n\n[^1]: Foot\n[^e1]: End\n",
            ),
            // an empty main story: the definition stands alone
            (b"\r", (b"Note\r", &[0], &[0, 5]), none, "[^1]: Note\n"),
        ];
        for (main, ftn, edn, want) in cases {
            let res = import(&notes_doc(main, ftn, edn), "assets").unwrap();
            let label = String::from_utf8_lossy(main);
            assert_eq!(res.markdown, want, "{label:?}");
            // the style and list tables of the document lie outside the table stream
            assert_eq!(losses(&res), vec![(LossKind::CorruptParts, 2)]);
        }
    }

    #[test]
    fn a_list_paragraph_without_a_definition_is_counted_as_corrupt() {
        let paps = vec![run(0, 5, sprm(PILFO, &7u16.to_le_bytes()))];
        let res = convert("item\r", paps, vec![], Lists::default(), vec![], "");
        assert_eq!(res.markdown, "item");
        assert_eq!(losses(&res), vec![(LossKind::CorruptParts, 1)]);
    }

    #[test]
    fn structures_outside_the_streams_are_refused() {
        let import_with = |clx: &[u8], pairs: &[(usize, (u32, u32))]| {
            let mut all = vec![(0u32, 0u32); IDX_PLF_LFO + 1];
            all[IDX_CLX] = (0, clx.len() as u32);
            for &(index, pair) in pairs {
                all[index] = pair;
            }
            let word = fib::tests::fib_bytes(0xA5EC, 0x00C1, 0, 14, 22, &all);
            import(&ole(&word, clx), "assets").err()
        };
        let clx = pieces::tests::clx(&[], &[(0, 4, 0x4000_0000, 0)]);
        // the PlcBtePapx / PlcBteChpx lie beyond the end of the table stream
        let slice_error = Some(ImportError::Corrupt(format!(
            "slice 100000+8 past {}",
            clx.len()
        )));
        for index in [IDX_PLCF_BTE_PAPX, IDX_PLCF_BTE_CHPX] {
            assert_eq!(import_with(&clx, &[(index, (100_000, 8))]), slice_error);
        }
        // so does the piece table itself
        assert_eq!(
            import_with(&clx, &[(IDX_CLX, (100_000, 8))]),
            Some(ImportError::Corrupt("CLX outside table stream".into()))
        );
        // bytes that are not a compound file never reach the streams
        assert_eq!(
            import(b"just some text, no Word file", "assets").err(),
            Some(ImportError::WrongFormat {
                expected: "DOC",
                detected: "text"
            })
        );
        // a piece table that claims far more text than the document stream holds
        let huge = pieces::tests::clx(&[], &[(0, 100_000, 0x4000_0000, 0)]);
        let refused = import_with(&huge, &[]).unwrap().to_string();
        assert!(
            refused.starts_with("the document is corrupt: piece table claims 100000 characters"),
            "{refused}"
        );
    }
}
