//! The built `SSWriter.exe` carries the application icon (`RT_GROUP_ICON` 1), the document icon
//! (`RT_GROUP_ICON` 2, what the installer's ProgID `DefaultIcon` entries point at) and its
//! `VERSIONINFO`. The resources are read straight from the PE file: no code is loaded and
//! nothing is executed.

mod pe;

/// `RT_ICON`.
const RT_ICON: u32 = 3;
/// `RT_GROUP_ICON`.
const RT_GROUP_ICON: u32 = 14;
/// `RT_VERSION`.
const RT_VERSION: u32 = 16;

fn le16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn executable() -> Vec<u8> {
    std::fs::read(env!("CARGO_BIN_EXE_SSWriter")).unwrap()
}

fn icon_file(name: &str) -> Vec<u8> {
    let path = format!("{}/../../assets/icons/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

#[test]
fn the_executable_embeds_the_application_and_the_document_icon_unchanged() {
    let bytes = executable();
    let image = pe::Image::parse(&bytes).expect("SSWriter.exe has no resource directory");
    assert_eq!(
        image.resource_ids(RT_GROUP_ICON),
        [1, 2],
        "group icon 1 is the application icon, 2 the document icon"
    );
    for (group_id, file) in [(1, "app.ico"), (2, "document.ico")] {
        let ico = icon_file(file);
        let group = image
            .resource(RT_GROUP_ICON, group_id)
            .unwrap_or_else(|| panic!("group icon {group_id} is missing"));
        assert_eq!(group[..4], [0, 0, 1, 0], "{file}: GRPICONDIR header");
        let count = usize::from(le16(&group[4..]));
        assert_eq!(count, usize::from(le16(&ico[4..])), "{file}: image count");
        let widths: Vec<u8> = (0..count).map(|index| group[6 + 14 * index]).collect();
        assert_eq!(
            widths,
            [16, 20, 24, 32, 40, 48, 64, 0],
            "{file}: sizes (0 stands for 256)"
        );
        for index in 0..count {
            let entry = &group[6 + 14 * index..][..14];
            let source = &ico[6 + 16 * index..][..16];
            assert_eq!(entry[..2], source[..2], "{file}: size of image {index}");
            assert_eq!(entry[4..8], source[4..8], "{file}: planes and bit count");
            let length = le32(&source[8..]) as usize;
            let offset = le32(&source[12..]) as usize;
            assert_eq!(
                le32(&entry[8..]) as usize,
                length,
                "{file}: image {index} length"
            );
            let icon_id = u32::from(le16(&entry[12..]));
            let data = image
                .resource(RT_ICON, icon_id)
                .unwrap_or_else(|| panic!("{file}: RT_ICON {icon_id} is missing"));
            assert!(
                data == &ico[offset..offset + length],
                "{file}: image {index} is embedded unchanged"
            );
        }
    }
}

/// A block of a `VS_VERSIONINFO` tree.
struct Block {
    key: String,
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
    Block {
        key: String::from_utf16(&key).unwrap(),
        value,
        children,
    }
}

fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16(&units)
        .unwrap()
        .trim_end_matches('\0')
        .to_owned()
}

fn child<'a>(block: &'a Block, key: &str) -> &'a Block {
    block
        .children
        .iter()
        .find(|child| child.key == key)
        .unwrap_or_else(|| panic!("{} has no {key} block", block.key))
}

#[test]
fn the_executable_carries_version_information_for_ss_writer() {
    let bytes = executable();
    let image = pe::Image::parse(&bytes).expect("SSWriter.exe has no resource directory");
    assert_eq!(image.resource_ids(RT_VERSION), [1]);
    let root = read_block(image.resource(RT_VERSION, 1).unwrap());
    assert_eq!(root.key, "VS_VERSION_INFO");

    let major: u32 = env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap();
    let minor: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap();
    let patch: u32 = env!("CARGO_PKG_VERSION_PATCH").parse().unwrap();
    assert_eq!(root.value.len(), 52, "VS_FIXEDFILEINFO");
    assert_eq!(le32(&root.value), 0xFEEF_04BD, "signature");
    assert_eq!(
        le32(&root.value[8..]),
        (major << 16) | minor,
        "file version, high"
    );
    assert_eq!(le32(&root.value[12..]), patch << 16, "file version, low");
    assert_eq!(
        le32(&root.value[16..]),
        (major << 16) | minor,
        "product version, high"
    );
    assert_eq!(le32(&root.value[20..]), patch << 16, "product version, low");
    assert_eq!(le32(&root.value[32..]), 0x0004_0004, "VOS_NT_WINDOWS32");
    assert_eq!(le32(&root.value[36..]), 1, "VFT_APP");

    let table = child(child(&root, "StringFileInfo"), "040904B0");
    let string = |name: &str| utf16(&child(table, name).value);
    assert_eq!(string("ProductName"), "SS Writer");
    assert_eq!(string("FileDescription"), "SS Writer");
    assert_eq!(string("CompanyName"), "isolmaz");
    assert_eq!(string("InternalName"), "SSWriter");
    assert_eq!(string("OriginalFilename"), "SSWriter.exe");
    assert_eq!(string("ProductVersion"), env!("CARGO_PKG_VERSION"));
    assert_eq!(string("FileVersion"), format!("{major}.{minor}.{patch}.0"));
    assert!(string("LegalCopyright").contains("isolmaz"));

    let translation = child(child(&root, "VarFileInfo"), "Translation");
    assert_eq!(
        translation.value,
        [0x09, 0x04, 0xB0, 0x04],
        "en-US, Unicode"
    );
}
