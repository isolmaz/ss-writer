//! Property modifiers ([MS-DOC] 2.6 Sprm/Prl), the paragraph / character / table properties
//! distilled from them (only what Markdown can use), and the PlcBteChpx / PlcBtePapx FKP pages
//! that attach them to CP ranges.

use super::fib::{Result, slice, u32_at};
use super::pieces::Pieces;

// ---- sprm iteration ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub(super) struct Sprm<'a> {
    pub op: u16,
    pub data: &'a [u8],
}

impl Sprm<'_> {
    pub fn u8(&self) -> u8 {
        self.data.first().copied().unwrap_or(0)
    }
    pub fn u16(&self) -> u16 {
        match self.data {
            [a, b, ..] => u16::from_le_bytes([*a, *b]),
            _ => 0,
        }
    }
    pub fn u32(&self) -> u32 {
        match self.data {
            [a, b, c, d, ..] => u32::from_le_bytes([*a, *b, *c, *d]),
            _ => 0,
        }
    }
}

/// Parses a grpprl. Stops (keeping what it has) at the first truncated sprm.
pub(super) fn parse_grpprl(g: &[u8]) -> Vec<Sprm<'_>> {
    let mut out = Vec::new();
    let mut o = 0usize;
    while o + 2 <= g.len() {
        let op = u16::from_le_bytes([g[o], g[o + 1]]);
        o += 2;
        let rest = &g[o..];
        // (bytes of length prefix to skip, operand length)
        let layout = match op >> 13 {
            0 | 1 => Some((0, 1)),
            2 | 4 | 5 => Some((0, 2)),
            3 => Some((0, 4)),
            7 => Some((0, 3)),
            _ => variable_layout(op, rest),
        };
        let Some((skip, len)) = layout else { break };
        if rest.len() < skip + len {
            break;
        }
        out.push(Sprm {
            op,
            data: &rest[skip..skip + len],
        });
        o += skip + len;
    }
    out
}

/// Layout of the variable-size operands (spra 6).
fn variable_layout(op: u16, rest: &[u8]) -> Option<(usize, usize)> {
    match op {
        // sprmTDefTable: u16 cb, and the operand is cb + 1 bytes in total (cb counts itself less one).
        TAP_TDEFTABLE => {
            let cb = rest.get(..2)?;
            Some((0, u16::from_le_bytes([cb[0], cb[1]]) as usize + 1))
        }
        // sprmPChgTabs: cb == 255 means the length is computed from the two tab lists.
        PAP_PCHGTABS | PAP_PCHGTABS_PAPX => {
            let cb = *rest.first()?;
            if cb != 255 {
                return Some((1, cb as usize));
            }
            let del = *rest.get(1)? as usize;
            let add = *rest.get(2 + del * 4)? as usize;
            Some((1, 1 + del * 4 + 1 + add * 3))
        }
        _ => Some((1, *rest.first()? as usize)),
    }
}

const PAP_PJC80: u16 = 0x2403;
const PAP_PJC: u16 = 0x2461;
const PAP_PFPAGEBREAKBEFORE: u16 = 0x2407;
const PAP_PFINTABLE: u16 = 0x2416;
const PAP_PFTTP: u16 = 0x2417;
const PAP_PILVL: u16 = 0x260A;
const PAP_PILFO: u16 = 0x460B;
const PAP_POUTLVL: u16 = 0x2640;
const PAP_PITAP: u16 = 0x6649;
const PAP_PFINNERTABLECELL: u16 = 0x244B;
const PAP_PFINNERTTP: u16 = 0x244C;
const PAP_PHUGEPAPX: u16 = 0x6646;
const PAP_PCHGTABS: u16 = 0xC615;
const PAP_PCHGTABS_PAPX: u16 = 0xC60D;
const TAP_TDEFTABLE: u16 = 0xD608;

const CHP_CFBOLD: u16 = 0x0835;
const CHP_CFITALIC: u16 = 0x0836;
const CHP_CFSTRIKE: u16 = 0x0837;
const CHP_CFVANISH: u16 = 0x083C;
const CHP_CFSPEC: u16 = 0x0855;
const CHP_CFDATA: u16 = 0x0806;
const CHP_CFOLE2: u16 = 0x0856;
const CHP_CFRMARKDEL: u16 = 0x0800;
const CHP_CKUL: u16 = 0x2A3E;
const CHP_CFDSTRIKE: u16 = 0x2A53;
const CHP_CISS: u16 = 0x2A48;
const CHP_CISTD: u16 = 0x4A30;
const CHP_CPICLOCATION: u16 = 0x6A03;
const CHP_CSYMBOL: u16 = 0x6A09;

// ---- paragraph / table properties ---------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CellDef {
    pub h_merged: bool,
    pub v_merge: bool,
    pub v_restart: bool,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Pap {
    pub in_table: Option<bool>,
    pub ttp: Option<bool>,
    pub itap: Option<u32>,
    pub inner_cell: Option<bool>,
    pub inner_ttp: Option<bool>,
    pub ilvl: Option<u8>,
    pub ilfo: Option<i16>,
    pub outlvl: Option<u8>,
    pub jc: Option<u8>,
    pub page_break_before: bool,
    pub cells: Option<Vec<CellDef>>,
}

impl Pap {
    pub fn apply(&mut self, grpprl: &[u8]) {
        for s in parse_grpprl(grpprl) {
            match s.op {
                PAP_PFINTABLE => self.in_table = Some(s.u8() != 0),
                PAP_PFTTP => self.ttp = Some(s.u8() != 0),
                PAP_PITAP => self.itap = Some(s.u32()),
                PAP_PFINNERTABLECELL => self.inner_cell = Some(s.u8() != 0),
                PAP_PFINNERTTP => self.inner_ttp = Some(s.u8() != 0),
                PAP_PILVL => self.ilvl = Some(s.u8()),
                PAP_PILFO => self.ilfo = Some(s.u16() as i16),
                PAP_POUTLVL => self.outlvl = Some(s.u8()),
                PAP_PJC80 | PAP_PJC => self.jc = Some(s.u8()),
                PAP_PFPAGEBREAKBEFORE => self.page_break_before = s.u8() != 0,
                TAP_TDEFTABLE => self.cells = parse_tdef(s.data),
                _ => {}
            }
        }
    }

    /// Effective table nesting depth (0 = not in a table).
    pub fn itap_eff(&self) -> u32 {
        match self.itap {
            Some(n) => n,
            None => u32::from(self.in_table == Some(true)),
        }
    }
}

/// `sprmTDefTable` operand: cb(2) itcMac(1) rgdxaCenter[(itcMac+1)*2] rgTc80[itcMac*20]
/// (the TC80 array may be truncated).
fn parse_tdef(d: &[u8]) -> Option<Vec<CellDef>> {
    let itc = *d.get(2)? as usize;
    let tc0 = 3 + (itc + 1) * 2;
    let mut v = Vec::with_capacity(itc);
    for i in 0..itc {
        let o = tc0 + i * 20;
        let Some(b) = d.get(o..o + 2) else {
            v.push(CellDef::default());
            continue;
        };
        let f = u16::from_le_bytes([b[0], b[1]]);
        v.push(CellDef {
            h_merged: f & 0x02 != 0,
            v_merge: f & 0x20 != 0,
            v_restart: f & 0x40 != 0,
        });
    }
    Some(v)
}

// ---- character properties -----------------------------------------------------------------

/// Raw toggle operand values: 0 off, 1 on, 0x80 same-as-style, 0x81 opposite-of-style.
#[derive(Debug, Clone, Default)]
pub(super) struct Chp {
    pub bold: Option<u8>,
    pub italic: Option<u8>,
    pub strike: Option<u8>,
    pub dstrike: Option<u8>,
    pub vanish: Option<u8>,
    pub del: Option<u8>,
    pub spec: Option<u8>,
    pub data: Option<u8>,
    pub ole2: Option<u8>,
    pub kul: Option<u8>,
    pub iss: Option<u8>,
    pub istd: Option<u16>,
    pub pic_loc: Option<u32>,
    /// `(font, character)` of a `sprmCSymbol`.
    pub symbol: Option<(u16, u16)>,
}

impl Chp {
    pub fn apply(&mut self, grpprl: &[u8]) {
        for s in parse_grpprl(grpprl) {
            match s.op {
                CHP_CFBOLD => self.bold = Some(s.u8()),
                CHP_CFITALIC => self.italic = Some(s.u8()),
                CHP_CFSTRIKE => self.strike = Some(s.u8()),
                CHP_CFDSTRIKE => self.dstrike = Some(s.u8()),
                CHP_CFVANISH => self.vanish = Some(s.u8()),
                CHP_CFRMARKDEL => self.del = Some(s.u8()),
                CHP_CFSPEC => self.spec = Some(s.u8()),
                CHP_CFDATA => self.data = Some(s.u8()),
                CHP_CFOLE2 => self.ole2 = Some(s.u8()),
                CHP_CKUL => self.kul = Some(s.u8()),
                CHP_CISS => self.iss = Some(s.u8()),
                CHP_CISTD => self.istd = Some(s.u16()),
                CHP_CPICLOCATION => self.pic_loc = Some(s.u32()),
                CHP_CSYMBOL => {
                    let v = s.u32();
                    self.symbol = Some((v as u16, (v >> 16) as u16));
                }
                _ => {}
            }
        }
    }
}

/// Resolves a toggle operand against the style-derived value.
pub(super) fn toggle(style: bool, raw: Option<u8>) -> bool {
    match raw {
        Some(0) => false,
        Some(1) => true,
        Some(0x81) => !style,
        _ => style,
    }
}

// ---- FKP pages ----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(super) struct Run {
    pub cp0: u32,
    pub cp1: u32,
    /// PAPX: istd of the paragraph style. CHPX: unused (0).
    pub istd: u16,
    pub grpprl: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Chpx,
    Papx,
}

/// The document streams an FKP reader needs.
pub(super) struct FkpSource<'a> {
    pub word: &'a [u8],
    pub table: &'a [u8],
    pub data: &'a [u8],
    pub pieces: &'a Pieces,
}

/// Runs of one property kind, sorted by `cp0`. Pages that cannot be parsed are skipped and
/// counted in `bad_pages`.
pub(super) fn read_runs(
    kind: Kind,
    src: &FkpSource<'_>,
    fc: u32,
    lcb: u32,
    bad_pages: &mut u32,
) -> Result<Vec<Run>> {
    if lcb < 4 {
        return Ok(Vec::new());
    }
    let plc = slice(src.table, fc as usize, lcb as usize)?;
    let n = (lcb as usize - 4) / 8;
    let mut runs = Vec::new();
    let mut cps = Vec::new();
    for i in 0..n {
        let pn = u32_at(plc, (n + 1) * 4 + i * 4)? & 0x003F_FFFF;
        let Some(page) = (pn as usize)
            .checked_mul(512)
            .and_then(|o| src.word.get(o..o + 512))
        else {
            *bad_pages += 1;
            continue;
        };
        let crun = page[511] as usize;
        let fits = match kind {
            Kind::Papx => (crun + 1) * 4 + crun * 13 <= 511,
            Kind::Chpx => (crun + 1) * 4 + crun <= 511,
        };
        if crun == 0 || !fits {
            *bad_pages += 1;
            continue;
        }
        for r in 0..crun {
            let fa = u32_at(page, r * 4)?;
            let fb = u32_at(page, (r + 1) * 4)?;
            let parsed = match kind {
                Kind::Chpx => chpx_at(page, crun, r),
                Kind::Papx => Some(papx_at(page, crun, r, src.data)),
            };
            let Some((istd, grpprl)) = parsed else {
                *bad_pages += 1;
                continue;
            };
            cps.clear();
            src.pieces.fc_range_to_cp(fa, fb, &mut cps);
            for &(c0, c1, pi) in &cps {
                // Pcd.prm modifiers apply after the FKP's own (fast-saved documents).
                let mut g = grpprl.clone();
                g.extend_from_slice(&src.pieces.pieces[pi].mods);
                runs.push(Run {
                    cp0: c0,
                    cp1: c1,
                    istd,
                    grpprl: g,
                });
            }
        }
    }
    runs.sort_by_key(|r| r.cp0);
    Ok(runs)
}

/// `(istd, grpprl)` of CHPX entry `r`; `None` when the entry runs past the page.
fn chpx_at(page: &[u8], crun: usize, r: usize) -> Option<(u16, Vec<u8>)> {
    let b = page[(crun + 1) * 4 + r] as usize * 2;
    if b == 0 {
        return Some((0, Vec::new()));
    }
    let cb = page[b] as usize;
    page.get(b + 1..b + 1 + cb).map(|g| (0, g.to_vec()))
}

/// `(istd, grpprl)` of PAPX entry `r` (a BX of 13 bytes); an empty or unreadable entry
/// yields `(0, [])`.
fn papx_at(page: &[u8], crun: usize, r: usize, data: &[u8]) -> (u16, Vec<u8>) {
    let b = page[(crun + 1) * 4 + r * 13] as usize * 2;
    if b == 0 {
        return (0, Vec::new());
    }
    let cb = page[b] as usize;
    let content = if cb != 0 {
        page.get(b + 1..b + 1 + (cb * 2 - 1))
    } else {
        page.get(b + 2..b + 2 + page[b + 1] as usize * 2)
    };
    let Some(c) = content.filter(|c| c.len() >= 2) else {
        return (0, Vec::new());
    };
    let mut g = c[2..].to_vec();
    // sprmPHugePapx: the real grpprl lives in the Data stream.
    if let Some(h) = parse_grpprl(&g)
        .into_iter()
        .find(|s| s.op == PAP_PHUGEPAPX)
        .map(|s| s.u32())
        && let Some(real) = huge_grpprl(data, h)
    {
        g = real;
    }
    (u16::from_le_bytes([c[0], c[1]]), g)
}

/// PrcData: cb(2) + (istd(2) + grpprl); returns the grpprl.
fn huge_grpprl(data: &[u8], off: u32) -> Option<Vec<u8>> {
    let o = off as usize;
    let cb = u16::from_le_bytes([*data.get(o)?, *data.get(o + 1)?]) as usize;
    let body = data.get(o + 2..o + 2 + cb)?;
    Some(body.get(2..)?.to_vec())
}

/// The run covering `cp` (runs sorted by `cp0`).
pub(super) fn find(runs: &[Run], cp: u32) -> Option<&Run> {
    let i = runs.partition_point(|r| r.cp0 <= cp);
    // Fast-saved documents can overlap runs slightly: the covering run need not be the
    // immediate predecessor.
    (i.saturating_sub(3)..i)
        .rev()
        .map(|j| &runs[j])
        .find(|r| cp < r.cp1)
}

/// Index of the first run whose `cp1` is beyond `cp` (for sequential run walking).
pub(super) fn first_at_or_after(runs: &[Run], cp: u32) -> usize {
    runs.partition_point(|r| r.cp1 <= cp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::pieces::tests::clx;

    fn sprm(op: u16, operand: &[u8]) -> Vec<u8> {
        let mut v = op.to_le_bytes().to_vec();
        v.extend_from_slice(operand);
        v
    }

    #[test]
    fn grpprl_operand_sizes() {
        let mut g = Vec::new();
        g.extend(sprm(0x0835, &[1])); // spra 0: 1 byte
        g.extend(sprm(0x2A3E, &[2])); // spra 1
        g.extend(sprm(0x4A30, &[7, 0])); // spra 2: 2 bytes
        g.extend(sprm(0x6A03, &[1, 0, 0, 0])); // spra 3: 4 bytes
        g.extend(sprm(0xE000 | 0x0001, &[1, 2, 3])); // spra 7: 3 bytes
        g.extend(sprm(0xC000 | 0x0001, &[2, 9, 9])); // spra 6: length-prefixed
        let v = parse_grpprl(&g);
        assert_eq!(v.len(), 6);
        assert_eq!((v[0].u8(), v[1].u8(), v[2].u16(), v[3].u32()), (1, 2, 7, 1));
        assert_eq!(v[4].data, &[1, 2, 3]);
        assert_eq!(v[5].data, &[9, 9]);
        // truncated operands end the list but keep what was read
        assert_eq!(parse_grpprl(&g[..g.len() - 1]).len(), 5);
        assert_eq!(parse_grpprl(&[0x35]).len(), 0);
        assert_eq!(Sprm { op: 0, data: &[] }.u8(), 0);
        assert_eq!(Sprm { op: 0, data: &[1] }.u16(), 0);
        assert_eq!(
            Sprm {
                op: 0,
                data: &[1, 2, 3]
            }
            .u32(),
            0
        );
    }

    #[test]
    fn grpprl_variable_operands() {
        // sprmTDefTable: cb counts the operand after its own two bytes, plus one
        let mut tdef = sprm(TAP_TDEFTABLE, &[3, 0, 9, 9]);
        tdef.extend(sprm(0x0835, &[1]));
        let v = parse_grpprl(&tdef);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].data.len(), 4);
        // sprmPChgTabs, plain and with the 255 escape (1 deleted, 1 added tab)
        let mut tabs = sprm(PAP_PCHGTABS, &[2, 5, 6]);
        tabs.extend(sprm(PAP_PCHGTABS, &[255, 1, 0, 0, 0, 0, 1, 0, 0, 0]));
        tabs.extend(sprm(0x0835, &[1]));
        let v = parse_grpprl(&tabs);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].data, &[5, 6]);
        assert_eq!(v[1].data.len(), 1 + 4 + 1 + 3);
        // truncated variable-size headers stop parsing
        for bad in [
            sprm(TAP_TDEFTABLE, &[1]),
            sprm(PAP_PCHGTABS, &[]),
            sprm(PAP_PCHGTABS, &[255]),
            sprm(PAP_PCHGTABS, &[255, 3]),
            sprm(0xC001, &[]),
            sprm(0xC001, &[5, 1]),
        ] {
            assert!(parse_grpprl(&bad).is_empty(), "{bad:?}");
        }
    }

    #[test]
    fn paragraph_properties() {
        let mut g = Vec::new();
        g.extend(sprm(PAP_PFINTABLE, &[1]));
        g.extend(sprm(PAP_PFTTP, &[1]));
        g.extend(sprm(PAP_PITAP, &[2, 0, 0, 0]));
        g.extend(sprm(PAP_PFINNERTABLECELL, &[1]));
        g.extend(sprm(PAP_PFINNERTTP, &[1]));
        g.extend(sprm(PAP_PILVL, &[3]));
        g.extend(sprm(PAP_PILFO, &[0xFE, 0xFF]));
        g.extend(sprm(PAP_POUTLVL, &[1]));
        g.extend(sprm(PAP_PJC80, &[2]));
        g.extend(sprm(PAP_PFPAGEBREAKBEFORE, &[1]));
        g.extend(sprm(0x2000 | 0x0123, &[0]));
        let mut p = Pap::default();
        assert_eq!(p.itap_eff(), 0);
        p.apply(&g);
        assert_eq!(
            (p.in_table, p.ttp, p.itap, p.inner_cell, p.inner_ttp),
            (Some(true), Some(true), Some(2), Some(true), Some(true))
        );
        assert_eq!(
            (p.ilvl, p.ilfo, p.outlvl, p.jc, p.page_break_before),
            (Some(3), Some(-2), Some(1), Some(2), true)
        );
        assert_eq!(p.itap_eff(), 2);
        let mut q = Pap::default();
        q.apply(&sprm(PAP_PFINTABLE, &[1]));
        assert_eq!(q.itap_eff(), 1);
        q.apply(&sprm(PAP_PJC, &[1]));
        assert_eq!(q.jc, Some(1));
    }

    #[test]
    fn table_cell_definitions() {
        // itcMac = 3, rgdxaCenter (4 * 2 bytes), then TC80 entries; the last one is truncated away
        let mut operand = vec![0, 0, 3];
        operand.extend_from_slice(&[0; 8]);
        for flags in [0x0002u16, 0x0020 | 0x0040] {
            let mut tc = flags.to_le_bytes().to_vec();
            tc.extend_from_slice(&[0; 18]);
            operand.extend_from_slice(&tc);
        }
        let cells = parse_tdef(&operand).unwrap();
        assert_eq!(
            cells,
            vec![
                CellDef {
                    h_merged: true,
                    v_merge: false,
                    v_restart: false
                },
                CellDef {
                    h_merged: false,
                    v_merge: true,
                    v_restart: true
                },
                CellDef::default(),
            ]
        );
        assert!(parse_tdef(&[0, 0]).is_none());
        let mut p = Pap::default();
        let cb = (operand.len() - 1) as u16;
        operand[0..2].copy_from_slice(&cb.to_le_bytes());
        p.apply(&sprm(TAP_TDEFTABLE, &operand));
        assert_eq!(p.cells.map(|c| c.len()), Some(3));
    }

    #[test]
    fn character_properties_and_toggles() {
        let mut g = Vec::new();
        for (op, v) in [
            (CHP_CFBOLD, 1u8),
            (CHP_CFITALIC, 0x81),
            (CHP_CFSTRIKE, 1),
            (CHP_CFDSTRIKE, 1),
            (CHP_CFVANISH, 1),
            (CHP_CFRMARKDEL, 1),
            (CHP_CFSPEC, 1),
            (CHP_CFDATA, 1),
            (CHP_CFOLE2, 1),
            (CHP_CKUL, 1),
            (CHP_CISS, 2),
        ] {
            g.extend(sprm(op, &[v]));
        }
        g.extend(sprm(CHP_CISTD, &[9, 0]));
        g.extend(sprm(CHP_CPICLOCATION, &[5, 0, 0, 0]));
        g.extend(sprm(CHP_CSYMBOL, &[1, 0, 0x41, 0xF0]));
        g.extend(sprm(CHP_CSYMBOL, &[1, 0]));
        let mut c = Chp::default();
        c.apply(&g);
        assert_eq!(
            (c.bold, c.italic, c.strike, c.dstrike, c.vanish, c.del),
            (Some(1), Some(0x81), Some(1), Some(1), Some(1), Some(1))
        );
        assert_eq!(
            (c.spec, c.data, c.ole2, c.kul, c.iss, c.istd),
            (Some(1), Some(1), Some(1), Some(1), Some(2), Some(9))
        );
        assert_eq!((c.pic_loc, c.symbol), (Some(5), Some((1, 0xF041))));
        assert!(
            toggle(false, Some(1))
                && !toggle(true, Some(0))
                && toggle(false, Some(0x81))
                && !toggle(true, Some(0x81))
        );
        assert!(toggle(true, Some(0x80)) && toggle(true, None) && !toggle(false, None));
    }

    /// One FKP page with `crun` entries; `entries[i]` is the already-placed grpprl bytes
    /// (without the length byte) for CHPX or `(istd, grpprl)` for PAPX.
    fn chpx_page(runs: &[(u32, u32, &[u8])]) -> Vec<u8> {
        let mut page = vec![0u8; 512];
        let crun = runs.len();
        let mut at = 300usize;
        for (i, (fa, fb, g)) in runs.iter().enumerate() {
            page[i * 4..i * 4 + 4].copy_from_slice(&fa.to_le_bytes());
            page[(i + 1) * 4..(i + 1) * 4 + 4].copy_from_slice(&fb.to_le_bytes());
            page[(crun + 1) * 4 + i] = (at / 2) as u8;
            page[at] = g.len() as u8;
            page[at + 1..at + 1 + g.len()].copy_from_slice(g);
            at += 2 + g.len() + g.len() % 2;
        }
        page[511] = crun as u8;
        page
    }

    fn papx_page(runs: &[(u32, u32, u16, &[u8])]) -> Vec<u8> {
        let mut page = vec![0u8; 512];
        let crun = runs.len();
        let mut at = 300usize;
        for (i, (fa, fb, istd, g)) in runs.iter().enumerate() {
            page[i * 4..i * 4 + 4].copy_from_slice(&fa.to_le_bytes());
            page[(i + 1) * 4..(i + 1) * 4 + 4].copy_from_slice(&fb.to_le_bytes());
            page[(crun + 1) * 4 + i * 13] = (at / 2) as u8;
            let total = 2 + g.len();
            page[at] = (total / 2 + 1) as u8;
            page[at + 1..at + 3].copy_from_slice(&istd.to_le_bytes());
            page[at + 3..at + 3 + g.len()].copy_from_slice(g);
            at += 2 + total + total % 2;
        }
        page[511] = crun as u8;
        page
    }

    /// WordDocument stream = 512 zero bytes followed by `page` (page number 1) and a table
    /// stream holding a PlcBte that points at it.
    fn fkp_fixture(page: &[u8]) -> (Vec<u8>, Vec<u8>, Pieces) {
        let mut word = vec![0u8; 512];
        word.extend_from_slice(page);
        let mut table = Vec::new();
        table.extend_from_slice(&0u32.to_le_bytes());
        table.extend_from_slice(&1000u32.to_le_bytes());
        table.extend_from_slice(&1u32.to_le_bytes());
        let c = clx(&[], &[(0, 1000, 0x4000_0000, 0)]);
        let pieces = Pieces::parse(&c, 0, c.len() as u32).unwrap();
        (word, table, pieces)
    }

    #[test]
    fn reads_chpx_runs_and_maps_them_to_cps() {
        let page = chpx_page(&[
            (0, 4, &[0x35, 0x08, 1]),
            (4, 9, &[]),
            (9, 12, &[0x36, 0x08, 1]),
        ]);
        let (word, table, pieces) = fkp_fixture(&page);
        let src = FkpSource {
            word: &word,
            table: &table,
            data: &[],
            pieces: &pieces,
        };
        let mut bad = 0;
        let runs = read_runs(Kind::Chpx, &src, 0, table.len() as u32, &mut bad).unwrap();
        assert_eq!(bad, 0);
        assert_eq!(
            runs.iter().map(|r| (r.cp0, r.cp1)).collect::<Vec<_>>(),
            vec![(0, 4), (4, 9), (9, 12)]
        );
        assert_eq!(runs[0].grpprl, vec![0x35, 0x08, 1]);
        assert!(runs[1].grpprl.is_empty());
        assert_eq!(find(&runs, 5).map(|r| r.cp0), Some(4));
        assert_eq!(find(&runs, 12).map(|r| r.cp0), None);
        assert_eq!(first_at_or_after(&runs, 4), 1);
        assert_eq!(first_at_or_after(&runs, 100), 3);
    }

    #[test]
    fn reads_papx_runs_with_huge_data_and_bad_pages() {
        let huge = sprm(PAP_PHUGEPAPX, &[0, 0, 0, 0]);
        let page = papx_page(&[(0, 5, 3, &sprm(PAP_PILVL, &[1])), (5, 8, 4, &huge)]);
        let (word, table, pieces) = fkp_fixture(&page);
        // Data stream: cb = 2 + 3 (istd + grpprl), istd 0, grpprl = sprmPFInTable 1
        let mut data = vec![5, 0, 0, 0];
        data.extend(sprm(PAP_PFINTABLE, &[1]));
        let src = FkpSource {
            word: &word,
            table: &table,
            data: &data,
            pieces: &pieces,
        };
        let mut bad = 0;
        let runs = read_runs(Kind::Papx, &src, 0, table.len() as u32, &mut bad).unwrap();
        assert_eq!(bad, 0);
        assert_eq!(
            (runs[0].istd, runs[0].grpprl.clone()),
            (3, sprm(PAP_PILVL, &[1]))
        );
        assert_eq!(
            (runs[1].istd, runs[1].grpprl.clone()),
            (4, sprm(PAP_PFINTABLE, &[1]))
        );
        // without the Data stream the sprmPHugePapx stays as is
        let src = FkpSource {
            word: &word,
            table: &table,
            data: &[],
            pieces: &pieces,
        };
        let runs = read_runs(Kind::Papx, &src, 0, table.len() as u32, &mut bad).unwrap();
        assert_eq!(runs[1].grpprl[..huge.len()], huge[..]);
        // page number pointing outside the stream
        let mut far = table.clone();
        far[8..12].copy_from_slice(&900u32.to_le_bytes());
        let mut bad = 0;
        let src = FkpSource {
            word: &word,
            table: &far,
            data: &[],
            pieces: &pieces,
        };
        assert!(
            read_runs(Kind::Papx, &src, 0, far.len() as u32, &mut bad)
                .unwrap()
                .is_empty()
        );
        assert_eq!(bad, 1);
        // too short / zero entries / overlong entries
        assert!(
            read_runs(Kind::Papx, &src, 0, 3, &mut bad)
                .unwrap()
                .is_empty()
        );
        assert!(read_runs(Kind::Papx, &src, 100_000, 20, &mut bad).is_err());
        for crun in [0u8, 255] {
            let mut page2 = page.clone();
            page2[511] = crun;
            let (word2, table2, pieces2) = fkp_fixture(&page2);
            let src = FkpSource {
                word: &word2,
                table: &table2,
                data: &[],
                pieces: &pieces2,
            };
            let mut bad = 0;
            read_runs(Kind::Papx, &src, 0, table2.len() as u32, &mut bad).unwrap();
            assert_eq!(bad, 1);
        }
    }

    #[test]
    fn entries_outside_the_page_degrade() {
        // CHPX entry whose length runs past the page is dropped and counted
        let mut page = chpx_page(&[(0, 4, &[0x35, 0x08, 1])]);
        page[(1 + 1) * 4] = 255; // offset 510: its length byte is 0 -> an empty entry
        let (word, table, pieces) = fkp_fixture(&page);
        let src = FkpSource {
            word: &word,
            table: &table,
            data: &[],
            pieces: &pieces,
        };
        let mut bad = 0;
        let runs = read_runs(Kind::Chpx, &src, 0, table.len() as u32, &mut bad).unwrap();
        assert_eq!((runs.len(), runs[0].grpprl.len(), bad), (1, 0, 0));
        let mut page = chpx_page(&[(0, 4, &[0x35, 0x08, 1])]);
        page[(1 + 1) * 4] = 254; // offset 508
        page[508] = 200; // length beyond the page
        let (word, table, pieces) = fkp_fixture(&page);
        let src = FkpSource {
            word: &word,
            table: &table,
            data: &[],
            pieces: &pieces,
        };
        assert!(
            read_runs(Kind::Chpx, &src, 0, table.len() as u32, &mut bad)
                .unwrap()
                .is_empty()
        );
        assert_eq!(bad, 1);
        // PAPX entry too short to hold an istd is treated as empty
        let mut page = papx_page(&[(0, 4, 1, &[])]);
        page[300] = 0;
        page[301] = 0;
        let (word, table, pieces) = fkp_fixture(&page);
        let src = FkpSource {
            word: &word,
            table: &table,
            data: &[],
            pieces: &pieces,
        };
        let runs = read_runs(Kind::Papx, &src, 0, table.len() as u32, &mut bad).unwrap();
        assert_eq!((runs[0].istd, runs[0].grpprl.len()), (0, 0));
    }

    #[test]
    fn find_ignores_gaps() {
        let runs = vec![
            Run {
                cp0: 0,
                cp1: 3,
                istd: 0,
                grpprl: vec![],
            },
            Run {
                cp0: 5,
                cp1: 8,
                istd: 1,
                grpprl: vec![],
            },
        ];
        assert_eq!(find(&runs, 4).map(|r| r.istd), None);
        assert_eq!(find(&runs, 5).map(|r| r.istd), Some(1));
        assert_eq!(find(&[], 0).map(|r| r.istd), None);
    }
}
