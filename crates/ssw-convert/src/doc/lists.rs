//! [MS-DOC] PlfLst (list definitions) + PlfLfo (list instances): bullet vs numbered per level,
//! and the running counters that number the items of ordered lists.

use std::collections::HashMap;

use super::fib::{Result, slice, u8_at, u16_at, u32_at};

#[derive(Debug, Clone)]
pub(super) struct Level {
    pub start_at: i32,
    /// Number format code: 0 arabic, 1 upper roman, 2 lower roman, 3 upper letter,
    /// 4 lower letter, 0x17 bullet, 0xFF none.
    pub nfc: u8,
}

#[derive(Debug, Clone)]
struct ListDef {
    levels: Vec<Level>,
}

#[derive(Debug, Default)]
pub(super) struct Lists {
    lists: HashMap<u32, ListDef>,
    /// PlfLfo: `lsid` per LFO (`ilfo` is a 1-based index).
    lfo_lsid: Vec<u32>,
}

/// What a paragraph's list properties resolve to.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Marker {
    /// Not a list item (no list, or a level without a number or bullet).
    None,
    /// The list or level definition is missing from the tables.
    Dangling,
    Bullet,
    /// `format` is the `nfc` of the level; anything but 0 (arabic) is flattened to digits.
    Number {
        n: i32,
        format: u8,
    },
}

/// Per-list, per-level item counters.
#[derive(Debug, Default)]
pub(super) struct Numbering {
    counters: HashMap<i16, [Option<i32>; 9]>,
}

impl Lists {
    pub fn parse(table: &[u8], lst: (u32, u32), lfo: (u32, u32)) -> Result<Lists> {
        let mut out = Lists::default();
        if lst.1 >= 2 {
            // lcbPlfLst only covers cLst + LSTF[]; the per-list LVL arrays follow it in the
            // Table stream up to PlfLfo (or the end of the stream when PlfLfo lies elsewhere).
            let start = lst.0 as usize;
            let stop = if lfo.1 >= 4 && lfo.0 as usize > start {
                (lfo.0 as usize).min(table.len())
            } else {
                table.len()
            };
            let b = slice(table, start, stop.saturating_sub(start).max(lst.1 as usize))?;
            let c_lst = u16_at(b, 0)? as usize;
            let mut o = 2usize;
            let mut heads = Vec::new();
            for _ in 0..c_lst {
                let lsid = u32_at(b, o)?;
                let flags = u8_at(b, o + 26)?; // after lsid(4) tplc(4) rgistd(18)
                heads.push((lsid, flags & 1 != 0));
                o += 28;
            }
            // A truncated or garbled level array must not discard the lists already parsed.
            'lists: for (lsid, simple) in heads {
                let n = if simple { 1 } else { 9 };
                let mut levels = Vec::new();
                for _ in 0..n {
                    // LVLF (28): iStartAt(4) nfc(1) flags(1) rgbxchNums(9) ixchFollow(1)
                    // dxaIndentSav(4) unused(4) cbGrpprlChpx(1) cbGrpprlPapx(1) ilvlRestartLim(1) grfhic(1)
                    let (Ok(start_at), Ok(nfc), Ok(cb_chpx), Ok(cb_papx)) = (
                        u32_at(b, o),
                        u8_at(b, o + 4),
                        u8_at(b, o + 24),
                        u8_at(b, o + 25),
                    ) else {
                        break 'lists;
                    };
                    o += 28 + cb_papx as usize + cb_chpx as usize;
                    let Ok(cch) = u16_at(b, o) else { break 'lists };
                    // the level text (xst) is not needed: only bullet vs number matters
                    o += 2 + cch as usize * 2;
                    levels.push(Level {
                        start_at: start_at as i32,
                        nfc,
                    });
                }
                out.lists.insert(lsid, ListDef { levels });
            }
        }
        if lfo.1 >= 4 {
            let b = slice(table, lfo.0 as usize, lfo.1 as usize)?;
            let n = u32_at(b, 0)? as usize;
            for i in 0..n.min((b.len() - 4) / 16) {
                out.lfo_lsid.push(u32_at(b, 4 + i * 16)?);
            }
        }
        Ok(out)
    }

    /// The level of list instance `ilfo` (1-based, positive) at depth `ilvl`.
    fn level(&self, ilfo: u16, ilvl: u8) -> Option<&Level> {
        let def = self.lists.get(self.lfo_lsid.get(usize::from(ilfo) - 1)?)?;
        def.levels
            .get((ilvl as usize).min(def.levels.len().saturating_sub(1)))
    }

    /// Resolves the list properties `(ilfo, ilvl)` of a paragraph, advancing the counters.
    pub fn marker(&self, ilfo: i16, ilvl: u8, numbering: &mut Numbering) -> Marker {
        // 0 = no list; negative values are legacy "special" markers that are not interpreted
        if ilfo <= 0 {
            return Marker::None;
        }
        let Some(level) = self.level(ilfo.unsigned_abs(), ilvl) else {
            return Marker::Dangling;
        };
        match level.nfc {
            0xFF => Marker::None,
            0x17 => Marker::Bullet,
            format => Marker::Number {
                n: numbering.next(ilfo, ilvl.min(8), level.start_at),
                format,
            },
        }
    }
}

impl Numbering {
    /// The number of the next item at `lvl` of list instance `ilfo`; deeper levels restart.
    fn next(&mut self, ilfo: i16, lvl: u8, start_at: i32) -> i32 {
        let c = self.counters.entry(ilfo).or_insert([None; 9]);
        let n = c[lvl as usize].map_or(start_at, |n| n.saturating_add(1));
        c[lvl as usize] = Some(n);
        for deeper in &mut c[lvl as usize + 1..] {
            *deeper = None;
        }
        n
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// `(lsid, simple, [(start_at, nfc)])`.
    pub(crate) type ListDef = (u32, bool, Vec<(u32, u8)>);
    /// `(table, lst, lfo)`: the table bytes plus the `(fc, lcb)` pairs of the two structures.
    pub(crate) type Tables = (Vec<u8>, (u32, u32), (u32, u32));

    /// A PlfLst (heads + LVL arrays) followed by a PlfLfo; returns `(table, lst, lfo)`.
    /// `lists`: `(lsid, simple, [(start_at, nfc)])`; `lfos`: lsid per list instance.
    pub(crate) fn tables(lists: &[ListDef], lfos: &[u32]) -> Tables {
        let mut t = Vec::new();
        t.extend_from_slice(&(lists.len() as u16).to_le_bytes());
        for (lsid, simple, _) in lists {
            let mut head = vec![0u8; 28];
            head[0..4].copy_from_slice(&lsid.to_le_bytes());
            head[26] = u8::from(*simple);
            t.extend_from_slice(&head);
        }
        let lcb_lst = t.len() as u32;
        for (_, _, levels) in lists {
            for (start, nfc) in levels {
                let mut lvl = vec![0u8; 28];
                lvl[0..4].copy_from_slice(&start.to_le_bytes());
                lvl[4] = *nfc;
                lvl[24] = 1; // cbGrpprlChpx
                lvl[25] = 2; // cbGrpprlPapx
                t.extend_from_slice(&lvl);
                t.extend_from_slice(&[0xAA; 3]);
                t.extend_from_slice(&1u16.to_le_bytes());
                t.extend_from_slice(&[0x2022u16 as u8, 0x20]);
            }
        }
        let lfo_at = t.len() as u32;
        t.extend_from_slice(&(lfos.len() as u32).to_le_bytes());
        for lsid in lfos {
            let mut e = vec![0u8; 16];
            e[0..4].copy_from_slice(&lsid.to_le_bytes());
            t.extend_from_slice(&e);
        }
        let lcb_lfo = t.len() as u32 - lfo_at;
        (t, (0, lcb_lst), (lfo_at, lcb_lfo))
    }

    fn lists() -> Lists {
        let nine = |first: (u32, u8)| {
            let mut v = vec![first];
            v.extend((1..9).map(|_| (1u32, 0u8)));
            v
        };
        let (t, lst, lfo) = tables(
            &[
                (10, false, nine((1, 0x17))),
                (20, false, nine((5, 0))),
                (30, true, vec![(1, 2)]),
                (40, false, nine((1, 0xFF))),
            ],
            &[10, 20, 30, 40, 99],
        );
        Lists::parse(&t, lst, lfo).unwrap()
    }

    #[test]
    fn resolves_markers() {
        let l = lists();
        let mut n = Numbering::default();
        assert_eq!(l.marker(1, 0, &mut n), Marker::Bullet);
        assert_eq!(l.marker(2, 0, &mut n), Marker::Number { n: 5, format: 0 });
        assert_eq!(l.marker(2, 0, &mut n), Marker::Number { n: 6, format: 0 });
        assert_eq!(l.marker(2, 1, &mut n), Marker::Number { n: 1, format: 0 });
        assert_eq!(l.marker(2, 1, &mut n), Marker::Number { n: 2, format: 0 });
        // a new item at the outer level restarts the inner one
        assert_eq!(l.marker(2, 0, &mut n), Marker::Number { n: 7, format: 0 });
        assert_eq!(l.marker(2, 1, &mut n), Marker::Number { n: 1, format: 0 });
        // a one-level list reuses its only level for deeper ilvl, and keeps its format
        assert_eq!(l.marker(3, 4, &mut n), Marker::Number { n: 1, format: 2 });
        assert_eq!(l.marker(4, 0, &mut n), Marker::None);
        // ilvl above 8 is clamped for counting
        assert_eq!(l.marker(2, 12, &mut n), Marker::Number { n: 1, format: 0 });
    }

    #[test]
    fn unresolvable_references() {
        let l = lists();
        let mut n = Numbering::default();
        for ilfo in [0, -1, i16::MIN] {
            assert_eq!(l.marker(ilfo, 0, &mut n), Marker::None);
        }
        assert_eq!(
            l.marker(5, 0, &mut n),
            Marker::Dangling,
            "lsid 99 has no definition"
        );
        assert_eq!(l.marker(6, 0, &mut n), Marker::Dangling, "no such LFO");
    }

    #[test]
    fn garbled_tables_degrade_without_failing() {
        // no tables at all
        let l = Lists::parse(&[], (0, 0), (0, 0)).unwrap();
        assert_eq!(l.marker(1, 0, &mut Numbering::default()), Marker::Dangling);
        // level array cut off in the middle: the list is dropped, earlier ones are kept
        let (t, lst, lfo) = tables(&[(1, true, vec![(1, 0)]), (2, true, vec![(1, 0)])], &[1, 2]);
        let cut_at = lst.1 as usize + 35 + 10; // inside the second list's LVL
        let mut cut = t[..cut_at].to_vec();
        cut.extend_from_slice(&t[lfo.0 as usize..]);
        let l = Lists::parse(&cut, lst, (cut_at as u32, lfo.1)).unwrap();
        let mut n = Numbering::default();
        assert_eq!(l.marker(1, 0, &mut n), Marker::Number { n: 1, format: 0 });
        assert_eq!(l.marker(2, 0, &mut n), Marker::Dangling);
        // truncated heads / out-of-range pointers are errors, not panics
        assert!(Lists::parse(&t, (u32::MAX, 10), (0, 0)).is_err());
        assert!(Lists::parse(&t, lst, (u32::MAX, 10)).is_err());
        assert!(
            Lists::parse(&[1, 0, 0], (0, 3), (0, 0)).is_err(),
            "head cut off"
        );
        for n in 0..t.len() {
            let _ = Lists::parse(&t[..n], (0, lst.1.min(n as u32)), (lfo.0.min(n as u32), 0));
        }
    }
}
