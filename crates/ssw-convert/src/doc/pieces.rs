//! [MS-DOC] 2.9.38 Clx / 2.8.35 PlcPcd: the piece table (CP <-> file offset, 8-bit vs UTF-16
//! pieces) and decoding of the text stream with the document's code page.

use encoding_rs::Encoding;

use super::fib::{Result, corrupt, slice, u16_at, u32_at};

/// Placeholder written at the trail-byte position of a double-byte character in an 8-bit
/// piece (every byte is its own CP); the converter skips it.
pub(super) const SKIP: u16 = 0xFFFF;

#[derive(Debug, Clone)]
pub(super) struct Piece {
    pub cp0: u32,
    pub cp1: u32,
    /// Byte offset in the WordDocument stream of the first character.
    pub fc: u32,
    /// `true`: 1 byte per character (document code page); `false`: UTF-16LE.
    pub compressed: bool,
    /// `Pcd.prm` resolved to a grpprl (Prm1 -> CLX RgPrc entry, Prm0 -> single sprm); empty
    /// when none. Applies to every paragraph/run whose characters live in this piece.
    pub mods: Vec<u8>,
}

impl Piece {
    fn bytes_per_char(&self) -> u32 {
        if self.compressed { 1 } else { 2 }
    }

    fn fc_end(&self) -> u32 {
        self.fc
            .saturating_add((self.cp1 - self.cp0).saturating_mul(self.bytes_per_char()))
    }
}

#[derive(Debug, Default)]
pub(super) struct Pieces {
    pub pieces: Vec<Piece>,
    /// Piece indexes sorted by `fc` (for FC -> CP mapping of FKP ranges).
    by_fc: Vec<usize>,
    /// Running max of `fc_end` along `by_fc`. Fast-saved documents can reference the same file
    /// bytes from several pieces, so `fc_end` alone is not monotone along `by_fc`.
    max_end: Vec<u32>,
}

impl Pieces {
    pub fn parse(table: &[u8], fc: u32, lcb: u32) -> Result<Pieces> {
        let mut off = fc as usize;
        let end = off.checked_add(lcb as usize).filter(|&e| e <= table.len());
        let Some(end) = end else {
            return corrupt("CLX outside table stream");
        };
        // RgPrc: clxt = 1, cbGrpprl: u16, grpprl
        let mut prcs: Vec<&[u8]> = Vec::new();
        // (`off < end <= table.len()`: the byte at `off` exists)
        while off < end && table[off] == 0x01 {
            let cb = u16_at(table, off + 1)? as usize;
            prcs.push(slice(table, off + 3, cb)?);
            off += 3 + cb;
        }
        if off >= end || table[off] != 0x02 {
            return corrupt("CLX has no Pcdt");
        }
        let lcb_plc = u32_at(table, off + 1)? as usize;
        off += 5;
        let plc = slice(table, off, lcb_plc)?;
        if lcb_plc < 4 {
            return corrupt("PlcPcd too small");
        }
        // n + 1 CPs, then n Pcds: `plc` holds at least 4 + 12 * n bytes
        let n = (lcb_plc - 4) / 12;
        let (cps, pcds) = plc.split_at((n + 1) * 4);
        let cps: Vec<u32> = cps
            .as_chunks::<4>()
            .0
            .iter()
            .map(|cp| u32::from_le_bytes([cp[0], cp[1], cp[2], cp[3]]))
            .collect();
        let mut pieces = Vec::with_capacity(n);
        for (i, pcd) in pcds.as_chunks::<8>().0.iter().take(n).enumerate() {
            let (cp0, cp1) = (cps[i], cps[i + 1]);
            // Pcd: flags(2), FcCompressed(4), prm(2)
            let fcc = u32::from_le_bytes([pcd[2], pcd[3], pcd[4], pcd[5]]);
            let prm = u16::from_le_bytes([pcd[6], pcd[7]]);
            let compressed = fcc & 0x4000_0000 != 0;
            let fc = fcc & 0x3FFF_FFFF;
            let fc = if compressed { fc / 2 } else { fc };
            if cp1 < cp0 {
                return corrupt("PlcPcd CPs not monotonic");
            }
            let mods = if prm & 1 != 0 {
                prcs.get((prm >> 1) as usize)
                    .map(|g| g.to_vec())
                    .unwrap_or_default()
            } else {
                prm0_grpprl(prm)
            };
            pieces.push(Piece {
                cp0,
                cp1,
                fc,
                compressed,
                mods,
            });
        }
        let mut by_fc: Vec<usize> = (0..pieces.len()).collect();
        by_fc.sort_by_key(|&i| pieces[i].fc);
        let mut max_end = Vec::with_capacity(by_fc.len());
        let mut m = 0u32;
        for &i in &by_fc {
            m = m.max(pieces[i].fc_end());
            max_end.push(m);
        }
        Ok(Pieces {
            pieces,
            by_fc,
            max_end,
        })
    }

    pub fn total_cp(&self) -> u32 {
        self.pieces.last().map_or(0, |p| p.cp1)
    }

    /// The whole text as UTF-16 code units indexed by CP, decoding 8-bit pieces with
    /// `codepage`. Also returns how many characters lay beyond the end of the stream.
    pub fn read_text(&self, word: &[u8], codepage: u16) -> Result<(Vec<u16>, u32)> {
        let total = self.total_cp() as usize;
        if total > word.len() + 16 {
            return corrupt(format!(
                "piece table claims {total} characters but WordDocument is {} bytes",
                word.len()
            ));
        }
        let enc = encoding_for(codepage);
        let mut out = vec![0x20u16; total];
        let mut truncated = 0u32;
        for p in &self.pieces {
            let n = (p.cp1 - p.cp0) as usize;
            let start = p.fc as usize;
            let bpc = p.bytes_per_char() as usize;
            let avail = word.len().saturating_sub(start) / bpc;
            let take = n.min(avail);
            truncated += (n - take) as u32;
            let dst = &mut out[p.cp0 as usize..p.cp0 as usize + take];
            if p.compressed {
                decode_compressed(&word[start..start + take], enc, dst);
            } else {
                for (i, d) in dst.iter_mut().enumerate() {
                    let o = start + i * 2;
                    *d = u16::from_le_bytes([word[o], word[o + 1]]);
                }
            }
        }
        Ok((out, truncated))
    }

    /// Maps a file-offset range `[a, b)` of the WordDocument stream to `(cp0, cp1, piece
    /// index)` triples appended to `out`.
    pub fn fc_range_to_cp(&self, a: u32, b: u32, out: &mut Vec<(u32, u32, usize)>) {
        if b <= a {
            return;
        }
        // first piece (in fc order) whose fc_end > a
        let first = self.max_end.partition_point(|&e| e <= a);
        for &i in &self.by_fc[first..] {
            let p = &self.pieces[i];
            if p.fc >= b {
                break;
            }
            let lo = a.max(p.fc);
            let hi = b.min(p.fc_end());
            if hi > lo {
                let bpc = p.bytes_per_char();
                out.push((
                    p.cp0 + (lo - p.fc) / bpc,
                    p.cp0 + (hi - p.fc).div_ceil(bpc),
                    i,
                ));
            }
        }
    }
}

/// `Encoding` for a Windows code page; unknown values fall back to windows-1252.
fn encoding_for(codepage: u16) -> &'static Encoding {
    match codepage {
        874 => encoding_rs::WINDOWS_874,
        932 => encoding_rs::SHIFT_JIS,
        936 => encoding_rs::GBK,
        949 => encoding_rs::EUC_KR,
        950 => encoding_rs::BIG5,
        1250 => encoding_rs::WINDOWS_1250,
        1251 => encoding_rs::WINDOWS_1251,
        1253 => encoding_rs::WINDOWS_1253,
        1254 => encoding_rs::WINDOWS_1254,
        1255 => encoding_rs::WINDOWS_1255,
        1256 => encoding_rs::WINDOWS_1256,
        1257 => encoding_rs::WINDOWS_1257,
        1258 => encoding_rs::WINDOWS_1258,
        _ => encoding_rs::WINDOWS_1252,
    }
}

/// Decodes one 8-bit piece into `dst` (`dst.len() == src.len()`): one UTF-16 unit per byte.
/// Single-byte code pages are 1:1; for double-byte code pages a lead+trail pair yields the
/// character at the lead position and [`SKIP`] at the trail position.
fn decode_compressed(src: &[u8], enc: &'static Encoding, dst: &mut [u16]) {
    let double_byte = [
        encoding_rs::SHIFT_JIS,
        encoding_rs::GBK,
        encoding_rs::EUC_KR,
        encoding_rs::BIG5,
    ]
    .contains(&enc);
    if !double_byte {
        let (s, _) = enc.decode_without_bom_handling(src);
        for (d, c) in dst.iter_mut().zip(s.encode_utf16()) {
            *d = c;
        }
        return;
    }
    let mut i = 0;
    while i < src.len() {
        let b = src[i];
        if b < 0x80 {
            dst[i] = u16::from(b);
            i += 1;
            continue;
        }
        if let Some(pair) = src.get(i..i + 2) {
            let (s, had_errors) = enc.decode_without_bom_handling(pair);
            let mut units = s.encode_utf16();
            if let (false, Some(u), None) = (had_errors, units.next(), units.next()) {
                dst[i] = u;
                dst[i + 1] = SKIP;
                i += 2;
                continue;
            }
        }
        let (s, _) = enc.decode_without_bom_handling(&src[i..=i]);
        dst[i] = s.encode_utf16().next().unwrap_or(0xFFFD);
        i += 1;
    }
}

/// Prm0 (`fComplex == 0`): the 7-bit `isprm` selects one sprm and the high byte is its 1-byte
/// operand. Only the sprms this importer models are mapped; the `isprm` -> sprm values are
/// the ones used by other open implementations and are not validated against the
/// specification text. Other values are ignored (they affect layout, not content).
fn prm0_grpprl(prm: u16) -> Vec<u8> {
    let isprm = (prm >> 1) & 0x7F;
    let val = (prm >> 8) as u8;
    let sprm: u16 = match isprm {
        0x0C => 0x260A, // sprmPIlvl
        0x18 => 0x2416, // sprmPFInTable
        0x19 => 0x2417, // sprmPFTtp
        0x55 => 0x0835, // sprmCFBold
        0x56 => 0x0836, // sprmCFItalic
        0x57 => 0x0837, // sprmCFStrike
        0x78 => 0x2640, // sprmPOutLvl
        _ => return Vec::new(),
    };
    vec![sprm as u8, (sprm >> 8) as u8, val]
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A CLX with the given RgPrc entries and `(cp0, cp1, fcc, prm)` pieces.
    pub(crate) fn clx(prcs: &[&[u8]], pieces: &[(u32, u32, u32, u16)]) -> Vec<u8> {
        let mut v = Vec::new();
        for g in prcs {
            v.push(1);
            v.extend_from_slice(&(g.len() as u16).to_le_bytes());
            v.extend_from_slice(g);
        }
        v.push(2);
        let n = pieces.len();
        v.extend_from_slice(&((n * 12 + 4) as u32).to_le_bytes());
        for p in pieces {
            v.extend_from_slice(&p.0.to_le_bytes());
        }
        v.extend_from_slice(&pieces.last().map_or(0, |p| p.1).to_le_bytes());
        for p in pieces {
            v.extend_from_slice(&[0, 0]);
            v.extend_from_slice(&p.2.to_le_bytes());
            v.extend_from_slice(&p.3.to_le_bytes());
        }
        v
    }

    fn parse(clx: &[u8]) -> Result<Pieces> {
        Pieces::parse(clx, 0, clx.len() as u32)
    }

    #[test]
    fn parses_pieces_and_resolves_prm() {
        let prc: &[u8] = &[0x35, 0x08, 1];
        let t = clx(
            &[prc],
            &[
                (0, 4, 0x4000_0000 | 20, 0x0001),
                (4, 6, 40, 0x7F00 | 0x18 << 1),
                (6, 8, 60, 0x0003),
            ],
        );
        let p = parse(&t).unwrap();
        assert_eq!(p.pieces.len(), 3);
        assert_eq!(p.total_cp(), 8);
        assert!(p.pieces[0].compressed);
        assert_eq!(p.pieces[0].fc, 10);
        assert_eq!(p.pieces[0].mods, prc);
        assert!(!p.pieces[1].compressed);
        assert_eq!(p.pieces[1].mods, vec![0x16, 0x24, 0x7F]);
        // Prm1 pointing past RgPrc has no modifiers
        assert!(p.pieces[2].mods.is_empty());
    }

    #[test]
    fn prm0_table() {
        let cases: [(u16, Option<[u8; 3]>); 9] = [
            (0x0C << 1 | 0x0100, Some([0x0A, 0x26, 1])),
            (0x18 << 1 | 0x0100, Some([0x16, 0x24, 1])),
            (0x19 << 1 | 0x0100, Some([0x17, 0x24, 1])),
            (0x55 << 1 | 0x0100, Some([0x35, 0x08, 1])),
            (0x56 << 1 | 0x0100, Some([0x36, 0x08, 1])),
            (0x57 << 1 | 0x0100, Some([0x37, 0x08, 1])),
            (0x78 << 1 | 0x0100, Some([0x40, 0x26, 1])),
            (0x01 << 1 | 0x0100, None),
            (0, None),
        ];
        for (prm, want) in cases {
            assert_eq!(
                prm0_grpprl(prm),
                want.map(|a| a.to_vec()).unwrap_or_default(),
                "{prm:#06x}"
            );
        }
    }

    #[test]
    fn rejects_malformed_clx() {
        let good = clx(&[], &[(0, 4, 0x4000_0000, 0)]);
        assert!(parse(&good).is_ok());
        assert!(
            Pieces::parse(&good, 0, good.len() as u32 + 1).is_err(),
            "outside table"
        );
        assert!(Pieces::parse(&good, u32::MAX, 16).is_err());
        assert!(parse(&[]).is_err(), "no Pcdt");
        assert!(parse(&[0x07, 0, 0]).is_err(), "bad clxt");
        let mut small = good.clone();
        small[1..5].copy_from_slice(&3u32.to_le_bytes());
        assert!(parse(&small).is_err(), "PlcPcd too small");
        let backwards = clx(&[], &[(5, 4, 0x4000_0000, 0)]);
        assert!(parse(&backwards).is_err(), "cps decrease");
        let mut long_prc = vec![1u8, 0xFF, 0xFF];
        long_prc.extend_from_slice(&good);
        assert!(parse(&long_prc).is_err(), "RgPrc entry beyond table");
        assert!(parse(&[1u8]).is_err(), "RgPrc size cut off");
        let mut truncated = good;
        truncated[1..5].copy_from_slice(&1000u32.to_le_bytes());
        assert!(parse(&truncated).is_err(), "PlcPcd beyond table");
        for cut in 0..truncated.len() {
            let _ = Pieces::parse(&truncated[..cut], 0, cut as u32);
        }
    }

    #[test]
    fn reads_text_with_code_pages_and_utf16() {
        // piece 1: 8-bit "\xE0\xE1b" at fc 0; piece 2: UTF-16 "Hi" at fc 3 (+ padding byte)
        let mut word = vec![0xE0, 0xE1, b'b', 0];
        word.extend_from_slice(&[b'H', 0, b'i', 0]);
        let t = clx(&[], &[(0, 3, 0x4000_0000, 0), (3, 5, 4, 0)]);
        let p = parse(&t).unwrap();
        let cases: [(u16, &str); 3] = [
            (1252, "\u{e0}\u{e1}bHi"),
            (1251, "\u{430}\u{431}bHi"),
            (1253, "\u{3b0}\u{3b1}bHi"),
        ];
        for (cp, want) in cases {
            let (units, truncated) = p.read_text(&word, cp).unwrap();
            assert_eq!(String::from_utf16_lossy(&units), want, "cp {cp}");
            assert_eq!(truncated, 0);
        }
    }

    #[test]
    fn missing_bytes_are_counted_and_absurd_tables_rejected() {
        // compressed pieces store fc * 2: the text starts at byte 8
        let t = clx(&[], &[(0, 6, 0x4000_0000 | 16, 0)]);
        let p = parse(&t).unwrap();
        let word = *b"abcdefghxy";
        let (units, truncated) = p.read_text(&word, 1252).unwrap();
        assert_eq!(String::from_utf16_lossy(&units), "xy    ");
        assert_eq!(truncated, 4);
        let huge = clx(&[], &[(0, 100_000, 0x4000_0000, 0)]);
        assert!(parse(&huge).unwrap().read_text(&word, 1252).is_err());
    }

    #[test]
    fn double_byte_pages_keep_one_unit_per_byte() {
        // Shift-JIS: "A", katakana KA as a pair (0x83 0x4A), half-width katakana (0xB1), a lone
        // trail-less lead byte at the end
        let src = [b'A', 0x83, 0x4A, 0xB1, 0x81];
        let mut dst = [0u16; 5];
        decode_compressed(&src, encoding_rs::SHIFT_JIS, &mut dst);
        assert_eq!(dst[0], 0x41);
        assert_eq!(dst[1], 0x30AB);
        assert_eq!(dst[2], SKIP);
        assert_eq!(dst[3], 0xFF71);
        assert_eq!(dst[4], 0xFFFD);
        // an invalid pair degrades to two single-byte decodes
        let mut dst = [0u16; 2];
        decode_compressed(&[0x81, 0x20], encoding_rs::SHIFT_JIS, &mut dst);
        assert_eq!(dst, [0xFFFD, 0x20]);
    }

    #[test]
    fn unknown_code_page_is_latin1() {
        assert_eq!(encoding_for(1), encoding_rs::WINDOWS_1252);
        for cp in [
            874, 932, 936, 949, 950, 1250, 1251, 1253, 1254, 1255, 1256, 1257, 1258,
        ] {
            assert_ne!(encoding_for(cp), encoding_rs::WINDOWS_1252, "{cp}");
        }
    }

    #[test]
    fn maps_file_ranges_to_cps_across_overlapping_pieces() {
        // fast-saved layout: pieces 0 and 2 both point at the same file bytes
        let t = clx(
            &[],
            &[
                (0, 10, 0x4000_0000 | 200, 0),
                (10, 14, 0x4000_0000 | 400, 0),
                (14, 20, 0x4000_0000 | 200, 0),
            ],
        );
        let p = parse(&t).unwrap();
        let mut out = Vec::new();
        p.fc_range_to_cp(104, 108, &mut out);
        assert_eq!(out, vec![(4, 8, 0), (18, 20, 2)]);
        out.clear();
        p.fc_range_to_cp(100, 110, &mut out);
        assert_eq!(out, vec![(0, 10, 0), (14, 20, 2)]);
        out.clear();
        // piece 2 already ended before the range starts: only piece 0 covers it
        p.fc_range_to_cp(107, 108, &mut out);
        assert_eq!(out, vec![(7, 8, 0)]);
        out.clear();
        p.fc_range_to_cp(5, 5, &mut out);
        p.fc_range_to_cp(0, 50, &mut out);
        assert!(out.is_empty());
    }
}
