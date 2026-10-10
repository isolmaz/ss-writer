//! [MS-DOC] 2.9.271 STSH: style names, `sti` (language-independent built-in id), base chain and
//! the PAPX/CHPX of the styles.

use super::fib::{Result, slice, u16_at};
use super::props::{Chp, Pap};

#[derive(Debug, Clone, Default)]
pub(super) struct Style {
    /// Language-independent built-in style id (1..=9 are the headings, 62 is Title).
    pub sti: u16,
    pub base: Option<u16>,
    pub papx: Vec<u8>,
    pub chpx: Vec<u8>,
}

#[derive(Debug, Default)]
pub(super) struct Stylesheet {
    pub styles: Vec<Option<Style>>,
}

pub(super) const STI_TITLE: u16 = 62;
const MAX_STYLES: usize = 4096;

impl Stylesheet {
    pub fn parse(table: &[u8], fc: u32, lcb: u32) -> Result<Stylesheet> {
        if lcb == 0 {
            return Ok(Stylesheet::default());
        }
        let all = slice(table, fc as usize, lcb as usize)?;
        let cb_stshi = u16_at(all, 0)? as usize;
        let cstd = u16_at(all, 2)? as usize;
        let cb_base = u16_at(all, 4)? as usize;
        let mut off = 2 + cb_stshi;
        let mut styles = Vec::with_capacity(cstd.min(MAX_STYLES));
        for _ in 0..cstd.min(MAX_STYLES) {
            let Ok(cb_std) = u16_at(all, off) else { break };
            off += 2;
            let cb_std = cb_std as usize;
            if cb_std == 0 {
                styles.push(None);
                continue;
            }
            let Ok(std) = slice(all, off, cb_std) else {
                break;
            };
            off += cb_std;
            styles.push(parse_std(std, cb_base));
        }
        Ok(Stylesheet { styles })
    }

    pub fn get(&self, istd: u16) -> Option<&Style> {
        self.styles.get(istd as usize).and_then(Option::as_ref)
    }

    /// Style ids root -> leaf (cycle and depth safe).
    pub fn chain(&self, istd: u16) -> Vec<u16> {
        let mut v = Vec::new();
        let mut cur = Some(istd);
        while let Some(i) = cur {
            if v.len() > 24 || v.contains(&i) {
                break;
            }
            let Some(s) = self.get(i) else { break };
            v.push(i);
            cur = s.base;
        }
        v.reverse();
        v
    }

    /// The paragraph properties produced by the style chain (no direct formatting).
    pub fn style_pap(&self, istd: u16) -> Pap {
        let mut p = Pap::default();
        for s in self.chain(istd).into_iter().filter_map(|i| self.get(i)) {
            p.apply(&s.papx);
        }
        p
    }

    /// The character properties produced by the paragraph (or character) style chain.
    pub fn style_chp(&self, istd: u16) -> Chp {
        let mut c = Chp::default();
        for s in self.chain(istd).into_iter().filter_map(|i| self.get(i)) {
            let mut n = Chp::default();
            n.apply(&s.chpx);
            // Derived styles override their base; a toggle in a style is relative to its base.
            merge_toggle(&mut c.bold, n.bold);
            merge_toggle(&mut c.italic, n.italic);
            merge_toggle(&mut c.strike, n.strike);
            merge_toggle(&mut c.dstrike, n.dstrike);
            merge_toggle(&mut c.vanish, n.vanish);
            c.kul = n.kul.or(c.kul);
            c.iss = n.iss.or(c.iss);
        }
        c
    }

    /// Heading level 1..=9 of a built-in heading style or of a style derived from one.
    pub fn heading_level(&self, istd: u16) -> Option<u8> {
        self.chain(istd)
            .iter()
            .rev()
            .filter_map(|i| self.get(*i))
            .find(|s| (1..=9).contains(&s.sti))
            .map(|s| s.sti as u8)
    }

    pub fn is_title(&self, istd: u16) -> bool {
        self.get(istd).is_some_and(|s| s.sti == STI_TITLE)
    }
}

fn merge_toggle(dst: &mut Option<u8>, new: Option<u8>) {
    match (*dst, new) {
        (_, None) | (_, Some(0x80)) => {}
        (Some(1), Some(0x81)) => *dst = Some(0),
        (Some(0) | None, Some(0x81)) => *dst = Some(1),
        (_, Some(v)) => *dst = Some(v),
    }
}

fn parse_std(std: &[u8], cb_base: usize) -> Option<Style> {
    let w0 = u16_at(std, 0).ok()?;
    let w1 = u16_at(std, 2).ok()?;
    let w2 = u16_at(std, 4).ok()?;
    let sti = w0 & 0x0FFF;
    let stk = w1 & 0x000F;
    let base = w1 >> 4;
    let cupx = (w2 & 0x000F) as usize;
    // xstzName: cch(2) + UTF-16 characters + terminator(2); the name itself is not needed.
    let cch = u16_at(std, cb_base).ok()? as usize;
    let mut off = cb_base + 2 + cch * 2 + 2;
    let mut upxs: Vec<&[u8]> = Vec::new();
    for _ in 0..cupx {
        let Ok(cb) = u16_at(std, off) else { break };
        let cb = cb as usize;
        let Some(u) = std.get(off + 2..off + 2 + cb) else {
            break;
        };
        upxs.push(u);
        off += 2 + cb + (cb & 1);
    }
    let (mut papx, mut chpx) = (Vec::new(), Vec::new());
    match stk {
        // paragraph style: UpxPapx (istd + grpprl) then UpxChpx
        1 => {
            papx = upxs
                .first()
                .and_then(|u| u.get(2..))
                .unwrap_or_default()
                .to_vec();
            chpx = upxs.get(1).copied().unwrap_or_default().to_vec();
        }
        // character style: UpxChpx
        2 => chpx = upxs.first().copied().unwrap_or_default().to_vec(),
        _ => {}
    }
    Some(Style {
        sti,
        base: if base == 0x0FFF { None } else { Some(base) },
        papx,
        chpx,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A style definition: `(sti, stk, istd_base, papx grpprl, chpx grpprl)`.
    pub(crate) type StyleSpec = (u16, u16, u16, Vec<u8>, Vec<u8>);

    /// A full STSH stream for `styles` (`None` = empty slot). `cb_base` is 10 as in Word 97.
    pub(crate) fn stsh(styles: &[Option<StyleSpec>]) -> Vec<u8> {
        let cb_stshi = 18u16;
        let mut v = Vec::new();
        v.extend_from_slice(&cb_stshi.to_le_bytes());
        v.extend_from_slice(&(styles.len() as u16).to_le_bytes());
        v.extend_from_slice(&10u16.to_le_bytes());
        v.resize(2 + cb_stshi as usize, 0);
        for s in styles {
            let Some((sti, stk, base, papx, chpx)) = s else {
                v.extend_from_slice(&0u16.to_le_bytes());
                continue;
            };
            let mut std = Vec::new();
            std.extend_from_slice(&sti.to_le_bytes());
            std.extend_from_slice(&(stk | base << 4).to_le_bytes());
            let cupx: u16 = if *stk == 1 { 2 } else { 1 };
            std.extend_from_slice(&cupx.to_le_bytes());
            std.resize(10, 0);
            std.extend_from_slice(&1u16.to_le_bytes()); // name "x"
            std.extend_from_slice(&[b'x', 0, 0, 0]);
            let mut upxs: Vec<Vec<u8>> = Vec::new();
            if *stk == 1 {
                let mut p = vec![0, 0];
                p.extend_from_slice(papx);
                upxs.push(p);
            }
            upxs.push(chpx.clone());
            for u in upxs {
                std.extend_from_slice(&(u.len() as u16).to_le_bytes());
                std.extend_from_slice(&u);
                if u.len() % 2 == 1 {
                    std.push(0);
                }
            }
            v.extend_from_slice(&(std.len() as u16).to_le_bytes());
            v.extend_from_slice(&std);
        }
        v
    }

    fn sheet() -> Stylesheet {
        let table = stsh(&[
            Some((0, 1, 0x0FFF, vec![], vec![])), // 0 Normal
            Some((1, 1, 0, vec![0x40, 0x26, 0], vec![0x35, 0x08, 1])), // 1 Heading 1, bold
            Some((62, 1, 0, vec![], vec![])),     // 2 Title
            None,                                 // 3 empty slot
            Some((0, 1, 1, vec![], vec![0x36, 0x08, 1])), // 4 custom, based on Heading 1
            Some((0, 2, 0x0FFF, vec![], vec![0x36, 0x08, 0x81])), // 5 character style
            Some((0, 1, 6, vec![], vec![])),      // 6 based on itself
            Some((0, 3, 0x0FFF, vec![], vec![])), // 7 table style
        ]);
        Stylesheet::parse(&table, 0, table.len() as u32).unwrap()
    }

    #[test]
    fn parses_names_chains_and_headings() {
        let s = sheet();
        assert_eq!(s.styles.len(), 8);
        assert!(s.get(3).is_none() && s.get(99).is_none());
        assert_eq!(s.get(0).unwrap().base, None);
        assert_eq!(s.chain(4), vec![0, 1, 4]);
        assert_eq!(s.chain(6), vec![6], "cycles terminate");
        assert_eq!(s.chain(99), Vec::<u16>::new());
        assert_eq!(s.heading_level(1), Some(1));
        assert_eq!(s.heading_level(4), Some(1), "derived from a heading");
        assert_eq!(s.heading_level(0), None);
        assert!(s.is_title(2) && !s.is_title(1) && !s.is_title(99));
        assert_eq!(s.style_pap(1).outlvl, Some(0));
        assert_eq!(s.style_pap(4).outlvl, Some(0), "inherited");
        assert_eq!(s.style_pap(0).outlvl, None);
    }

    #[test]
    fn character_properties_follow_the_chain() {
        let s = sheet();
        let h1 = s.style_chp(1);
        assert_eq!((h1.bold, h1.italic), (Some(1), None));
        let derived = s.style_chp(4);
        assert_eq!((derived.bold, derived.italic), (Some(1), Some(1)));
        // 0x81 flips relative to the base (none -> on)
        assert_eq!(s.style_chp(5).italic, Some(1));
        assert_eq!(
            s.style_chp(7).bold,
            None,
            "table styles carry no character properties"
        );
    }

    #[test]
    fn toggle_merging() {
        let cases: [(Option<u8>, Option<u8>, Option<u8>); 8] = [
            (Some(1), None, Some(1)),
            (Some(1), Some(0x80), Some(1)),
            (Some(1), Some(0x81), Some(0)),
            (Some(0), Some(0x81), Some(1)),
            (None, Some(0x81), Some(1)),
            (Some(1), Some(0), Some(0)),
            (None, Some(1), Some(1)),
            (None, None, None),
        ];
        for (old, new, want) in cases {
            let mut d = old;
            merge_toggle(&mut d, new);
            assert_eq!(d, want, "{old:?} + {new:?}");
        }
    }

    #[test]
    fn tolerates_missing_and_damaged_tables() {
        assert!(Stylesheet::parse(&[], 0, 0).unwrap().styles.is_empty());
        assert!(Stylesheet::parse(&[1, 2], 0, 10).is_err());
        let good = stsh(&[
            Some((1, 1, 0, vec![], vec![])),
            Some((1, 1, 0, vec![], vec![])),
        ]);
        // stylesheet cut inside the second STD: the first is kept
        let cut = Stylesheet::parse(&good[..good.len() - 4], 0, (good.len() - 4) as u32).unwrap();
        assert_eq!(cut.styles.len(), 1);
        // header only
        let hdr = Stylesheet::parse(&good[..20], 0, 20).unwrap();
        assert!(hdr.styles.is_empty());
        // every truncation is survivable
        for n in 0..good.len() {
            let _ = Stylesheet::parse(&good[..n], 0, n as u32);
        }
        // an STD too short for its header or name yields an empty slot; a name or UPX past the STD is tolerated
        assert!(parse_std(&[1, 0], 10).is_none());
        assert!(parse_std(&[0u8; 10], 10).is_none());
        let mut std = vec![0u8; 10];
        std[4] = 0x02; // two UPXs claimed
        std.extend_from_slice(&200u16.to_le_bytes()); // name length beyond the STD
        assert!(parse_std(&std, 10).is_some());
        let mut std = vec![0u8; 10];
        std[2] = 0x01; // stk = paragraph
        std[4] = 0x03; // three UPXs claimed, none present
        std.extend_from_slice(&[0, 0, 0, 0]);
        let st = parse_std(&std, 10).unwrap();
        assert!(st.papx.is_empty() && st.chpx.is_empty());
        let mut std = vec![0u8; 10];
        std[2] = 0x01;
        std[4] = 0x01;
        std.extend_from_slice(&[0, 0, 0, 0]);
        std.extend_from_slice(&[9, 0, 1, 2]); // UPX claims 9 bytes, 2 present
        assert!(parse_std(&std, 10).unwrap().papx.is_empty());
    }
}
