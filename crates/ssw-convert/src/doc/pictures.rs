//! Inline pictures: `sprmCPicLocation` -> PICFAndOfficeArtData in the Data stream -> BLIP
//! (JPEG / PNG / DIB / TIFF bytes). Device-independent bitmaps are re-encoded as PNG with an
//! uncompressed (stored) deflate stream, so no image library is needed.

/// What an inline picture location resolved to.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Picture {
    /// A raster image ready to be written next to the Markdown file.
    Image { ext: &'static str, bytes: Vec<u8> },
    /// EMF / WMF / PICT: a vector format that cannot be converted.
    Vector,
    /// A DIB that is unreadable or uses an unsupported encoding, or no BLIP at all.
    Unreadable,
}

fn u16le(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(o)?, *b.get(o + 1)?]))
}

fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(o)?,
        *b.get(o + 1)?,
        *b.get(o + 2)?,
        *b.get(o + 3)?,
    ]))
}

/// Extracts the picture stored at `loc` of the Data stream.
pub(super) fn extract(data: &[u8], loc: u32) -> Picture {
    let found = locate(data, loc as usize);
    match found {
        Some(Blip::Vector) => Picture::Vector,
        Some(Blip::Bitmap { ext: "dib", bytes }) => match dib_to_png(&bytes) {
            Some(png) => Picture::Image {
                ext: "png",
                bytes: png,
            },
            None => Picture::Unreadable,
        },
        Some(Blip::Bitmap { ext, bytes }) => Picture::Image {
            ext: sniff_ext(&bytes).unwrap_or(ext),
            bytes,
        },
        None => Picture::Unreadable,
    }
}

/// File extension for the well-known raster signatures.
fn sniff_ext(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("png")
    } else if b.starts_with(&[0xFF, 0xD8]) {
        Some("jpg")
    } else if b.starts_with(b"GIF8") {
        Some("gif")
    } else {
        None
    }
}

enum Blip {
    Bitmap { ext: &'static str, bytes: Vec<u8> },
    Vector,
}

fn locate(data: &[u8], o: usize) -> Option<Blip> {
    // PICFAndOfficeArtData: lcb(4) cbHeader(2) mfpf(8, mm first) ...
    let lcb = u32le(data, o)? as usize;
    let cb_hdr = u16le(data, o + 4)? as usize;
    let mm = u16le(data, o + 6)?;
    // `o + 7 < data.len()` (the reads above succeeded) and `data.len() <= isize::MAX`, so neither
    // sum below can overflow a `usize` (`cb_hdr` is a u16, `lcb` a u32).
    let mut p = o + cb_hdr;
    if mm == 0x0066 {
        // MM_SHAPEFILE: cchPicName + name follows the header
        p += 1 + *data.get(p)? as usize;
    }
    let end = (o + lcb).min(data.len());
    let mut found = None;
    walk(&data[..end], p, end, 0, &mut found);
    found
}

fn walk(d: &[u8], mut p: usize, end: usize, depth: u8, out: &mut Option<Blip>) {
    if depth > 6 {
        return;
    }
    // `end <= d.len()` holds for every caller (`locate` slices `d` at `end`; records only recurse
    // into their own body), so the 8-byte record header below is always in bounds.
    while p + 8 <= end && !matches!(out, Some(Blip::Bitmap { .. })) {
        let hdr = &d[p..p + 8];
        let vi = u16::from_le_bytes([hdr[0], hdr[1]]);
        let ty = u16::from_le_bytes([hdr[2], hdr[3]]);
        let len = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let (ver, inst) = (vi & 0xF, vi >> 4);
        let body = p + 8;
        let body_end = body.saturating_add(len as usize);
        if body_end > end {
            return;
        }
        match ty {
            _ if ver == 0xF => walk(d, body, body_end, depth + 1, out),
            0xF007 => {
                // OfficeArtFBSE: 36-byte header, name, then an embedded BLIP record
                let name = d.get(body + 33).copied().unwrap_or(0) as usize;
                let start = body + 36 + name;
                if start + 8 <= body_end {
                    walk(d, start, body_end, depth + 1, out);
                }
            }
            0xF018..=0xF117 => blip(d, body, body_end, ty, inst, out),
            _ => {}
        }
        p = body_end;
    }
}

fn blip(d: &[u8], body: usize, end: usize, ty: u16, inst: u16, out: &mut Option<Blip>) {
    let (ext, uids): (&'static str, usize) = match (ty, inst) {
        (0xF01D | 0xF02A, 0x46A | 0x6E2) => ("jpg", 1),
        (0xF01D | 0xF02A, 0x46B | 0x6E3) => ("jpg", 2),
        (0xF01E, 0x6E0) => ("png", 1),
        (0xF01E, 0x6E1) => ("png", 2),
        (0xF01F, 0x7A8) => ("dib", 1),
        (0xF01F, 0x7A9) => ("dib", 2),
        (0xF029, 0x6E4) => ("tif", 1),
        (0xF029, 0x6E5) => ("tif", 2),
        (0xF01A..=0xF01C, _) => {
            *out = Some(Blip::Vector);
            return;
        }
        _ => return,
    };
    let start = body + uids * 16 + 1; // + tag byte
    if start < end {
        *out = Some(Blip::Bitmap {
            ext,
            bytes: d[start..end].to_vec(),
        });
    }
}

// ---- DIB -> PNG ---------------------------------------------------------------------------

/// Largest bitmap (in pixels) that is converted.
const MAX_PIXELS: u64 = 64 << 20;

/// Converts a headerless DIB (BITMAPINFOHEADER of 40 bytes or more, uncompressed or
/// bit-fields, 1/4/8/16/24/32 bits per pixel) to a PNG. `None` for anything else.
pub(super) fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let head = dib.first_chunk::<40>()?;
    let field = |o: usize| u32::from_le_bytes([head[o], head[o + 1], head[o + 2], head[o + 3]]);
    let hdr = field(0) as usize;
    if hdr < 40 || hdr > dib.len() {
        return None;
    }
    let width = field(4) as i32;
    let height = field(8) as i32;
    let bpp = u32::from(u16::from_le_bytes([head[14], head[15]]));
    let compression = field(16);
    let clr_used = field(32) as usize;
    if width <= 0
        || height == 0
        || !matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32)
        || !matches!(compression, 0 | 3)
    {
        return None;
    }
    let (w, h) = (width as u32, height.unsigned_abs());
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    let top_down = height < 0;
    // bit masks: stored after a 40-byte header, part of the header from 52 bytes on
    let mut masks = [0x7C00u32, 0x03E0, 0x001F];
    let mut pal_at = hdr;
    if compression == 3 && bpp >= 16 {
        for (i, m) in masks.iter_mut().enumerate() {
            *m = u32le(dib, 40 + i * 4)?;
        }
        if hdr == 40 {
            pal_at += 12;
        }
    } else if bpp == 32 {
        masks = [0x00FF_0000, 0x0000_FF00, 0x0000_00FF];
    }
    let palette: Vec<[u8; 3]> = if bpp <= 8 {
        let n = if clr_used > 0 {
            clr_used.min(1 << bpp)
        } else {
            1 << bpp
        };
        let raw = dib.get(pal_at..pal_at + n * 4)?;
        pal_at += n * 4;
        raw.as_chunks::<4>()
            .0
            .iter()
            .map(|c| [c[2], c[1], c[0]])
            .collect()
    } else {
        Vec::new()
    };
    let stride = ((w as usize * bpp as usize).div_ceil(32)) * 4;
    // `pal_at <= dib.len()` on every path above (the header, the 12 mask bytes and the palette
    // were all bounds-checked); `w * h <= MAX_PIXELS` bounds `stride * h` far below `usize::MAX`.
    let pixels = &dib[pal_at..];
    if pixels.len() < stride * h as usize {
        return None;
    }
    let mut raw = Vec::with_capacity((w as usize * 3 + 1) * h as usize);
    for y in 0..h as usize {
        let src_row = if top_down { y } else { h as usize - 1 - y };
        let row = &pixels[src_row * stride..(src_row + 1) * stride];
        raw.push(0); // PNG filter: none
        for x in 0..w as usize {
            let rgb = match bpp {
                1 | 4 | 8 => {
                    let per = 8 / bpp as usize;
                    let byte = row[x / per];
                    let shift = (8 - bpp as usize) - (x % per) * bpp as usize;
                    let idx = usize::from(byte >> shift) & ((1 << bpp) - 1);
                    palette.get(idx).copied().unwrap_or([0, 0, 0])
                }
                24 => [row[x * 3 + 2], row[x * 3 + 1], row[x * 3]],
                16 => channels(
                    u32::from(u16::from_le_bytes([row[x * 2], row[x * 2 + 1]])),
                    &masks,
                ),
                _ => channels(
                    u32::from_le_bytes([
                        row[x * 4],
                        row[x * 4 + 1],
                        row[x * 4 + 2],
                        row[x * 4 + 3],
                    ]),
                    &masks,
                ),
            };
            raw.extend_from_slice(&rgb);
        }
    }
    Some(encode_png(w, h, &raw))
}

/// Extracts and scales the three colour channels selected by `masks` to 8 bits.
fn channels(px: u32, masks: &[u32; 3]) -> [u8; 3] {
    let mut out = [0u8; 3];
    for (o, &m) in out.iter_mut().zip(masks) {
        if m == 0 {
            continue;
        }
        let shift = m.trailing_zeros();
        let max = m >> shift;
        let v = (px & m) >> shift;
        *o = (u64::from(v) * 255 / u64::from(max)) as u8;
    }
    out
}

fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut t = [0u32; 256];
        let mut n = 0;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[n] = c;
            n += 1;
        }
        t
    };
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &v in data {
        a = (a + u32::from(v)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// An 8-bit RGB PNG whose scanlines (`raw`, each prefixed by its filter byte) are stored in
/// uncompressed deflate blocks.
fn encode_png(w: u32, h: u32, raw: &[u8]) -> Vec<u8> {
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65535).peekable();
    if blocks.peek().is_none() {
        z.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(b) = blocks.next() {
        z.push(u8::from(blocks.peek().is_none()));
        z.extend_from_slice(&(b.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
        z.extend_from_slice(b);
    }
    z.extend_from_slice(&adler32(raw).to_be_bytes());
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes the PNGs produced here: `(width, height, raw scanlines)`; verifies every CRC
    /// and the zlib checksum.
    fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        let (mut o, mut idat, mut dims) = (8, Vec::new(), (0, 0));
        while o < png.len() {
            let len = u32::from_be_bytes(png[o..o + 4].try_into().unwrap()) as usize;
            let kind = &png[o + 4..o + 8];
            let body = &png[o + 8..o + 8 + len];
            let crc = u32::from_be_bytes(png[o + 8 + len..o + 12 + len].try_into().unwrap());
            let name = String::from_utf8_lossy(kind);
            assert_eq!(crc, crc32(&png[o + 4..o + 8 + len]), "crc of {name}");
            match kind {
                b"IHDR" => {
                    dims = (
                        u32::from_be_bytes(body[0..4].try_into().unwrap()),
                        u32::from_be_bytes(body[4..8].try_into().unwrap()),
                    )
                }
                b"IDAT" => idat.extend_from_slice(body),
                _ => {}
            }
            o += 12 + len;
        }
        // stored deflate blocks
        assert_eq!(&idat[..2], &[0x78, 0x01]);
        let (mut p, mut raw) = (2, Vec::new());
        loop {
            let last = idat[p] == 1;
            let n = u16::from_le_bytes([idat[p + 1], idat[p + 2]]) as usize;
            assert_eq!(u16::from_le_bytes([idat[p + 3], idat[p + 4]]), !(n as u16));
            raw.extend_from_slice(&idat[p + 5..p + 5 + n]);
            p += 5 + n;
            if last {
                break;
            }
        }
        assert_eq!(
            u32::from_be_bytes(idat[p..p + 4].try_into().unwrap()),
            adler32(&raw)
        );
        (dims.0, dims.1, raw)
    }

    fn dib(
        w: i32,
        h: i32,
        bpp: u16,
        compression: u32,
        extra_hdr: &[u8],
        palette: &[[u8; 4]],
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(40u32).to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&bpp.to_le_bytes());
        v.extend_from_slice(&compression.to_le_bytes());
        v.extend_from_slice(&[0; 12]);
        v.extend_from_slice(&(palette.len() as u32).to_le_bytes());
        v.extend_from_slice(&[0; 4]);
        v.extend_from_slice(extra_hdr);
        for p in palette {
            v.extend_from_slice(p);
        }
        v.extend_from_slice(pixels);
        v
    }

    #[test]
    fn crc_and_adler_known_values() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn converts_24_bit_bottom_up() {
        // 2x2, rows padded to 8 bytes; bottom row first. BGR order.
        let px = [
            0, 0, 255, 0, 255, 0, 0, 0, /* bottom: red, green */ 255, 0, 0, 255, 255, 255, 0,
            0, /* top: blue, white */
        ];
        let (w, h, raw) = decode(&dib_to_png(&dib(2, 2, 24, 0, &[], &[], &px)).unwrap());
        assert_eq!((w, h), (2, 2));
        assert_eq!(
            raw,
            vec![0, 0, 0, 255, 255, 255, 255, 0, 255, 0, 0, 0, 255, 0]
        );
        // top-down (negative height) keeps the row order
        let (_, _, raw) = decode(&dib_to_png(&dib(2, -2, 24, 0, &[], &[], &px)).unwrap());
        assert_eq!(&raw[..7], &[0, 255, 0, 0, 0, 255, 0]);
    }

    #[test]
    fn converts_palette_images() {
        let pal = [[0, 0, 0, 0], [0, 0, 255, 0]]; // black, red (BGRA)
        // 1 bpp 3x1: bits 1,0,1
        let (_, _, raw) =
            decode(&dib_to_png(&dib(3, 1, 1, 0, &[], &pal, &[0b1010_0000, 0, 0, 0])).unwrap());
        assert_eq!(raw, vec![0, 255, 0, 0, 0, 0, 0, 255, 0, 0]);
        // 4 bpp 2x1: indices 1, 0
        let (_, _, raw) =
            decode(&dib_to_png(&dib(2, 1, 4, 0, &[], &pal, &[0x10, 0, 0, 0])).unwrap());
        assert_eq!(raw, vec![0, 255, 0, 0, 0, 0, 0]);
        // 8 bpp 2x1 with an index outside the palette -> black
        let (_, _, raw) = decode(&dib_to_png(&dib(2, 1, 8, 0, &[], &pal, &[1, 9, 0, 0])).unwrap());
        assert_eq!(raw, vec![0, 255, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn converts_16_and_32_bit() {
        // 16 bpp 5-5-5: pure red = 0x7C00; 1x1 padded to 4 bytes
        let (_, _, raw) =
            decode(&dib_to_png(&dib(1, 1, 16, 0, &[], &[], &[0x00, 0x7C, 0, 0])).unwrap());
        assert_eq!(raw, vec![0, 255, 0, 0]);
        // 16 bpp 5-6-5 bit fields (masks follow the 40-byte header): pure green = 0x07E0
        let masks: Vec<u8> = [0xF800u32, 0x07E0, 0x001F]
            .iter()
            .flat_map(|m| m.to_le_bytes())
            .collect();
        let (_, _, raw) =
            decode(&dib_to_png(&dib(1, 1, 16, 3, &masks, &[], &[0xE0, 0x07, 0, 0])).unwrap());
        assert_eq!(raw, vec![0, 0, 255, 0]);
        // 32 bpp BGRX
        let (_, _, raw) =
            decode(&dib_to_png(&dib(1, 1, 32, 0, &[], &[], &[10, 20, 30, 0])).unwrap());
        assert_eq!(raw, vec![0, 30, 20, 10]);
        // 32 bpp with an all-zero channel mask contributes 0
        let masks: Vec<u8> = [0xFF00_0000u32, 0, 0x0000_00FF]
            .iter()
            .flat_map(|m| m.to_le_bytes())
            .collect();
        let (_, _, raw) =
            decode(&dib_to_png(&dib(1, 1, 32, 3, &masks, &[], &[7, 0, 0, 9])).unwrap());
        assert_eq!(raw, vec![0, 9, 0, 7]);
    }

    #[test]
    fn rejects_unsupported_or_damaged_dibs() {
        let good = dib(2, 2, 24, 0, &[], &[], &[0; 16]);
        assert!(dib_to_png(&good).is_some());
        let mut cases: Vec<Vec<u8>> = vec![
            dib(2, 2, 24, 1, &[], &[], &[0; 16]),         // RLE
            dib(2, 2, 3, 0, &[], &[], &[0; 16]),          // odd depth
            dib(0, 2, 24, 0, &[], &[], &[0; 16]),         // empty width
            dib(2, 0, 24, 0, &[], &[], &[0; 16]),         // empty height
            dib(2, 2, 24, 0, &[], &[], &[0; 15]),         // short pixel data
            dib(2, 2, 8, 0, &[], &[], &[0; 8]),           // palette of 256 entries missing
            dib(20000, 20000, 24, 0, &[], &[], &[0; 16]), // too many pixels
            dib(1, 1, 16, 3, &[], &[], &[0; 4]),          // bit-field masks missing
        ];
        let mut short_hdr = good.clone();
        short_hdr[0] = 12;
        cases.push(short_hdr);
        let mut long_hdr = good.clone();
        long_hdr[0] = 0xFF;
        cases.push(long_hdr);
        cases.push(vec![]);
        for c in &cases {
            assert!(dib_to_png(c).is_none());
        }
        for n in 0..good.len() {
            let _ = dib_to_png(&good[..n]);
        }
    }

    #[test]
    fn empty_pixel_stream_still_encodes() {
        let (w, h, raw) = decode(&encode_png(0, 0, &[]));
        assert_eq!((w, h, raw.len()), (0, 0, 0));
        // more than one stored block
        let big = vec![7u8; 70_000];
        let (_, _, raw) = decode(&encode_png(1, 1, &big));
        assert_eq!(raw, big);
    }

    /// PICFAndOfficeArtData holding one BLIP inside an OfficeArtFBSE.
    fn pic_record(
        mm: u16,
        blip_ty: u16,
        blip_inst: u16,
        uids: usize,
        payload: &[u8],
        wrap_in_fbse: bool,
    ) -> Vec<u8> {
        let mut blip = Vec::new();
        blip.extend_from_slice(&(blip_inst << 4).to_le_bytes());
        blip.extend_from_slice(&blip_ty.to_le_bytes());
        blip.extend_from_slice(&((uids * 16 + 1 + payload.len()) as u32).to_le_bytes());
        blip.extend_from_slice(&vec![0u8; uids * 16 + 1]);
        blip.extend_from_slice(payload);
        let body = if wrap_in_fbse {
            let mut fbse = vec![0u8; 36];
            fbse.extend_from_slice(&blip);
            let mut rec = vec![0x02, 0x00]; // ver 2, inst 0
            rec.extend_from_slice(&0xF007u16.to_le_bytes());
            rec.extend_from_slice(&(fbse.len() as u32).to_le_bytes());
            rec.extend_from_slice(&fbse);
            // container (ver 0xF) around the FBSE
            let mut cont = vec![0x0F, 0x00];
            cont.extend_from_slice(&0xF004u16.to_le_bytes());
            cont.extend_from_slice(&(rec.len() as u32).to_le_bytes());
            cont.extend_from_slice(&rec);
            cont
        } else {
            blip
        };
        let name: Vec<u8> = if mm == 0x0066 {
            vec![2, b'a', b'b']
        } else {
            vec![]
        };
        let cb_hdr = 68u16;
        let mut v = Vec::new();
        v.extend_from_slice(&((cb_hdr as usize + name.len() + body.len()) as u32).to_le_bytes());
        v.extend_from_slice(&cb_hdr.to_le_bytes());
        v.extend_from_slice(&mm.to_le_bytes());
        v.resize(cb_hdr as usize, 0);
        v.extend_from_slice(&name);
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn extracts_blips() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];
        let png = [0x89, b'P', b'N', b'G', 1, 2];
        let gif = *b"GIF89a--";
        let tif = [b'I', b'I', 42, 0];
        type BlipCase<'a> = (u16, u16, usize, &'a [u8], bool, Picture);
        let cases: [BlipCase<'_>; 11] = [
            (
                0x64,
                0xF01D,
                0x46A,
                &jpeg,
                true,
                Picture::Image {
                    ext: "jpg",
                    bytes: jpeg.to_vec(),
                },
            ),
            (
                0x64,
                0xF01D,
                0x46B,
                &jpeg,
                false,
                Picture::Image {
                    ext: "jpg",
                    bytes: jpeg.to_vec(),
                },
            ),
            (
                0x66,
                0xF01E,
                0x6E0,
                &png,
                true,
                Picture::Image {
                    ext: "png",
                    bytes: png.to_vec(),
                },
            ),
            // the signature wins over the record type
            (
                0x64,
                0xF01E,
                0x6E1,
                &gif,
                false,
                Picture::Image {
                    ext: "gif",
                    bytes: gif.to_vec(),
                },
            ),
            (
                0x64,
                0xF029,
                0x6E4,
                &tif,
                false,
                Picture::Image {
                    ext: "tif",
                    bytes: tif.to_vec(),
                },
            ),
            (0x64, 0xF01A, 0x3D4, &[1, 2, 3], false, Picture::Vector),
            (0x64, 0xF01B, 0x216, &[1, 2, 3], true, Picture::Vector),
            (0x64, 0xF01F, 0x7A8, &[1, 2, 3], false, Picture::Unreadable),
            // the two-UID variants of DIB and TIFF blips
            (0x64, 0xF01F, 0x7A9, &[1, 2, 3], false, Picture::Unreadable),
            (
                0x64,
                0xF029,
                0x6E5,
                &tif,
                true,
                Picture::Image {
                    ext: "tif",
                    bytes: tif.to_vec(),
                },
            ),
            // a blip record whose instance does not name a known format yields nothing
            (0x64, 0xF01E, 0x123, &png, false, Picture::Unreadable),
        ];
        for (mm, ty, inst, payload, fbse, want) in cases {
            let uids = if matches!(inst, 0x46B | 0x6E1 | 0x7A9 | 0x6E5) {
                2
            } else {
                1
            };
            let rec = pic_record(mm, ty, inst as u16, uids, payload, fbse);
            let mut data = vec![0xEE; 7];
            data.extend_from_slice(&rec);
            assert_eq!(extract(&data, 7), want, "type {ty:#06x}");
        }
        // DIB payloads become PNG
        let d = dib(1, 1, 24, 0, &[], &[], &[1, 2, 3, 0]);
        let rec = pic_record(0x64, 0xF01F, 0x7A8, 1, &d, false);
        let png = dib_to_png(&d).unwrap();
        assert_eq!(decode(&png).2, vec![0, 3, 2, 1]);
        assert_eq!(
            extract(&rec, 0),
            Picture::Image {
                ext: "png",
                bytes: png
            }
        );
    }

    #[test]
    fn the_first_bitmap_wins_and_an_fbse_without_a_blip_is_skipped() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 1];
        let png = [0x89, b'P', b'N', b'G', 1, 2];
        let first = pic_record(0x64, 0xF01D, 0x46A, 1, &jpeg, false);
        let second = pic_record(0x64, 0xF01E, 0x6E0, 1, &png, false);
        // header (68 bytes) followed by two blip records
        let mut data = first.clone();
        data.extend_from_slice(&second[68..]);
        let total = data.len() as u32;
        data[0..4].copy_from_slice(&total.to_le_bytes());
        assert_eq!(
            extract(&data, 0),
            Picture::Image {
                ext: "jpg",
                bytes: jpeg.to_vec()
            }
        );
        // an OfficeArtFBSE that embeds no BLIP is passed over; the record after it is read
        let mut fbse = vec![0x02, 0x00];
        fbse.extend_from_slice(&0xF007u16.to_le_bytes());
        fbse.extend_from_slice(&36u32.to_le_bytes());
        fbse.extend_from_slice(&[0u8; 36]);
        let mut data = second[..68].to_vec();
        data.extend_from_slice(&fbse);
        data.extend_from_slice(&second[68..]);
        let total = data.len() as u32;
        data[0..4].copy_from_slice(&total.to_le_bytes());
        assert_eq!(
            extract(&data, 0),
            Picture::Image {
                ext: "png",
                bytes: png.to_vec()
            }
        );
    }

    #[test]
    fn records_of_other_types_are_stepped_over() {
        let png = [0x89, b'P', b'N', b'G', 1, 2];
        let rec = pic_record(0x64, 0xF01E, 0x6E0, 1, &png, false);
        // an OfficeArtFOPT property record (neither a container, an FBSE nor a BLIP) ahead of
        // the blip is skipped by its length
        let mut other = vec![0x03, 0x00];
        other.extend_from_slice(&0xF00Bu16.to_le_bytes());
        other.extend_from_slice(&4u32.to_le_bytes());
        other.extend_from_slice(&[9, 9, 9, 9]);
        let mut data = rec[..68].to_vec();
        data.extend_from_slice(&other);
        data.extend_from_slice(&rec[68..]);
        let total = data.len() as u32;
        data[0..4].copy_from_slice(&total.to_le_bytes());
        assert_eq!(
            extract(&data, 0),
            Picture::Image {
                ext: "png",
                bytes: png.to_vec()
            }
        );
    }

    #[test]
    fn bit_masks_inside_an_extended_header_do_not_shift_the_pixels() {
        // A 52-byte header already contains the three masks, so the pixels start right after it
        // (a 40-byte header is followed by the masks and only then by the pixels).
        let masks: Vec<u8> = [0xF800u32, 0x07E0, 0x001F]
            .iter()
            .flat_map(|m| m.to_le_bytes())
            .collect();
        let mut d = dib(1, 1, 16, 3, &masks, &[], &[0xE0, 0x07, 0, 0]);
        d[0..4].copy_from_slice(&52u32.to_le_bytes());
        let (_, _, raw) = decode(&dib_to_png(&d).unwrap());
        assert_eq!(raw, vec![0, 0, 255, 0]);
    }

    #[test]
    fn unlocatable_pictures_are_unreadable() {
        assert_eq!(extract(&[], 0), Picture::Unreadable);
        assert_eq!(extract(&[1, 2, 3], 1), Picture::Unreadable);
        let rec = pic_record(0x64, 0xF01D, 0x46A, 1, &[0xFF, 0xD8, 0], false);
        // location beyond the stream, header size past the end, record with no blip
        assert_eq!(extract(&rec, u32::MAX), Picture::Unreadable);
        // a blip record that carries only its UID and tag bytes, no image data
        let hollow = pic_record(0x64, 0xF01D, 0x46A, 1, &[], false);
        assert_eq!(extract(&hollow, 0), Picture::Unreadable);
        let mut bad = rec.clone();
        bad[4..6].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert_eq!(extract(&bad, 0), Picture::Unreadable);
        let mut empty = rec.clone();
        empty[0..4].copy_from_slice(&68u32.to_le_bytes());
        assert_eq!(extract(&empty, 0), Picture::Unreadable);
        let mut deep = rec.clone();
        deep[68 + 7] = 0; // blip record length bytes damaged below
        deep[68 + 4..68 + 8].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(extract(&deep, 0), Picture::Unreadable);
        let shapefile = pic_record(0x66, 0xF01D, 0x46A, 1, &[0xFF, 0xD8, 0], false);
        for n in 0..shapefile.len() {
            let _ = extract(&shapefile[..n], 0);
        }
        // nesting beyond the depth limit is ignored
        let mut nested = rec[68..].to_vec();
        for _ in 0..8 {
            let mut c = vec![0x0F, 0x00];
            c.extend_from_slice(&0xF004u16.to_le_bytes());
            c.extend_from_slice(&(nested.len() as u32).to_le_bytes());
            c.extend_from_slice(&nested);
            nested = c;
        }
        let mut data = rec[..68].to_vec();
        let total = (68 + nested.len()) as u32;
        data[0..4].copy_from_slice(&total.to_le_bytes());
        data.extend_from_slice(&nested);
        assert_eq!(extract(&data, 0), Picture::Unreadable);
    }
}
