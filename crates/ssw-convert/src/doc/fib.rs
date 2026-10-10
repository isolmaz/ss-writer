//! [MS-DOC] 2.5 File Information Block and the compound-file container: the three streams the
//! importer needs (`WordDocument`, `0Table`/`1Table`, `Data`), plus bounds-checked readers.

use std::io::{Cursor, Read, Seek};

use crate::import::ImportError;

use super::sniff;

pub(super) type Result<T, E = ImportError> = std::result::Result<T, E>;

pub(super) fn corrupt<T>(msg: impl Into<String>) -> Result<T> {
    Err(ImportError::Corrupt(msg.into()))
}

// ---- bounds-checked little-endian reads -------------------------------------------------

pub(super) fn u8_at(b: &[u8], o: usize) -> Result<u8> {
    match b.get(o) {
        Some(v) => Ok(*v),
        None => corrupt(format!("read u8 @{o} past {}", b.len())),
    }
}

pub(super) fn u16_at(b: &[u8], o: usize) -> Result<u16> {
    match o.checked_add(2).and_then(|e| b.get(o..e)) {
        Some(s) => Ok(u16::from_le_bytes([s[0], s[1]])),
        None => corrupt(format!("read u16 @{o} past {}", b.len())),
    }
}

pub(super) fn u32_at(b: &[u8], o: usize) -> Result<u32> {
    match o.checked_add(4).and_then(|e| b.get(o..e)) {
        Some(s) => Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]])),
        None => corrupt(format!("read u32 @{o} past {}", b.len())),
    }
}

/// The little-endian `u32` at the start of `b`, for callers that have already established that
/// four bytes are there (indexing panics otherwise).
pub(super) fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Bounds-checked sub-slice `[off, off+len)`.
pub(super) fn slice(b: &[u8], off: usize, len: usize) -> Result<&[u8]> {
    match off.checked_add(len).and_then(|e| b.get(off..e)) {
        Some(s) => Ok(s),
        None => corrupt(format!("slice {off}+{len} past {}", b.len())),
    }
}

// ---- FIB ----------------------------------------------------------------------------------

const WIDENT_WORD97: u16 = 0xA5EC;
const WIDENT_WORD6_95: u16 = 0xA5DC;
const WIDENT_WORD2: u16 = 0xA5DB;
const NFIB_WORD97: u16 = 0x00C1;

// Indices into the FibRgFcLcb97 (fc, lcb) pair array.
pub(super) const IDX_STSHF: usize = 1;
pub(super) const IDX_PLCFFND_REF: usize = 2;
pub(super) const IDX_PLCFFND_TXT: usize = 3;
pub(super) const IDX_PLCF_SED: usize = 6;
pub(super) const IDX_PLCF_BTE_CHPX: usize = 12;
pub(super) const IDX_PLCF_BTE_PAPX: usize = 13;
pub(super) const IDX_CLX: usize = 33;
pub(super) const IDX_PLCFEND_REF: usize = 46;
pub(super) const IDX_PLCFEND_TXT: usize = 47;
pub(super) const IDX_PLF_LST: usize = 73;
pub(super) const IDX_PLF_LFO: usize = 74;

#[derive(Debug, Clone)]
pub(super) struct Fib {
    pub lid: u16,
    pub lid_fe: u16,
    flags: u16,
    pub ccp_text: u32,
    pub ccp_ftn: u32,
    pub ccp_hdd: u32,
    pub ccp_atn: u32,
    pub ccp_edn: u32,
    pub ccp_txbx: u32,
    pub ccp_hdr_txbx: u32,
    pairs: Vec<(u32, u32)>,
}

impl Fib {
    pub fn parse(w: &[u8]) -> Result<Fib> {
        let ident = u16_at(w, 0)?;
        let nfib = u16_at(w, 2)?;
        match ident {
            WIDENT_WORD97 if nfib >= NFIB_WORD97 => {}
            WIDENT_WORD97 => {
                return Err(ImportError::UnsupportedVersion(format!(
                    "Word binary format {nfib:#06x}"
                )));
            }
            WIDENT_WORD6_95 => return Err(ImportError::UnsupportedVersion("Word 6/95".into())),
            WIDENT_WORD2 => return Err(ImportError::UnsupportedVersion("Word 2.0".into())),
            other => return corrupt(format!("not a Word document (wIdent {other:#06x})")),
        }
        let lid = u16_at(w, 6)?;
        let flags = u16_at(w, 10)?;
        // FibBase is 32 bytes; FibRgW97 follows (csw 16-bit words).
        let csw = u16_at(w, 32)? as usize;
        let rgw = 34;
        // lidFE is the last (14th) word of FibRgW97.
        let lid_fe = if csw >= 14 { u16_at(w, rgw + 26)? } else { 0 };
        let rgw_end = rgw + csw * 2;
        let cslw = u16_at(w, rgw_end)? as usize;
        if cslw < 11 {
            return corrupt(format!("FibRgLw97 too short (cslw={cslw})"));
        }
        let lw = rgw_end + 2;
        let ccp_text = u32_at(w, lw + 12)?;
        let ccp_ftn = u32_at(w, lw + 16)?;
        let ccp_hdd = u32_at(w, lw + 20)?;
        let ccp_atn = u32_at(w, lw + 28)?;
        let ccp_edn = u32_at(w, lw + 32)?;
        let ccp_txbx = u32_at(w, lw + 36)?;
        let ccp_hdr_txbx = u32_at(w, lw + 40)?;
        let fclcb_at = lw + cslw * 4;
        let cb = u16_at(w, fclcb_at)? as usize;
        let mut pairs = Vec::with_capacity(cb.min(256));
        for i in 0..cb.min(256) {
            let o = fclcb_at + 2 + i * 8;
            // Tolerate a truncated tail (fuzzed files): keep what is there.
            match (u32_at(w, o), u32_at(w, o + 4)) {
                (Ok(fc), Ok(lcb)) => pairs.push((fc, lcb)),
                _ => break,
            }
        }
        Ok(Fib {
            lid,
            lid_fe,
            flags,
            ccp_text,
            ccp_ftn,
            ccp_hdd,
            ccp_atn,
            ccp_edn,
            ccp_txbx,
            ccp_hdr_txbx,
            pairs,
        })
    }

    pub fn encrypted(&self) -> bool {
        self.flags & 0x0100 != 0
    }
    pub fn table1(&self) -> bool {
        self.flags & 0x0200 != 0
    }
    pub fn far_east(&self) -> bool {
        self.flags & 0x4000 != 0
    }
    pub fn obfuscated(&self) -> bool {
        self.flags & 0x8000 != 0
    }

    /// Windows code page of the 8-bit ("compressed") text pieces: `lidFE` for East-Asian
    /// documents, the document language otherwise.
    pub fn text_codepage(&self) -> u16 {
        codepage_for_lid(if self.far_east() && self.lid_fe != 0 {
            self.lid_fe
        } else {
            self.lid
        })
    }

    /// (fc, lcb) of `FibRgFcLcb97` entry `idx`; (0, 0) when the FIB is too short to have it.
    pub fn pair(&self, idx: usize) -> (u32, u32) {
        self.pairs.get(idx).copied().unwrap_or((0, 0))
    }
}

/// Maps a Windows LCID to the ANSI code page Word uses for its 8-bit text.
pub(super) fn codepage_for_lid(lid: u16) -> u16 {
    match lid & 0x03FF {
        0x04 => match lid {
            0x0804 | 0x1004 => 936,
            _ => 950,
        },
        0x11 => 932,
        0x12 => 949,
        0x1E => 874,
        0x2A => 1258,
        0x08 => 1253,
        0x0D => 1255,
        0x01 | 0x20 | 0x29 | 0x59 | 0x60 | 0x63 | 0x8C => 1256,
        0x25..=0x27 => 1257,
        0x1F | 0x2C => match lid {
            // Azeri (Cyrillic) is 0x082C.
            0x082C => 1251,
            _ => 1254,
        },
        0x1A => match lid {
            0x081A | 0x0C1A | 0x1C1A | 0x201A | 0x1A1A | 0x241A | 0x281A | 0x301A => 1251,
            _ => 1250,
        },
        0x02 | 0x19 | 0x22 | 0x23 | 0x28 | 0x2F | 0x3F | 0x40 | 0x44 | 0x50 | 0x85 => 1251,
        0x43 => match lid {
            0x0843 => 1251,
            _ => 1254,
        },
        0x05 | 0x0E | 0x15 | 0x18 | 0x1B | 0x1C | 0x24 | 0x7C => 1250,
        _ => 1252,
    }
}

// ---- container ----------------------------------------------------------------------------

pub(super) struct Streams {
    pub word: Vec<u8>,
    pub table: Vec<u8>,
    /// Optional `Data` stream (inline pictures, huge PAPX). Empty if absent.
    pub data: Vec<u8>,
}

const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const MAX_PAD: usize = 64 << 20;

fn read_stream<F: Read + Seek>(cf: &mut cfb::CompoundFile<F>, name: &str) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    let read = cf.open_stream(name).and_then(|mut s| s.read_to_end(&mut v));
    match read {
        Ok(_) => Ok(v),
        Err(e) => corrupt(format!("stream {name}: {e}")),
    }
}

/// Zero-pads a compound file up to the size implied by its header's FAT sector count.
/// `None` when the header says nothing useful or the padding would be absurd (> 64 MiB).
pub(super) fn pad_truncated(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < 512 {
        return None;
    }
    let shift = u16::from_le_bytes([bytes[0x1E], bytes[0x1F]]);
    if !(7..=16).contains(&shift) {
        return None;
    }
    let ssz = 1u64 << shift;
    let n_fat = u64::from(u32::from_le_bytes([
        bytes[0x2C],
        bytes[0x2D],
        bytes[0x2E],
        bytes[0x2F],
    ]));
    // At most (1 + 2^32 * 2^14) * 2^16 < 2^63: fits in `u64` whatever the header says.
    let want = ssz * (1 + n_fat * (ssz / 4));
    if want <= bytes.len() as u64 || want > MAX_PAD as u64 {
        return None;
    }
    let want = want as usize;
    let mut v = Vec::with_capacity(want);
    v.extend_from_slice(bytes);
    v.resize(want, 0);
    Some(v)
}

/// Classifies a non-OLE input; `Err` is always the right refusal.
fn not_ole(bytes: &[u8]) -> ImportError {
    if bytes.starts_with(&[0xDB, 0xA5]) {
        return ImportError::UnsupportedVersion("Word 2.0".into());
    }
    match sniff::sniff(bytes) {
        Some(detected) => ImportError::WrongFormat {
            expected: "DOC",
            detected,
        },
        None => ImportError::Corrupt("not an OLE compound file".into()),
    }
}

/// Opens the container and extracts the streams; rejects encrypted documents.
pub(super) fn load(bytes: &[u8]) -> Result<(Fib, Streams)> {
    if bytes.len() < 8 || bytes[..8] != CFB_MAGIC {
        return Err(not_ole(bytes));
    }
    let padded;
    let mut cf = match cfb::CompoundFile::open(Cursor::new(bytes)) {
        Ok(cf) => cf,
        Err(first) => {
            // cfb rejects truncated containers (the FAT claims more sectors than the file has);
            // Word accepts them. Zero-pad to the size the FAT claims and retry once.
            let Some(p) = pad_truncated(bytes) else {
                return corrupt(format!("compound file: {first}"));
            };
            padded = p;
            match cfb::CompoundFile::open(Cursor::new(&padded[..])) {
                Ok(cf) => cf,
                Err(e) => return corrupt(format!("compound file: {e}")),
            }
        }
    };
    if !cf.exists("/WordDocument") {
        return corrupt("compound file has no WordDocument stream");
    }
    let word = read_stream(&mut cf, "/WordDocument")?;
    let fib = Fib::parse(&word)?;
    if fib.encrypted() || fib.obfuscated() {
        return Err(ImportError::Encrypted);
    }
    let (prefer, other) = if fib.table1() {
        ("/1Table", "/0Table")
    } else {
        ("/0Table", "/1Table")
    };
    let name = if cf.exists(prefer) { prefer } else { other };
    if !cf.exists(name) {
        return corrupt("compound file has no 0Table/1Table stream");
    }
    let table = read_stream(&mut cf, name)?;
    let data = if cf.exists("/Data") {
        read_stream(&mut cf, "/Data")?
    } else {
        Vec::new()
    };
    Ok((fib, Streams { word, table, data }))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Builds the smallest FIB `Fib::parse` accepts.
    pub(crate) fn fib_bytes(
        ident: u16,
        nfib: u16,
        flags: u16,
        csw: u16,
        cslw: u16,
        pairs: &[(u32, u32)],
    ) -> Vec<u8> {
        let mut w = vec![0u8; 32];
        w[0..2].copy_from_slice(&ident.to_le_bytes());
        w[2..4].copy_from_slice(&nfib.to_le_bytes());
        w[6..8].copy_from_slice(&0x0419u16.to_le_bytes());
        w[10..12].copy_from_slice(&flags.to_le_bytes());
        w.extend_from_slice(&csw.to_le_bytes());
        let mut rgw = vec![0u8; csw as usize * 2];
        if csw >= 14 {
            rgw[26..28].copy_from_slice(&0x0411u16.to_le_bytes());
        }
        w.extend_from_slice(&rgw);
        w.extend_from_slice(&cslw.to_le_bytes());
        let mut lw = vec![0u8; cslw as usize * 4];
        for (i, v) in [
            (3usize, 100u32),
            (4, 5),
            (5, 6),
            (7, 7),
            (8, 10),
            (9, 8),
            (10, 9),
        ] {
            if (i + 1) * 4 <= lw.len() {
                lw[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
        }
        w.extend_from_slice(&lw);
        w.extend_from_slice(&(pairs.len() as u16).to_le_bytes());
        for (fc, lcb) in pairs {
            w.extend_from_slice(&fc.to_le_bytes());
            w.extend_from_slice(&lcb.to_le_bytes());
        }
        w
    }

    #[test]
    fn readers_check_bounds() {
        let b = [1u8, 2, 3, 4, 5];
        assert_eq!(u8_at(&b, 4), Ok(5));
        assert!(u8_at(&b, 5).is_err());
        assert_eq!(u16_at(&b, 3), Ok(0x0504));
        assert!(u16_at(&b, 4).is_err());
        assert!(u16_at(&b, usize::MAX).is_err());
        assert_eq!(u32_at(&b, 1), Ok(0x0504_0302));
        assert!(u32_at(&b, 2).is_err());
        assert_eq!(slice(&b, 1, 3), Ok(&b[1..4]));
        assert!(slice(&b, 3, 3).is_err());
        assert!(slice(&b, usize::MAX, 2).is_err());
    }

    #[test]
    fn parse_reads_counts_flags_and_pairs() {
        let w = fib_bytes(0xA5EC, 0x00C1, 0x4200, 14, 22, &[(1, 2), (3, 4)]);
        let f = Fib::parse(&w).unwrap();
        assert_eq!((f.lid, f.lid_fe), (0x0419, 0x0411));
        assert_eq!(
            (
                f.ccp_text,
                f.ccp_ftn,
                f.ccp_hdd,
                f.ccp_atn,
                f.ccp_edn,
                f.ccp_txbx,
                f.ccp_hdr_txbx
            ),
            (100, 5, 6, 7, 10, 8, 9)
        );
        assert!(f.table1() && f.far_east() && !f.encrypted() && !f.obfuscated());
        assert_eq!(f.pair(1), (3, 4));
        assert_eq!(f.pair(9), (0, 0));
        // far-east document: the code page follows lidFE (Japanese)
        assert_eq!(f.text_codepage(), 932);
    }

    #[test]
    fn parse_flags_and_codepage_follow_lid() {
        let w = fib_bytes(0xA5EC, 0x0101, 0x8100, 12, 22, &[]);
        let f = Fib::parse(&w).unwrap();
        assert!(f.encrypted() && f.obfuscated() && !f.far_east());
        assert_eq!(f.lid_fe, 0);
        assert_eq!(f.text_codepage(), 1251);
    }

    #[test]
    fn parse_tolerates_truncated_pair_table() {
        let mut w = fib_bytes(0xA5EC, 0x00C1, 0, 14, 22, &[(1, 2), (3, 4)]);
        w.truncate(w.len() - 6);
        assert_eq!(Fib::parse(&w).unwrap().pair(1), (0, 0));
        assert_eq!(Fib::parse(&w).unwrap().pair(0), (1, 2));
    }

    #[test]
    fn parse_rejects_other_versions_and_garbage() {
        let cases: [(u16, u16, &str); 4] = [
            (0xA5DC, 0x0065, "Word 6/95"),
            (0xA5DB, 0x0023, "Word 2.0"),
            (0xA5EC, 0x0068, "Word binary format 0x0068"),
            (0xA5EC, 0x00C0, "Word binary format 0x00c0"),
        ];
        for (ident, nfib, expect) in cases {
            let w = fib_bytes(ident, nfib, 0, 14, 22, &[]);
            assert_eq!(
                Fib::parse(&w).err(),
                Some(ImportError::UnsupportedVersion(expect.into())),
                "{ident:#06x}/{nfib:#06x}"
            );
        }
        assert_eq!(
            Fib::parse(&fib_bytes(0x1234, 0x00C1, 0, 14, 22, &[])).err(),
            Some(ImportError::Corrupt(
                "not a Word document (wIdent 0x1234)".into()
            ))
        );
        assert_eq!(
            Fib::parse(&fib_bytes(0xA5EC, 0x00C1, 0, 14, 5, &[])).err(),
            Some(ImportError::Corrupt("FibRgLw97 too short (cslw=5)".into()))
        );
        assert!(Fib::parse(&[0xEC]).is_err());
        let w = fib_bytes(0xA5EC, 0x00C1, 0, 14, 22, &[]);
        for cut in 0..w.len() {
            // never panics on any truncation
            let _ = Fib::parse(&w[..cut]);
        }
    }

    #[test]
    fn codepages_by_lid() {
        let cases: [(u16, u16); 24] = [
            (0x0409, 1252),
            (0x0407, 1252),
            (0x0419, 1251),
            (0x0422, 1251),
            (0x0405, 1250),
            (0x0415, 1250),
            (0x041A, 1250),
            (0x0C1A, 1251),
            (0x0408, 1253),
            (0x041F, 1254),
            (0x042C, 1254),
            (0x082C, 1251),
            (0x040D, 1255),
            (0x0401, 1256),
            (0x0429, 1256),
            (0x0425, 1257),
            (0x042A, 1258),
            (0x041E, 874),
            (0x0411, 932),
            (0x0412, 949),
            (0x0804, 936),
            (0x0404, 950),
            (0x0843, 1251),
            (0x0443, 1254),
        ];
        for (lid, cp) in cases {
            assert_eq!(codepage_for_lid(lid), cp, "lid {lid:#06x}");
        }
    }

    #[test]
    fn pad_truncated_extends_to_fat_size() {
        let mut b = vec![0u8; 1024];
        b[0x1E] = 9; // 512-byte sectors
        b[0x2C] = 2; // two FAT sectors -> (1 + 2 * 128) * 512 bytes
        let p = pad_truncated(&b).unwrap();
        assert_eq!(p.len(), 512 * (1 + 2 * 128));
        assert!(pad_truncated(&p).is_none(), "already complete");
        assert!(pad_truncated(&b[..100]).is_none());
        let mut bad_shift = b.clone();
        bad_shift[0x1E] = 3;
        assert!(pad_truncated(&bad_shift).is_none());
        let mut huge = b.clone();
        huge[0x2C..0x30].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(pad_truncated(&huge).is_none());
        let mut absurd = b;
        absurd[0x2C..0x30].copy_from_slice(&0x0100_0000u32.to_le_bytes());
        assert!(pad_truncated(&absurd).is_none());
    }

    #[test]
    fn load_refuses_non_ole_inputs() {
        let cases: [(&[u8], &str); 6] = [
            (b"{\\rtf1\\ansi hello}", "RTF"),
            (b"PK\x03\x04rest", "DOCX"),
            (b"<html><body>x</body></html>", "HTML"),
            (b"%PDF-1.7 ...", "PDF"),
            (b"just some plain text in a file called .doc", "text"),
            (b"<!DOCTYPE html>", "HTML"),
        ];
        for (input, want) in cases {
            assert_eq!(
                load(input).err(),
                Some(ImportError::WrongFormat {
                    expected: "DOC",
                    detected: want
                }),
                "{input:?}"
            );
        }
        assert_eq!(
            load(&[0xDB, 0xA5, 0, 1, 2, 3, 4, 5, 6, 7, 8]).err(),
            Some(ImportError::UnsupportedVersion("Word 2.0".into()))
        );
        let not_ole = Some(ImportError::Corrupt("not an OLE compound file".into()));
        assert_eq!(load(&[0, 0, 0, 0, 0, 0, 0, 0, 0]).err(), not_ole);
        assert_eq!(load(&[0xD0, 0xCF]).err(), not_ole);
        let mut magic_only = CFB_MAGIC.to_vec();
        magic_only.extend_from_slice(&[0; 600]);
        let refused = load(&magic_only).err().unwrap().to_string();
        assert!(
            refused.starts_with("the document is corrupt: compound file: "),
            "{refused}"
        );
    }

    #[test]
    fn load_reports_a_data_stream_that_cannot_be_read() {
        use std::io::Write as _;
        let mut cf = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        for (name, data) in [
            ("/WordDocument", fib_bytes(0xA5EC, 0x00C1, 0, 14, 22, &[])),
            ("/0Table", Vec::new()),
            ("/Data", b"pictures".to_vec()),
        ] {
            cf.create_stream(name).unwrap().write_all(&data).unwrap();
        }
        cf.flush().unwrap();
        let mut bytes = cf.into_inner().into_inner();
        let (_, streams) = load(&bytes).unwrap();
        assert_eq!(streams.data, b"pictures");
        // point the directory entry of `Data` (UTF-16 name, start sector at byte 116 of the
        // 128-byte entry) at a mini sector the file does not have
        let name = [b'D', 0, b'a', 0, b't', 0, b'a', 0, 0, 0];
        let entry = bytes.windows(name.len()).position(|w| w == name).unwrap();
        bytes[entry + 116..entry + 120].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        let refused = load(&bytes).err().unwrap().to_string();
        assert!(
            refused.starts_with("the document is corrupt: stream /Data: "),
            "{refused}"
        );
    }
}
