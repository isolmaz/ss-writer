//! A minimal PE32+ (x64) resource reader for the tests that inspect the built `SSWriter.exe`.
//! The resources are read straight from the file: no code is loaded and nothing is executed.

const SUBDIRECTORY_FLAG: u32 = 0x8000_0000;

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([raw[0], raw[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

/// A PE32+ file with a resource directory.
pub struct Image<'a> {
    bytes: &'a [u8],
    sections: usize,
    section_table: usize,
    /// File offset of the resource directory root.
    root: usize,
}

impl<'a> Image<'a> {
    /// Parses the headers of `bytes`; `None` unless it is a PE32+ file with a resource
    /// directory.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        let pe = u32_at(bytes, 0x3C)? as usize;
        if bytes.get(pe..pe + 4)? != b"PE\0\0" {
            return None;
        }
        let sections = usize::from(u16_at(bytes, pe + 6)?);
        let optional_size = usize::from(u16_at(bytes, pe + 20)?);
        let optional = pe + 24;
        // PE32+ (x64): the data directories start 112 bytes into the optional header.
        if u16_at(bytes, optional)? != 0x20B {
            return None;
        }
        let section_table = optional + optional_size;
        let resource_rva = u32_at(bytes, optional + 112 + 2 * 8)?;
        if resource_rva == 0 {
            return None;
        }
        let mut image = Self {
            bytes,
            sections,
            section_table,
            root: 0,
        };
        image.root = image.rva_to_offset(resource_rva)?;
        Some(image)
    }

    /// File offset of the relative virtual address `rva`.
    fn rva_to_offset(&self, rva: u32) -> Option<usize> {
        for index in 0..self.sections {
            let header = self.section_table + index * 40;
            let virtual_size = u32_at(self.bytes, header + 8)?;
            let virtual_address = u32_at(self.bytes, header + 12)?;
            let raw_size = u32_at(self.bytes, header + 16)?;
            let raw_pointer = u32_at(self.bytes, header + 20)?;
            let span = virtual_size.max(raw_size);
            if rva >= virtual_address && rva - virtual_address < span {
                return Some((raw_pointer + (rva - virtual_address)) as usize);
            }
        }
        None
    }

    /// The integer-id entries of the resource directory at `directory` (relative to the
    /// resource root) as `(id, raw OffsetToData field)`. Named entries are skipped.
    fn entries(&self, directory: u32) -> Vec<(u32, u32)> {
        let read = || -> Option<Vec<(u32, u32)>> {
            let table = self.root + directory as usize;
            let named = usize::from(u16_at(self.bytes, table + 12)?);
            let ids = usize::from(u16_at(self.bytes, table + 14)?);
            (named..named + ids)
                .map(|index| {
                    let entry = table + 16 + index * 8;
                    Some((u32_at(self.bytes, entry)?, u32_at(self.bytes, entry + 4)?))
                })
                .collect()
        };
        read().unwrap_or_default()
    }

    /// The subdirectory of `directory` stored under `id`, relative to the resource root.
    fn subdirectory(&self, directory: u32, id: u32) -> Option<u32> {
        let (_, offset) = self
            .entries(directory)
            .into_iter()
            .find(|(entry, _)| *entry == id)?;
        (offset & SUBDIRECTORY_FLAG != 0).then_some(offset & !SUBDIRECTORY_FLAG)
    }

    /// The integer ids of all resources of type `kind`, in directory order (ascending).
    pub fn resource_ids(&self, kind: u32) -> Vec<u32> {
        match self.subdirectory(0, kind) {
            Some(directory) => self
                .entries(directory)
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
            None => Vec::new(),
        }
    }

    /// The data of resource `kind` / `id` in its first language.
    pub fn resource(&self, kind: u32, id: u32) -> Option<&'a [u8]> {
        let by_name = self.subdirectory(self.subdirectory(0, kind)?, id)?;
        let (_, leaf) = *self.entries(by_name).first()?;
        if leaf & SUBDIRECTORY_FLAG != 0 {
            return None;
        }
        let entry = self.root + leaf as usize;
        let data_rva = u32_at(self.bytes, entry)?;
        let size = u32_at(self.bytes, entry + 4)? as usize;
        let start = self.rva_to_offset(data_rva)?;
        self.bytes.get(start..start.checked_add(size)?)
    }
}
