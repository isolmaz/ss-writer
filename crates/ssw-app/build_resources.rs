//! The Windows resources of `SSWriter.exe` (application and document icons, `VERSIONINFO`),
//! written as a 32-bit `.res` file that the MSVC linker merges into the executable's `.rsrc`
//! section. The format is small: a 32-byte empty record, then one record per resource (a
//! 32-byte header with numeric type and name, the data, padding to four bytes).
//!
//! `build.rs` includes this file as a module. It is plain `std` code on purpose (the build
//! script has no dependencies besides `embed-manifest`), and `xtask` compiles it for its tests
//! (`#[path]` in `xtask/src/main.rs`), which build resources from real icon files and parse
//! them back.
//!
//! The manifest is not part of this file: `embed-manifest` links it separately
//! (`/MANIFEST:EMBED`), so there is exactly one `RT_MANIFEST`.

/// `RT_ICON`: one image of an icon.
const RT_ICON: u16 = 3;
/// `RT_GROUP_ICON`: the directory of an icon (what `LoadIcon` and Explorer look up).
const RT_GROUP_ICON: u16 = 14;
/// `RT_VERSION`: the `VS_VERSIONINFO` block.
const RT_VERSION: u16 = 16;

/// `MOVEABLE | PURE | DISCARDABLE`, the flags `rc.exe` writes for icons. The linker ignores
/// them; they are recorded for tools that dump the `.res` file.
const ICON_MEMORY_FLAGS: u16 = 0x1030;
/// `MOVEABLE | PURE`, what `rc.exe` writes for `VERSIONINFO`.
const VERSION_MEMORY_FLAGS: u16 = 0x0030;

/// `LANG_NEUTRAL`: icons are looked up without regard for the user's language.
const LANGUAGE_NEUTRAL: u16 = 0;
/// English (United States): the language of the version strings.
const LANGUAGE_EN_US: u16 = 0x0409;
/// Code page 1200 (UTF-16), the second half of the `Translation` pair.
const CODE_PAGE_UNICODE: u16 = 1200;

/// `VS_FIXEDFILEINFO.dwSignature`.
const FIXED_FILE_INFO_SIGNATURE: u32 = 0xFEEF_04BD;

/// One image of an `.ico` file: the directory fields the resource needs and the image data
/// (BMP or PNG).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IconImage {
    /// Width in pixels; 0 stands for 256.
    width: u8,
    /// Height in pixels; 0 stands for 256.
    height: u8,
    /// Colour planes.
    planes: u16,
    /// Bits per pixel.
    bit_count: u16,
    /// The image bytes as stored in the file.
    data: Vec<u8>,
}

fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Splits an `.ico` file into its images.
pub fn parse_ico(bytes: &[u8]) -> Result<Vec<IconImage>, String> {
    let header = bytes
        .get(..6)
        .ok_or("the icon file is shorter than its header")?;
    if header[..4] != [0, 0, 1, 0] {
        return Err("not an icon file: the reserved word or the type is wrong".to_owned());
    }
    let count = usize::from(le16(&header[4..]));
    if count == 0 {
        return Err("the icon file holds no images".to_owned());
    }
    let mut images = Vec::with_capacity(count);
    for index in 0..count {
        let start = 6 + 16 * index;
        let entry = bytes
            .get(start..start + 16)
            .ok_or_else(|| format!("icon directory entry {index} is truncated"))?;
        let length = le32(&entry[8..]) as usize;
        let offset = le32(&entry[12..]) as usize;
        let data = offset
            .checked_add(length)
            .and_then(|end| bytes.get(offset..end))
            .ok_or_else(|| format!("icon image {index} lies outside the file"))?;
        images.push(IconImage {
            width: entry[0],
            height: entry[1],
            planes: le16(&entry[4..]),
            bit_count: le16(&entry[6..]),
            data: data.to_vec(),
        });
    }
    Ok(images)
}

/// Pads `bytes` with zeros to a multiple of four.
fn align4(bytes: &mut Vec<u8>) {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
}

/// Appends `text` as zero-terminated UTF-16LE.
fn push_utf16z(bytes: &mut Vec<u8>, text: &str) {
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.extend_from_slice(&[0, 0]);
}

/// One block of a `VS_VERSIONINFO` tree: `wLength`, `wValueLength`, `wType` (1 = text,
/// 0 = binary), the zero-terminated key, padding, the value, then the children, each preceded
/// by padding to four bytes. `wLength` covers everything but the padding after the block.
fn block(
    key: &str,
    value_length: usize,
    text: bool,
    value: &[u8],
    children: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = vec![0, 0]; // wLength, filled in below
    // Version blocks are a few hundred bytes: the lengths fit 16 bits.
    out.extend_from_slice(&(value_length as u16).to_le_bytes());
    out.extend_from_slice(&u16::from(text).to_le_bytes());
    push_utf16z(&mut out, key);
    align4(&mut out);
    out.extend_from_slice(value);
    for child in children {
        align4(&mut out);
        out.extend_from_slice(child);
    }
    let length = out.len() as u16;
    out[..2].copy_from_slice(&length.to_le_bytes());
    out
}

/// The `VS_VERSIONINFO` data: fixed file info (file and product version both `version`), the
/// `StringFileInfo` table for English (United States) / Unicode with `strings`, and the
/// matching `Translation`.
fn version_info(version: [u16; 4], prerelease: bool, strings: &[(&str, &str)]) -> Vec<u8> {
    let [major, minor, patch, build] = version;
    let most = (u32::from(major) << 16) | u32::from(minor);
    let least = (u32::from(patch) << 16) | u32::from(build);
    let mut fixed = Vec::with_capacity(52);
    for field in [
        FIXED_FILE_INFO_SIGNATURE,
        0x0001_0000,                      // dwStrucVersion
        most,                             // dwFileVersionMS
        least,                            // dwFileVersionLS
        most,                             // dwProductVersionMS
        least,                            // dwProductVersionLS
        0x3F,                             // dwFileFlagsMask (VS_FFI_FILEFLAGSMASK)
        if prerelease { 0x2 } else { 0 }, // dwFileFlags (VS_FF_PRERELEASE)
        0x0004_0004,                      // dwFileOS: VOS_NT_WINDOWS32
        1,                                // dwFileType: VFT_APP
        0,                                // dwFileSubtype
        0,                                // dwFileDateMS
        0,                                // dwFileDateLS
    ] {
        fixed.extend_from_slice(&field.to_le_bytes());
    }
    let entries: Vec<Vec<u8>> = strings
        .iter()
        .map(|(name, value)| {
            let mut data = Vec::new();
            push_utf16z(&mut data, value);
            // A string's wValueLength counts UTF-16 units including the terminator.
            block(name, data.len() / 2, true, &data, &[])
        })
        .collect();
    let table = block(
        &format!("{LANGUAGE_EN_US:04X}{CODE_PAGE_UNICODE:04X}"),
        0,
        true,
        &[],
        &entries,
    );
    let string_file_info = block("StringFileInfo", 0, true, &[], &[table]);
    let translation = (u32::from(CODE_PAGE_UNICODE) << 16) | u32::from(LANGUAGE_EN_US);
    let var = block("Translation", 4, false, &translation.to_le_bytes(), &[]);
    let var_file_info = block("VarFileInfo", 0, true, &[], &[var]);
    block(
        "VS_VERSION_INFO",
        fixed.len(),
        false,
        &fixed,
        &[string_file_info, var_file_info],
    )
}

/// A `.res` file under construction.
#[derive(Debug)]
pub struct ResourceFile {
    bytes: Vec<u8>,
}

impl Default for ResourceFile {
    /// A file holding only the mandatory empty first record.
    fn default() -> Self {
        let mut file = Self { bytes: Vec::new() };
        file.push_record(0, 0, 0, 0, &[]);
        file
    }
}

impl ResourceFile {
    /// Appends one record: header (data size, header size 32, numeric `kind` and `name`, data
    /// version 0, memory flags, `language`, version 0, characteristics 0), the data, padding.
    fn push_record(&mut self, kind: u16, name: u16, memory_flags: u16, language: u16, data: &[u8]) {
        let bytes = &mut self.bytes;
        // Resource data is at most a few hundred kilobytes: the size fits 32 bits.
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&32_u32.to_le_bytes());
        for ordinal in [kind, name] {
            bytes.extend_from_slice(&0xFFFF_u16.to_le_bytes());
            bytes.extend_from_slice(&ordinal.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&memory_flags.to_le_bytes());
        bytes.extend_from_slice(&language.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(data);
        align4(bytes);
    }

    /// Adds an icon: one `RT_ICON` per image (ids `first_icon_id`, `first_icon_id + 1`, …)
    /// and the `RT_GROUP_ICON` `group_id` that lists them.
    pub fn add_icon(
        &mut self,
        group_id: u16,
        first_icon_id: u16,
        images: &[IconImage],
    ) -> Result<(), String> {
        let count = u16::try_from(images.len())
            .ok()
            .filter(|count| first_icon_id.checked_add(*count).is_some())
            .ok_or_else(|| {
                format!(
                    "{} icon images starting at id {first_icon_id} do not fit the 16-bit id space",
                    images.len()
                )
            })?;
        let mut group = Vec::with_capacity(6 + 14 * images.len());
        group.extend_from_slice(&[0, 0, 1, 0]); // reserved, type 1 = icon
        group.extend_from_slice(&count.to_le_bytes());
        for (index, image) in images.iter().enumerate() {
            // Checked above: the last id does not overflow.
            let id = first_icon_id + index as u16;
            group.extend_from_slice(&[image.width, image.height, 0, 0]);
            group.extend_from_slice(&image.planes.to_le_bytes());
            group.extend_from_slice(&image.bit_count.to_le_bytes());
            group.extend_from_slice(&(image.data.len() as u32).to_le_bytes());
            group.extend_from_slice(&id.to_le_bytes()); // the RT_ICON id replaces the offset
            self.push_record(
                RT_ICON,
                id,
                ICON_MEMORY_FLAGS,
                LANGUAGE_NEUTRAL,
                &image.data,
            );
        }
        self.push_record(
            RT_GROUP_ICON,
            group_id,
            ICON_MEMORY_FLAGS,
            LANGUAGE_NEUTRAL,
            &group,
        );
        Ok(())
    }

    /// Adds `RT_VERSION` 1: file and product version `version` (major, minor, patch, build)
    /// and the given string table entries, in English (United States) with a Unicode code page.
    pub fn add_version(&mut self, version: [u16; 4], prerelease: bool, strings: &[(&str, &str)]) {
        self.push_record(
            RT_VERSION,
            1,
            VERSION_MEMORY_FLAGS,
            LANGUAGE_EN_US,
            &version_info(version, prerelease, strings),
        );
    }

    /// The finished `.res` file.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icons;

    /// A parsed record of a `.res` file.
    #[derive(Debug)]
    struct Record {
        kind: u16,
        name: u16,
        memory_flags: u16,
        language: u16,
        data: Vec<u8>,
    }

    /// Parses a `.res` file, asserting its structure (header size, ordinals, alignment).
    fn parse_records(bytes: &[u8]) -> Vec<Record> {
        let mut records = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            assert!(at.is_multiple_of(4), "records start on a DWORD boundary");
            let data_size = le32(&bytes[at..]) as usize;
            assert_eq!(le32(&bytes[at + 4..]), 32, "header size");
            assert_eq!(le16(&bytes[at + 8..]), 0xFFFF, "numeric type");
            assert_eq!(le16(&bytes[at + 12..]), 0xFFFF, "numeric name");
            assert_eq!(le32(&bytes[at + 16..]), 0, "data version");
            assert_eq!(le32(&bytes[at + 24..]), 0, "version");
            assert_eq!(le32(&bytes[at + 28..]), 0, "characteristics");
            records.push(Record {
                kind: le16(&bytes[at + 10..]),
                name: le16(&bytes[at + 14..]),
                memory_flags: le16(&bytes[at + 20..]),
                language: le16(&bytes[at + 22..]),
                data: bytes[at + 32..at + 32 + data_size].to_vec(),
            });
            at = (at + 32 + data_size).next_multiple_of(4);
        }
        assert_eq!(at, bytes.len(), "no bytes after the last record");
        records
    }

    /// A parsed `VS_VERSIONINFO`-family block.
    #[derive(Debug)]
    struct Block {
        key: String,
        text: bool,
        value_length: usize,
        value: Vec<u8>,
        children: Vec<Block>,
    }

    fn read_block(data: &[u8]) -> Block {
        let length = usize::from(le16(data));
        let value_length = usize::from(le16(&data[2..]));
        let text = le16(&data[4..]) == 1;
        let mut at = 6;
        let mut key = Vec::new();
        loop {
            let unit = le16(&data[at..]);
            at += 2;
            if unit == 0 {
                break;
            }
            key.push(unit);
        }
        at = at.next_multiple_of(4);
        let value_bytes = if text { value_length * 2 } else { value_length };
        let value = data[at..at + value_bytes].to_vec();
        at += value_bytes;
        let mut children = Vec::new();
        while at < length {
            at = at.next_multiple_of(4);
            children.push(read_block(&data[at..length]));
            at += usize::from(le16(&data[at..]));
        }
        assert_eq!(at, length, "the children end exactly at wLength");
        Block {
            key: String::from_utf16(&key).unwrap(),
            text,
            value_length,
            value,
            children,
        }
    }

    fn utf16(bytes: &[u8]) -> String {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| le16(pair))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    /// An ICO file with images rendered from a solid drawing, as `cargo xtask icons` writes it.
    fn sample_ico() -> Vec<u8> {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8" viewBox="0 0 8 8"><rect width="8" height="8" fill="#1874D3"/></svg>"##;
        icons::ico_from_svg(svg).unwrap()
    }

    #[test]
    fn a_new_file_holds_only_the_empty_first_record() {
        let bytes = ResourceFile::default().into_bytes();
        assert_eq!(bytes.len(), 32);
        let records = parse_records(&bytes);
        assert_eq!(records.len(), 1);
        assert_eq!((records[0].kind, records[0].name), (0, 0));
        assert!(records[0].data.is_empty());
        // The canonical 32-byte prologue of every .res file.
        assert_eq!(
            bytes,
            [
                0, 0, 0, 0, 32, 0, 0, 0, 0xFF, 0xFF, 0, 0, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        );
    }

    #[test]
    fn icon_images_become_rt_icon_records_and_a_group_that_lists_them() {
        let ico = sample_ico();
        let images = parse_ico(&ico).unwrap();
        assert_eq!(images.len(), icons::SIZES.len());
        let mut file = ResourceFile::default();
        file.add_icon(7, 20, &images).unwrap();
        let records = parse_records(&file.into_bytes());

        // Empty record, one RT_ICON per image (ids 20..), then the RT_GROUP_ICON 7.
        assert_eq!(records.len(), 1 + images.len() + 1);
        for (index, image) in images.iter().enumerate() {
            let record = &records[1 + index];
            assert_eq!(record.kind, RT_ICON);
            assert_eq!(usize::from(record.name), 20 + index);
            assert_eq!(record.language, LANGUAGE_NEUTRAL);
            assert_eq!(record.memory_flags, ICON_MEMORY_FLAGS);
            assert_eq!(record.data, image.data, "image {index} is stored unchanged");
        }
        let group = &records[records.len() - 1];
        assert_eq!((group.kind, group.name), (RT_GROUP_ICON, 7));
        assert_eq!(group.data[..4], [0, 0, 1, 0]);
        assert_eq!(usize::from(le16(&group.data[4..])), images.len());
        assert_eq!(group.data.len(), 6 + 14 * images.len());
        for (index, size) in icons::SIZES.iter().enumerate() {
            let entry = &group.data[6 + 14 * index..6 + 14 * (index + 1)];
            let expected = if *size == 256 { 0 } else { *size as u8 };
            assert_eq!(entry[..2], [expected, expected], "size of entry {index}");
            assert_eq!(le16(&entry[4..]), 1, "planes");
            assert_eq!(le16(&entry[6..]), 32, "bits per pixel");
            assert_eq!(
                le32(&entry[8..]) as usize,
                images[index].data.len(),
                "bytes in resource"
            );
            assert_eq!(usize::from(le16(&entry[12..])), 20 + index, "RT_ICON id");
        }
    }

    #[test]
    fn two_icons_can_share_one_file_with_separate_id_ranges() {
        let images = parse_ico(&sample_ico()).unwrap();
        let mut file = ResourceFile::default();
        file.add_icon(1, 1, &images).unwrap();
        file.add_icon(2, 101, &images[..3]).unwrap();
        let records = parse_records(&file.into_bytes());
        let groups: Vec<u16> = records
            .iter()
            .filter(|r| r.kind == RT_GROUP_ICON)
            .map(|r| r.name)
            .collect();
        assert_eq!(groups, [1, 2]);
        let icon_ids: Vec<u16> = records
            .iter()
            .filter(|r| r.kind == RT_ICON)
            .map(|r| r.name)
            .collect();
        assert_eq!(icon_ids.len(), images.len() + 3);
        assert!(icon_ids.contains(&1) && icon_ids.contains(&8));
        assert!(icon_ids.contains(&101) && icon_ids.contains(&103));
    }

    #[test]
    fn icon_ids_that_would_overflow_are_refused() {
        let images = parse_ico(&sample_ico()).unwrap();
        let mut file = ResourceFile::default();
        let error = file.add_icon(1, u16::MAX - 2, &images).unwrap_err();
        assert!(error.contains("16-bit"), "{error}");
        assert_eq!(file.into_bytes().len(), 32, "nothing was added");
    }

    #[test]
    fn records_are_padded_to_four_bytes_whatever_the_data_length() {
        let mut file = ResourceFile::default();
        file.push_record(10, 1, 0, 0, &[1]);
        file.push_record(10, 2, 0, 0, &[1, 2, 3, 4]);
        file.push_record(10, 3, 0, 0, &[]);
        file.push_record(10, 4, 0, 0, &[9; 7]);
        let bytes = file.into_bytes();
        assert!(bytes.len().is_multiple_of(4));
        let records = parse_records(&bytes);
        let sizes: Vec<usize> = records.iter().map(|r| r.data.len()).collect();
        assert_eq!(sizes, [0, 1, 4, 0, 7]);
        assert_eq!(records[4].data, [9; 7]);
    }

    #[test]
    fn version_info_carries_fixed_numbers_strings_and_the_translation() {
        let mut file = ResourceFile::default();
        file.add_version(
            [1, 2, 3, 0],
            false,
            &[
                ("ProductName", "SS Writer"),
                ("CompanyName", "isolmaz"),
                ("LegalCopyright", "\u{a9} isolmaz \u{10348}"),
                ("Comments", ""),
            ],
        );
        let records = parse_records(&file.into_bytes());
        assert_eq!(records.len(), 2);
        let record = &records[1];
        assert_eq!((record.kind, record.name), (RT_VERSION, 1));
        assert_eq!(record.language, LANGUAGE_EN_US);
        assert_eq!(record.memory_flags, VERSION_MEMORY_FLAGS);

        let root = read_block(&record.data);
        assert_eq!(
            usize::from(le16(&record.data)),
            record.data.len(),
            "wLength"
        );
        assert_eq!(root.key, "VS_VERSION_INFO");
        assert!(!root.text);
        assert_eq!(root.value_length, 52);
        let fixed: Vec<u32> = root
            .value
            .as_chunks::<4>()
            .0
            .iter()
            .map(|quad| le32(quad))
            .collect();
        assert_eq!(fixed[0], 0xFEEF_04BD, "signature");
        assert_eq!(fixed[1], 0x0001_0000, "structure version");
        assert_eq!(
            (fixed[2], fixed[3]),
            (0x0001_0002, 0x0003_0000),
            "file version"
        );
        assert_eq!(
            (fixed[4], fixed[5]),
            (0x0001_0002, 0x0003_0000),
            "product version"
        );
        assert_eq!((fixed[6], fixed[7]), (0x3F, 0), "flags mask and flags");
        assert_eq!(
            (fixed[8], fixed[9]),
            (0x0004_0004, 1),
            "NT Win32 application"
        );

        assert_eq!(root.children.len(), 2);
        let string_file_info = &root.children[0];
        assert_eq!(string_file_info.key, "StringFileInfo");
        let table = &string_file_info.children[0];
        assert_eq!(table.key, "040904B0", "English (United States), Unicode");
        let strings: Vec<(String, String)> = table
            .children
            .iter()
            .map(|entry| {
                assert!(entry.text);
                // wValueLength counts UTF-16 units including the terminator.
                assert_eq!(entry.value.len(), entry.value_length * 2);
                let value = utf16(&entry.value);
                assert_eq!(value.chars().last(), Some('\0'));
                (entry.key.clone(), value.trim_end_matches('\0').to_owned())
            })
            .collect();
        assert_eq!(
            strings,
            [
                ("ProductName".to_owned(), "SS Writer".to_owned()),
                ("CompanyName".to_owned(), "isolmaz".to_owned()),
                (
                    "LegalCopyright".to_owned(),
                    "\u{a9} isolmaz \u{10348}".to_owned()
                ),
                ("Comments".to_owned(), String::new()),
            ]
        );

        let var_file_info = &root.children[1];
        assert_eq!(var_file_info.key, "VarFileInfo");
        let translation = &var_file_info.children[0];
        assert_eq!(translation.key, "Translation");
        assert!(!translation.text);
        assert_eq!(translation.value, [0x09, 0x04, 0xB0, 0x04]);
    }

    #[test]
    fn a_pre_release_sets_the_prerelease_flag_and_the_build_number_goes_into_the_low_word() {
        let mut file = ResourceFile::default();
        file.add_version([0, 9, 1, 7], true, &[]);
        let records = parse_records(&file.into_bytes());
        let root = read_block(&records[1].data);
        let fixed: Vec<u32> = root
            .value
            .as_chunks::<4>()
            .0
            .iter()
            .map(|quad| le32(quad))
            .collect();
        assert_eq!((fixed[2], fixed[3]), (0x0000_0009, 0x0001_0007));
        assert_eq!(fixed[7], 0x2, "VS_FF_PRERELEASE");
        assert_eq!(root.children[0].children[0].children.len(), 0, "no strings");
    }

    #[test]
    fn parse_ico_reads_every_directory_entry_of_a_generated_file() {
        let ico = sample_ico();
        let images = parse_ico(&ico).unwrap();
        let first = &images[0];
        assert_eq!((first.width, first.height), (16, 16));
        assert_eq!((first.planes, first.bit_count), (1, 32));
        assert_eq!(le32(&first.data), 40, "the 16 px image is a BMP");
        let last = &images[images.len() - 1];
        assert_eq!((last.width, last.height), (0, 0), "256 is stored as 0");
        assert_eq!(last.data[..8], *b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn parse_ico_rejects_files_that_are_not_complete_icons() {
        let ico = sample_ico();
        assert!(parse_ico(&ico[..5]).unwrap_err().contains("header"));
        assert!(
            parse_ico(b"GIF89a....")
                .unwrap_err()
                .contains("not an icon")
        );
        let mut cursor = ico.clone();
        cursor[2] = 2; // type 2 = cursor
        assert!(parse_ico(&cursor).unwrap_err().contains("not an icon"));
        let mut empty = ico.clone();
        empty[4..6].copy_from_slice(&[0, 0]);
        assert!(parse_ico(&empty).unwrap_err().contains("no images"));
        let truncated_directory = &ico[..6 + 10];
        assert!(
            parse_ico(truncated_directory)
                .unwrap_err()
                .contains("entry 0")
        );
        let truncated_data = &ico[..ico.len() - 1];
        assert!(parse_ico(truncated_data).unwrap_err().contains("image 7"));
        let mut wrapping = ico.clone();
        wrapping[6 + 8..6 + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        wrapping[6 + 12..6 + 16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_ico(&wrapping).unwrap_err().contains("image 0"));
    }
}
