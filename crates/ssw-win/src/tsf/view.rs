//! The document as TSF sees it: the host's text with the IME composition spliced in.
//!
//! TSF addresses a text store in UTF-16 code units ("application character positions", ACPs);
//! the host works in UTF-8 bytes. [`View`] caches the host text as UTF-16 together with a block
//! table that converts byte and UTF-16 offsets (the composition string and its attributes use
//! [`Utf16Map`]), updates both incrementally from host edits, and overlays the one
//! active [`Composition`]: while an IME composes, its string is part of the TSF-visible text but
//! not of the host document (the host displays it as an overlay, see
//! [`ImeEvent`](ssw_ui::input::ImeEvent)). Everything here is plain Rust without COM.

use std::ops::Range;

use ssw_core::edit::Selection as HostSelection;
use ssw_core::text::Utf16Map;
use ssw_ui::input::{ImeAttr, ImeAttrStyle, ImeEvent};

/// A selection in ACPs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AcpSelection {
    pub(super) start: usize,
    pub(super) end: usize,
    /// The caret is at `start` (a selection made right to left).
    pub(super) backward: bool,
}

impl AcpSelection {
    pub(super) const fn caret(position: usize) -> Self {
        Self {
            start: position,
            end: position,
            backward: false,
        }
    }

    /// The end where the visible caret sits.
    pub(super) const fn head(&self) -> usize {
        if self.backward { self.start } else { self.end }
    }
}

/// A replacement in ACPs: the shape of `TS_TEXTCHANGE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TextChange {
    pub(super) start: usize,
    pub(super) old_end: usize,
    pub(super) new_end: usize,
}

impl TextChange {
    fn delta(&self) -> isize {
        self.new_end as isize - self.old_end as isize
    }

    /// Moves an anchor through the change. Anchors inside the replaced range collapse to the end
    /// of the new text (`forward`) or to its start; anchors at or after the old end shift. An
    /// anchor at the position of a pure insertion stays put.
    pub(super) fn adjust(&self, position: usize, forward: bool) -> usize {
        if position < self.old_end && position > self.start {
            if forward { self.new_end } else { self.start }
        } else if position >= self.old_end {
            if position == self.start && self.old_end == self.start {
                position
            } else {
                (position as isize + self.delta()) as usize
            }
        } else {
            position
        }
    }
}

/// The single change covering two consecutive changes (`second` is expressed against the text
/// after `first`).
pub(super) fn merge_changes(first: TextChange, second: TextChange) -> TextChange {
    let start = first.start.min(second.start);
    let new_end = second.adjust(first.new_end, true).max(second.new_end);
    let old_end = if second.old_end > first.new_end {
        (second.old_end as isize - first.delta()) as usize
    } else {
        first.old_end
    };
    TextChange {
        start,
        old_end,
        new_end,
    }
}

/// A display attribute run inside the composition string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AttrRun {
    /// UTF-16 range inside the composition string.
    pub(super) range: Range<usize>,
    pub(super) style: ImeAttrStyle,
}

/// The IME composition: a string displayed by the host at `host_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Composition {
    /// UTF-8 offset in the host text where the host displays the composition.
    pub(super) host_at: usize,
    /// The composition string.
    pub(super) units: Vec<u16>,
    /// TSF's selection, in ACPs of the TSF-visible text.
    pub(super) selection: AcpSelection,
    pub(super) attrs: Vec<AttrRun>,
    /// The host has not yet been told the current string, caret and attributes.
    pub(super) dirty: bool,
}

/// Where a TSF range lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Target {
    /// Entirely in the host text: UTF-8 byte range of the host text.
    Host(Range<usize>),
    /// Entirely in the composition string: UTF-16 range inside it.
    Composition(Range<usize>),
    /// Across a composition boundary.
    Straddles,
}

/// One piece of a range for layout queries, in the offsets the host speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Part {
    /// UTF-8 byte range of the host text.
    Host(Range<usize>),
    /// UTF-8 byte range inside the composition string.
    Composition(Range<usize>),
}

fn is_high(unit: u16) -> bool {
    (0xD800..0xDC00).contains(&unit)
}

fn is_low(unit: u16) -> bool {
    (0xDC00..0xE000).contains(&unit)
}

/// Approximate size of the blocks of the byte / UTF-16 conversion table.
const BLOCK_BYTES: usize = 4096;

/// A run of whole scalars of the host text and its length in both encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Block {
    bytes: usize,
    units: usize,
}

/// Splits `text` into blocks of about [`BLOCK_BYTES`] bytes.
fn chunk(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut current = Block { bytes: 0, units: 0 };
    for scalar in text.chars() {
        current.bytes += scalar.len_utf8();
        current.units += scalar.len_utf16();
        if current.bytes >= BLOCK_BYTES {
            blocks.push(std::mem::replace(
                &mut current,
                Block { bytes: 0, units: 0 },
            ));
        }
    }
    if current.bytes > 0 {
        blocks.push(current);
    }
    blocks
}

/// `(units, bytes)` of the scalar starting at `units[at]`.
fn scalar_at(units: &[u16], at: usize) -> (usize, usize) {
    let unit = units[at];
    if is_high(unit) && units.get(at + 1).is_some_and(|next| is_low(*next)) {
        (2, 4)
    } else if unit < 0x80 {
        (1, 1)
    } else if unit < 0x800 {
        (1, 2)
    } else {
        (1, 3)
    }
}

/// The TSF-visible text; see the module documentation.
///
/// The host text is kept as UTF-16 (`units`) with a table of blocks that converts between byte
/// and UTF-16 offsets in `O(blocks + block size)`. Host edits are applied to both in
/// `O(edit + blocks)` ([`View::apply_host_edit`]); only the first access and an unknown change
/// rebuild everything. The composition string is not part of `units`: it is overlaid on access.
#[derive(Debug)]
pub(super) struct View {
    blocks: Vec<Block>,
    byte_len: usize,
    units: Vec<u16>,
    valid: bool,
    comp: Option<Composition>,
}

impl View {
    /// An empty, invalid view: the first access rebuilds it from the host.
    pub(super) fn new() -> Self {
        Self {
            blocks: Vec::new(),
            byte_len: 0,
            units: Vec::new(),
            valid: false,
            comp: None,
        }
    }

    pub(super) fn is_valid(&self) -> bool {
        self.valid
    }

    /// Rebuilds the cache from the host text.
    pub(super) fn rebuild(&mut self, host_text: &str) {
        self.blocks = chunk(host_text);
        self.byte_len = host_text.len();
        self.units = host_text.encode_utf16().collect();
        self.valid = true;
    }

    /// UTF-16 offset of the host byte offset `byte`; inside a scalar it floors to the scalar.
    fn byte_to_host_units(&self, byte: usize) -> usize {
        let (mut bytes, mut units) = (0usize, 0usize);
        for block in &self.blocks {
            if bytes + block.bytes > byte {
                break;
            }
            bytes += block.bytes;
            units += block.units;
        }
        while bytes < byte && units < self.units.len() {
            let (scalar_units, scalar_bytes) = scalar_at(&self.units, units);
            if bytes + scalar_bytes > byte {
                break;
            }
            bytes += scalar_bytes;
            units += scalar_units;
        }
        units
    }

    /// Host byte offset of the UTF-16 offset `at`; inside a pair it floors to the pair.
    fn host_units_to_byte(&self, at: usize) -> usize {
        let (mut bytes, mut units) = (0usize, 0usize);
        for block in &self.blocks {
            if units + block.units > at {
                break;
            }
            bytes += block.bytes;
            units += block.units;
        }
        while units < at && units < self.units.len() {
            let (scalar_units, scalar_bytes) = scalar_at(&self.units, units);
            if units + scalar_units > at {
                break;
            }
            bytes += scalar_bytes;
            units += scalar_units;
        }
        bytes
    }

    /// The host replaced `removed_len` bytes at `start` by `inserted`. Updates the cache in
    /// `O(edit + blocks)` and keeps the composition anchored to the same host text.
    pub(super) fn apply_host_edit(&mut self, start: usize, removed_len: usize, inserted: &str) {
        if let Some(comp) = &mut self.comp
            && start < comp.host_at
        {
            comp.host_at = if start + removed_len <= comp.host_at {
                comp.host_at - removed_len + inserted.len()
            } else {
                start + inserted.len()
            };
        }
        if !self.valid {
            return;
        }
        let end = start + removed_len;
        let (from, to) = (self.byte_to_host_units(start), self.byte_to_host_units(end));
        // The blocks to re-chunk: those the edit touches plus the next one, so small
        // remainders merge instead of accumulating.
        let count = self.blocks.len();
        let mut first = None;
        let mut last = count.saturating_sub(1);
        let (mut bytes, mut units) = (0usize, 0usize);
        let mut first_units = 0usize;
        for (index, block) in self.blocks.iter().enumerate() {
            if first.is_none() && start < bytes + block.bytes {
                first = Some(index);
                first_units = units;
            }
            if end < bytes + block.bytes {
                last = index;
                break;
            }
            bytes += block.bytes;
            units += block.units;
        }
        let first = match first {
            Some(index) => index,
            None => {
                // The edit starts at the end of the text: extend the last block.
                let index = count.saturating_sub(1);
                if let Some(block) = self.blocks.get(index) {
                    first_units = self.units.len() - block.units;
                }
                index
            }
        };
        let last = (last + 1).min(count.saturating_sub(1)).max(first);
        let old_units: usize = self
            .blocks
            .get(first..=last)
            .map_or(0, |blocks| blocks.iter().map(|block| block.units).sum());
        let new_units: Vec<u16> = inserted.encode_utf16().collect();
        let new_len = old_units - (to - from) + new_units.len();
        self.units.splice(from..to, new_units);
        let region = String::from_utf16_lossy(&self.units[first_units..first_units + new_len]);
        let replacement = chunk(&region);
        if self.blocks.is_empty() {
            self.blocks = replacement;
        } else {
            self.blocks.splice(first..=last, replacement);
        }
        self.byte_len = self.byte_len - removed_len + inserted.len();
    }

    /// Length of the TSF-visible text in UTF-16 units.
    pub(super) fn len(&self) -> usize {
        self.units.len() + self.comp.as_ref().map_or(0, |comp| comp.units.len())
    }

    /// The TSF-visible unit at `position`.
    fn at(&self, position: usize) -> u16 {
        match (&self.comp, self.composition_start()) {
            (Some(comp), Some(start)) if position >= start => {
                if position < start + comp.units.len() {
                    comp.units[position - start]
                } else {
                    self.units[position - comp.units.len()]
                }
            }
            _ => self.units[position],
        }
    }

    /// The TSF-visible units in `range`.
    pub(super) fn slice(&self, range: Range<usize>) -> Vec<u16> {
        let (Some(comp), Some(start)) = (&self.comp, self.composition_start()) else {
            return self.units[range].to_vec();
        };
        let (comp_len, end) = (comp.units.len(), start + comp.units.len());
        let mut out = Vec::with_capacity(range.len());
        if range.start < start {
            out.extend_from_slice(&self.units[range.start..range.end.min(start)]);
        }
        let (from, to) = (range.start.max(start), range.end.min(end));
        if from < to {
            out.extend_from_slice(&comp.units[from - start..to - start]);
        }
        let from = range.start.max(end);
        if from < range.end {
            out.extend_from_slice(&self.units[from - comp_len..range.end - comp_len]);
        }
        out
    }

    pub(super) fn composition(&self) -> Option<&Composition> {
        self.comp.as_ref()
    }

    pub(super) fn composition_mut(&mut self) -> Option<&mut Composition> {
        self.comp.as_mut()
    }

    /// True when `position` does not fall between the two units of a surrogate pair.
    pub(super) fn is_boundary(&self, position: usize) -> bool {
        position == 0
            || position >= self.len()
            || !(is_low(self.at(position)) && is_high(self.at(position - 1)))
    }

    /// `position` moved onto a scalar boundary: forward to the end of a pair, else to its start.
    pub(super) fn snap(&self, position: usize, forward: bool) -> usize {
        let position = position.min(self.len());
        if self.is_boundary(position) {
            position
        } else if forward {
            position + 1
        } else {
            position - 1
        }
    }

    /// The range `start..end` moved onto scalar boundaries: outwards, except that an empty range
    /// (an insertion point inside a pair) stays empty at the start of the pair.
    pub(super) fn snap_range(&self, start: usize, end: usize) -> (usize, usize) {
        let snapped = self.snap(start, false);
        if start == end {
            (snapped, snapped)
        } else {
            (snapped, self.snap(end, true))
        }
    }

    /// ACP where the composition `comp` of this view starts.
    fn start_of(&self, comp: &Composition) -> usize {
        self.byte_to_host_units(comp.host_at)
    }

    /// ACP where the composition string starts, if composing.
    pub(super) fn composition_start(&self) -> Option<usize> {
        self.comp.as_ref().map(|comp| self.start_of(comp))
    }

    /// ACP range of the composition string, if composing.
    pub(super) fn composition_extent(&self) -> Option<Range<usize>> {
        let comp = self.comp.as_ref()?;
        let start = self.start_of(comp);
        Some(start..start + comp.units.len())
    }

    /// ACP of the host byte offset `byte`. A position at the composition's insertion point maps
    /// to the start of the composition, or to its end when `after_composition`.
    pub(super) fn byte_to_acp(&self, byte: usize, after_composition: bool) -> usize {
        let units = self.byte_to_host_units(byte);
        match &self.comp {
            Some(comp) => {
                let start = self.byte_to_host_units(comp.host_at);
                if units > start || (units == start && after_composition) {
                    units + comp.units.len()
                } else {
                    units
                }
            }
            None => units,
        }
    }

    /// The selection TSF sees: the composition's own while composing, else the host's.
    pub(super) fn selection(&self, host: HostSelection) -> AcpSelection {
        if let Some(comp) = &self.comp {
            return comp.selection;
        }
        let (start, end) = (host.start(), host.end());
        AcpSelection {
            start: self.byte_to_host_units(start),
            end: self.byte_to_host_units(end),
            backward: host.is_reversed(),
        }
    }

    /// Host byte offset of the ACP `position` outside the composition.
    pub(super) fn acp_to_byte(&self, position: usize) -> usize {
        self.host_units_to_byte(position)
    }

    /// Classifies the (scalar-aligned) ACP range `start..end`.
    pub(super) fn classify(&self, start: usize, end: usize) -> Target {
        let Some(comp_start) = self.composition_start() else {
            return Target::Host(self.acp_to_byte(start)..self.acp_to_byte(end));
        };
        let comp_end = comp_start + self.comp.as_ref().map_or(0, |comp| comp.units.len());
        if start >= comp_start && end <= comp_end {
            Target::Composition(start - comp_start..end - comp_start)
        } else if end <= comp_start {
            Target::Host(self.acp_to_byte(start)..self.acp_to_byte(end))
        } else if start >= comp_end {
            let shift = comp_end - comp_start;
            Target::Host(self.acp_to_byte(start - shift)..self.acp_to_byte(end - shift))
        } else {
            Target::Straddles
        }
    }

    /// UTF-8 byte range of the UTF-16 `range` inside the composition string.
    fn composition_bytes(&self, range: Range<usize>) -> Range<usize> {
        let text = self
            .comp
            .as_ref()
            .map(|comp| String::from_utf16_lossy(&comp.units))
            .unwrap_or_default();
        let map = Utf16Map::new(&text);
        map.utf16_to_byte_floor(range.start)..map.utf16_to_byte_floor(range.end)
    }

    /// The pieces of the range `start..end` to ask the host for rectangles.
    pub(super) fn parts(&self, start: usize, end: usize) -> Vec<Part> {
        match self.classify(start, end) {
            Target::Host(bytes) => vec![Part::Host(bytes)],
            Target::Composition(range) => vec![Part::Composition(self.composition_bytes(range))],
            Target::Straddles => {
                let comp_start = self.composition_start().unwrap_or(0);
                let comp_len = self.comp.as_ref().map_or(0, |comp| comp.units.len());
                let comp_end = comp_start + comp_len;
                let mut parts = Vec::with_capacity(3);
                if start < comp_start {
                    parts.push(Part::Host(
                        self.acp_to_byte(start)..self.acp_to_byte(comp_start),
                    ));
                }
                parts.push(Part::Composition(self.composition_bytes(
                    start.max(comp_start) - comp_start..end.min(comp_end) - comp_start,
                )));
                if end > comp_end {
                    parts.push(Part::Host(
                        self.acp_to_byte(start.max(comp_end) - comp_len)
                            ..self.acp_to_byte(end - comp_len),
                    ));
                }
                parts
            }
        }
    }

    /// Starts a composition over the ACP range `start..end` (scalar-aligned, in the host text)
    /// while TSF's selection is `selection`. Returns the host byte range the composition
    /// replaces. The host text still contains that range: the caller tells the host to remove
    /// it and then calls [`View::apply_host_edit`].
    pub(super) fn begin_composition(
        &mut self,
        start: usize,
        end: usize,
        selection: AcpSelection,
    ) -> Range<usize> {
        let bytes = self.acp_to_byte(start)..self.acp_to_byte(end);
        self.comp = Some(Composition {
            host_at: bytes.start,
            units: self.units[start..end].to_vec(),
            selection,
            attrs: Vec::new(),
            dirty: false,
        });
        bytes
    }

    /// Ends the composition. The caller commits its string to the host and then calls
    /// [`View::apply_host_edit`] for it.
    pub(super) fn take_composition(&mut self) -> Option<Composition> {
        self.comp.take()
    }

    /// Replaces `range` (inside the composition string) by `text`. Returns the change in ACPs.
    ///
    /// # Panics
    /// When not composing or `range` is outside the composition string.
    pub(super) fn edit_composition(&mut self, range: Range<usize>, text: &[u16]) -> TextChange {
        // INVARIANT: callers obtain `range` from `classify` returning `Target::Composition`,
        // which exists only while composing.
        let start = self
            .composition_start()
            .expect("edit_composition requires an active composition");
        let change = TextChange {
            start: start + range.start,
            old_end: start + range.end,
            new_end: start + range.start + text.len(),
        };
        let comp = self
            .comp
            .as_mut()
            .expect("edit_composition requires an active composition");
        comp.units.splice(range, text.iter().copied());
        comp.selection = AcpSelection {
            start: change.adjust(comp.selection.start, false),
            end: change.adjust(comp.selection.end, true),
            backward: comp.selection.backward,
        };
        comp.dirty = true;
        change
    }

    /// A host edit changed the text around the composition: moves TSF's selection through it.
    pub(super) fn adjust_selection(&mut self, change: TextChange) {
        if let Some(comp) = self.comp.as_mut() {
            comp.selection = AcpSelection {
                start: change.adjust(comp.selection.start, false),
                end: change.adjust(comp.selection.end, true),
                backward: comp.selection.backward,
            };
        }
    }

    /// Stores new display attributes of the composition (ACP runs); marks it dirty on change.
    pub(super) fn set_attrs(&mut self, runs: &[(Range<usize>, ImeAttrStyle)]) {
        let start = self.composition_start();
        let Some((start, comp)) = start.zip(self.comp.as_mut()) else {
            return;
        };
        let len = comp.units.len();
        let attrs: Vec<AttrRun> = runs
            .iter()
            .filter_map(|(range, style)| {
                let from = range.start.max(start) - start;
                let to = range.end.min(start + len).saturating_sub(start);
                (from < to).then_some(AttrRun {
                    range: from..to,
                    style: *style,
                })
            })
            .collect();
        if attrs != comp.attrs {
            comp.attrs = attrs;
            comp.dirty = true;
        }
    }

    /// The `ImeEvent::Update` for the current composition string, caret and attributes.
    pub(super) fn update_event(&self) -> Option<ImeEvent> {
        let comp = self.comp.as_ref()?;
        let start = self.start_of(comp);
        let text = String::from_utf16_lossy(&comp.units);
        let map = Utf16Map::new(&text);
        let caret = comp.selection.head().clamp(start, start + comp.units.len()) - start;
        Some(ImeEvent::Update {
            caret: map.utf16_to_byte_floor(caret),
            attrs: ime_attrs(&comp.attrs, &map, text.len()),
            text,
        })
    }

    /// Takes the pending `ImeEvent::Update`, if the composition changed since the last one.
    pub(super) fn take_update(&mut self) -> Option<ImeEvent> {
        let comp = self.comp.as_mut()?;
        if !comp.dirty {
            return None;
        }
        comp.dirty = false;
        self.update_event()
    }

    /// The composition string, caret or attributes changed: the host hears of it with the next
    /// update. Without a composition there is nothing to tell.
    pub(super) fn mark_composition_dirty(&mut self) {
        if let Some(comp) = self.comp.as_mut() {
            comp.dirty = true;
        }
    }

    /// The composition was ended and started again over a moved extent: the new one inherits
    /// `selection`, TSF's selection of the old one, and the host is told about it. Without a
    /// new composition (the host refused it) or without the old selection nothing changes.
    pub(super) fn resume_composition(&mut self, selection: Option<AcpSelection>) {
        if let (Some(comp), Some(selection)) = (self.comp.as_mut(), selection) {
            comp.selection = selection;
            comp.dirty = true;
        }
    }
}

/// Display attributes as the host wants them: UTF-8 ranges of the composition string covering all
/// of it, `Input` where the IME gave no attribute.
fn ime_attrs(runs: &[AttrRun], map: &Utf16Map, text_len: usize) -> Vec<ImeAttr> {
    let mut sorted: Vec<&AttrRun> = runs.iter().collect();
    sorted.sort_by_key(|run| run.range.start);
    let mut out = Vec::with_capacity(sorted.len() + 1);
    let mut covered = 0usize;
    for run in sorted {
        let from = map.utf16_to_byte_floor(run.range.start).max(covered);
        let to = map.utf16_to_byte_floor(run.range.end);
        if to <= from {
            continue;
        }
        if from > covered {
            out.push(ImeAttr {
                range: covered..from,
                style: ImeAttrStyle::Input,
            });
        }
        out.push(ImeAttr {
            range: from..to,
            style: run.style,
        });
        covered = to;
    }
    if covered < text_len {
        out.push(ImeAttr {
            range: covered..text_len,
            style: ImeAttrStyle::Input,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(text: &str) -> View {
        let mut view = View::new();
        view.rebuild(text);
        view
    }

    /// A composition over `start..end` with TSF's selection on the same range.
    fn begin(view: &mut View, start: usize, end: usize) -> Range<usize> {
        view.begin_composition(
            start,
            end,
            AcpSelection {
                start,
                end,
                backward: false,
            },
        )
    }

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn new_view_is_invalid_until_rebuilt_and_rebuild_maps_utf16() {
        let mut view = View::new();
        assert!(!view.is_valid());
        view.rebuild("a😀é");
        assert!(view.is_valid());
        assert_eq!(view.len(), 4);
        assert_eq!(view.slice(1..3), units("😀"));
        assert_eq!(view.byte_to_acp(5, false), 3);
        assert_eq!(view.acp_to_byte(3), 5);
    }

    #[test]
    fn surrogate_pairs_are_never_split_by_snap() {
        let view = view("a😀b");
        assert!(view.is_boundary(0) && view.is_boundary(1) && view.is_boundary(3));
        assert!(!view.is_boundary(2));
        assert_eq!(view.snap(2, false), 1);
        assert_eq!(view.snap(2, true), 3);
        assert_eq!(view.snap(99, false), 4);
    }

    #[test]
    fn host_selection_converts_with_direction() {
        let view = view("a😀b");
        let forward = view.selection(HostSelection::new(1, 5));
        assert_eq!(
            forward,
            AcpSelection {
                start: 1,
                end: 3,
                backward: false
            }
        );
        let backward = view.selection(HostSelection::new(5, 1));
        assert_eq!(
            (backward.start, backward.end, backward.backward),
            (1, 3, true)
        );
        assert_eq!(backward.head(), 1);
        assert_eq!(AcpSelection::caret(4).head(), 4);
    }

    #[test]
    fn classify_without_composition_converts_to_bytes() {
        let view = view("a😀b");
        assert_eq!(view.classify(1, 3), Target::Host(1..5));
        assert_eq!(view.parts(1, 3), vec![Part::Host(1..5)]);
        assert_eq!(view.composition_extent(), None);
        assert_eq!(view.composition_start(), None);
    }

    #[test]
    fn composition_is_spliced_into_the_visible_text() {
        let mut view = view("abcd");
        let bytes = begin(&mut view, 2, 2);
        assert_eq!(bytes, 2..2);
        view.edit_composition(0..0, &units("あい"));
        assert_eq!(view.len(), 6);
        assert_eq!(view.slice(0..6), units("abあいcd"));
        // A range that ends with the composition takes nothing from the text after it.
        assert_eq!(view.slice(1..4), units("bあい"));
        assert_eq!(view.slice(3..5), units("いc"));
        assert_eq!(view.composition_extent(), Some(2..4));
        // The overlay survives a rebuild from unchanged host text.
        view.rebuild("abcd");
        assert_eq!(view.slice(0..6), units("abあいcd"));
        // Host positions: before the insertion point stay, after it shift by the overlay.
        assert_eq!(view.byte_to_acp(1, false), 1);
        assert_eq!(view.byte_to_acp(3, false), 5);
        assert_eq!(view.byte_to_acp(2, false), 2);
        assert_eq!(view.byte_to_acp(2, true), 4);
    }

    #[test]
    fn classify_splits_inside_before_after_and_straddling() {
        let mut view = view("abcd");
        begin(&mut view, 2, 2);
        view.edit_composition(0..0, &units("xyz"));
        // composition covers ACP 2..5
        assert_eq!(view.classify(2, 2), Target::Composition(0..0));
        assert_eq!(view.classify(3, 5), Target::Composition(1..3));
        assert_eq!(view.classify(0, 2), Target::Host(0..2));
        assert_eq!(view.classify(5, 6), Target::Host(2..3));
        assert_eq!(view.classify(1, 4), Target::Straddles);
        assert_eq!(view.classify(3, 7), Target::Straddles);
        assert_eq!(
            view.parts(1, 7),
            vec![Part::Host(1..2), Part::Composition(0..3), Part::Host(2..4)]
        );
        assert_eq!(
            view.parts(1, 4),
            vec![Part::Host(1..2), Part::Composition(0..2)]
        );
        assert_eq!(
            view.parts(3, 7),
            vec![Part::Composition(1..3), Part::Host(2..4)]
        );
    }

    #[test]
    fn editing_the_composition_moves_its_selection_through_the_change() {
        let mut view = view("ab");
        begin(&mut view, 1, 1);
        view.edit_composition(0..0, &units("kk"));
        {
            let comp = view.composition_mut().expect("composing");
            comp.selection = AcpSelection::caret(3);
        }
        let change = view.edit_composition(0..2, &units("か"));
        assert_eq!(
            change,
            TextChange {
                start: 1,
                old_end: 3,
                new_end: 2
            }
        );
        let comp = view.composition().expect("composing");
        assert_eq!(comp.selection, AcpSelection::caret(2));
        assert_eq!(comp.units, units("か"));
        assert!(comp.dirty);
    }

    #[test]
    fn begin_composition_over_text_keeps_it_visible() {
        let mut view = view("a😀b");
        let bytes = begin(&mut view, 1, 3);
        assert_eq!(bytes, 1..5);
        let comp = view.composition().expect("composing");
        assert_eq!(comp.units, units("😀"));
        assert_eq!(
            comp.selection,
            AcpSelection {
                start: 1,
                end: 3,
                backward: false
            }
        );
        // The host removed the replaced text: the visible text is unchanged.
        view.apply_host_edit(1, 4, "");
        assert_eq!(view.slice(0..4), units("a😀b"));
        let taken = view.take_composition().expect("composing");
        assert_eq!(taken.host_at, 1);
        assert!(view.composition().is_none());
    }

    #[test]
    fn host_edits_before_the_composition_shift_its_anchor() {
        let mut view = view("abcd");
        begin(&mut view, 3, 3);
        view.apply_host_edit(0, 1, "xyz");
        assert_eq!(view.composition().expect("composing").host_at, 5);
        // after the anchor: unchanged
        view.apply_host_edit(6, 0, "##");
        assert_eq!(view.composition().expect("composing").host_at, 5);
        // across the anchor: collapses to the end of the new text
        view.apply_host_edit(2, 6, "!");
        assert_eq!(view.composition().expect("composing").host_at, 3);
        assert_eq!(view.slice(0..view.len()), units("xy!"));
        // An invalid view only moves the anchor; the next rebuild reads the host.
        let mut stale = View::new();
        stale.apply_host_edit(0, 0, "x");
        assert!(!stale.is_valid());
    }

    #[test]
    fn incremental_edits_keep_the_conversion_table_exact_across_blocks() {
        let mut text: String = (0..3000)
            .map(|i| match i % 7 {
                0 => 'é',
                3 => '😀',
                5 => '漢',
                _ => 'a',
            })
            .collect();
        let mut view = View::new();
        view.rebuild(&text);
        // (first char, chars removed, inserted): inserts, a cross-block replace, a big
        // deletion spanning many blocks, an edit at the very end and emptying the text.
        let edits: [(usize, usize, &str); 7] = [
            (0, 0, "x😀"),
            (1500, 40, ""),
            (2900, 100, "é漢"),
            (10, 2500, "Z"),
            (usize::MAX, 0, "tail😀"),
            (0, usize::MAX, ""),
            (0, 0, "ab"),
        ];
        for (first, count, inserted) in edits {
            let byte_of = |text: &str, index: usize| {
                text.char_indices()
                    .nth(index)
                    .map_or(text.len(), |(byte, _)| byte)
            };
            let start = byte_of(&text, first);
            let end = if count == usize::MAX {
                text.len()
            } else {
                byte_of(&text, first.saturating_add(count))
            };
            view.apply_host_edit(start, end - start, inserted);
            text.replace_range(start..end, inserted);
            let map = Utf16Map::new(&text);
            assert_eq!(view.len(), map.utf16_len());
            for (byte, _) in text.char_indices().step_by(13) {
                assert_eq!(view.byte_to_acp(byte, false), map.byte_to_utf16_floor(byte));
            }
            for unit in (0..=map.utf16_len()).step_by(13) {
                assert_eq!(view.acp_to_byte(unit), map.utf16_to_byte_floor(unit));
            }
            assert_eq!(view.slice(0..view.len()), units(&text));
        }
    }

    #[test]
    fn text_changes_adjust_and_merge() {
        let insert = TextChange {
            start: 2,
            old_end: 2,
            new_end: 4,
        };
        assert_eq!(
            insert.adjust(2, true),
            2,
            "an anchor at a pure insertion stays"
        );
        assert_eq!(insert.adjust(3, true), 5);
        assert_eq!(insert.adjust(1, true), 1);
        let replace = TextChange {
            start: 2,
            old_end: 6,
            new_end: 3,
        };
        assert_eq!(replace.adjust(4, true), 3);
        assert_eq!(replace.adjust(4, false), 2);
        assert_eq!(replace.adjust(2, true), 2);
        assert_eq!(replace.adjust(6, true), 3);
        assert_eq!(replace.adjust(9, true), 6);

        let first = TextChange {
            start: 0,
            old_end: 0,
            new_end: 1,
        };
        let second = TextChange {
            start: 4,
            old_end: 5,
            new_end: 6,
        };
        assert_eq!(
            merge_changes(first, second),
            TextChange {
                start: 0,
                old_end: 4,
                new_end: 6
            }
        );
        let first = TextChange {
            start: 2,
            old_end: 4,
            new_end: 6,
        };
        let second = TextChange {
            start: 3,
            old_end: 4,
            new_end: 5,
        };
        assert_eq!(
            merge_changes(first, second),
            TextChange {
                start: 2,
                old_end: 4,
                new_end: 7
            }
        );
    }

    #[test]
    fn update_events_carry_text_utf8_caret_and_gap_filled_attributes() {
        let mut view = view("xy");
        begin(&mut view, 1, 1);
        view.edit_composition(0..0, &units("かな漢"));
        view.composition_mut().expect("composing").selection = AcpSelection::caret(3);
        view.set_attrs(&[
            (2..3, ImeAttrStyle::TargetConverted),
            (1..2, ImeAttrStyle::Converted),
            (9..12, ImeAttrStyle::InputError),
        ]);
        // ACP 3 is composition offset 2 = 6 bytes.
        assert_eq!(
            view.take_update(),
            Some(ImeEvent::Update {
                text: "かな漢".to_owned(),
                caret: 6,
                attrs: vec![
                    ImeAttr {
                        range: 0..3,
                        style: ImeAttrStyle::Converted
                    },
                    ImeAttr {
                        range: 3..6,
                        style: ImeAttrStyle::TargetConverted
                    },
                    ImeAttr {
                        range: 6..9,
                        style: ImeAttrStyle::Input
                    },
                ]
            })
        );
        assert!(view.take_update().is_none(), "an update is delivered once");
        // Unchanged attributes do not make the composition dirty again.
        view.set_attrs(&[
            (2..3, ImeAttrStyle::TargetConverted),
            (1..2, ImeAttrStyle::Converted),
        ]);
        assert!(view.take_update().is_none());
        // No attributes at all: the whole string is Input.
        view.set_attrs(&[]);
        assert_eq!(
            view.take_update(),
            Some(ImeEvent::Update {
                text: "かな漢".to_owned(),
                caret: 6,
                attrs: vec![ImeAttr {
                    range: 0..9,
                    style: ImeAttrStyle::Input
                }]
            }),
            "removing the attributes changes the composition"
        );
    }

    #[test]
    fn attribute_runs_overlapping_each_other_are_clipped_to_the_uncovered_part() {
        let map = Utf16Map::new("abcdef");
        let runs = [
            AttrRun {
                range: 0..4,
                style: ImeAttrStyle::Converted,
            },
            AttrRun {
                range: 2..6,
                style: ImeAttrStyle::TargetConverted,
            },
            AttrRun {
                range: 5..5,
                style: ImeAttrStyle::InputError,
            },
        ];
        assert_eq!(
            ime_attrs(&runs, &map, 6),
            vec![
                ImeAttr {
                    range: 0..4,
                    style: ImeAttrStyle::Converted
                },
                ImeAttr {
                    range: 4..6,
                    style: ImeAttrStyle::TargetConverted
                },
            ]
        );
        assert!(ime_attrs(&[], &map, 0).is_empty());
    }

    #[test]
    fn attribute_gaps_before_and_between_runs_are_input() {
        let map = Utf16Map::new("abcdef");
        let runs = [
            AttrRun {
                range: 1..2,
                style: ImeAttrStyle::Converted,
            },
            AttrRun {
                range: 4..5,
                style: ImeAttrStyle::TargetConverted,
            },
        ];
        assert_eq!(
            ime_attrs(&runs, &map, 6),
            vec![
                ImeAttr {
                    range: 0..1,
                    style: ImeAttrStyle::Input
                },
                ImeAttr {
                    range: 1..2,
                    style: ImeAttrStyle::Converted
                },
                ImeAttr {
                    range: 2..4,
                    style: ImeAttrStyle::Input
                },
                ImeAttr {
                    range: 4..5,
                    style: ImeAttrStyle::TargetConverted
                },
                ImeAttr {
                    range: 5..6,
                    style: ImeAttrStyle::Input
                },
            ]
        );
    }

    #[test]
    fn host_bytes_inside_a_scalar_floor_to_that_scalar() {
        // 'a' is byte 0, the emoji bytes 1..5 (ACPs 1..3), 'b' byte 5.
        let view = view("a😀b");
        assert_eq!(view.byte_to_acp(2, false), 1);
        assert_eq!(view.byte_to_acp(4, false), 1);
        assert_eq!(view.byte_to_acp(5, false), 3);
    }

    #[test]
    fn composition_parts_are_utf8_ranges_of_the_composition_string() {
        let mut view = view("ab");
        begin(&mut view, 1, 1);
        view.edit_composition(0..0, &units("かな"));
        // The visible text is "aかなb": the composition occupies ACPs 1..3.
        assert_eq!(view.parts(1, 3), vec![Part::Composition(0..6)]);
        assert_eq!(view.parts(2, 3), vec![Part::Composition(3..6)]);
        assert_eq!(
            view.parts(0, 4),
            vec![Part::Host(0..1), Part::Composition(0..6), Part::Host(1..2)]
        );
    }

    #[test]
    fn empty_ranges_inside_a_pair_stay_empty_and_others_snap_outwards() {
        let view = view("a😀b");
        assert_eq!(view.snap_range(2, 2), (1, 1));
        assert_eq!(view.snap_range(1, 1), (1, 1));
        assert_eq!(view.snap_range(2, 3), (1, 3));
        assert_eq!(view.snap_range(0, 2), (0, 3));
        assert_eq!(view.snap_range(1, 99), (1, 4));
    }

    #[test]
    fn host_edits_move_the_selection_of_the_composition() {
        let mut view = view("abcd");
        view.adjust_selection(TextChange {
            start: 0,
            old_end: 0,
            new_end: 2,
        });
        begin(&mut view, 2, 2);
        view.composition_mut().expect("composing").selection = AcpSelection::caret(2);
        view.adjust_selection(TextChange {
            start: 0,
            old_end: 0,
            new_end: 2,
        });
        assert_eq!(
            view.composition().expect("composing").selection,
            AcpSelection::caret(4)
        );
    }

    #[test]
    fn updates_need_a_composition_and_attrs_are_ignored_without_one() {
        let mut view = view("ab");
        assert!(view.update_event().is_none());
        assert!(view.take_update().is_none());
        view.set_attrs(&[(0..1, ImeAttrStyle::Converted)]);
        assert!(view.composition().is_none());
    }

    #[test]
    fn marking_a_composition_dirty_schedules_one_update_and_needs_a_composition() {
        let mut view = view("ab");
        // Nothing to mark: no composition appears and no update is pending.
        view.mark_composition_dirty();
        assert!(view.composition().is_none());
        assert!(view.take_update().is_none());
        begin(&mut view, 1, 1);
        assert!(view.take_update().is_none(), "a fresh composition is clean");
        view.mark_composition_dirty();
        assert!(view.composition().expect("composing").dirty);
        assert!(view.take_update().is_some());
        assert!(view.take_update().is_none(), "the update is delivered once");
    }

    #[test]
    fn a_restarted_composition_inherits_the_selection_only_when_there_is_one_to_inherit() {
        let mut view = view("abcd");
        let old = AcpSelection {
            start: 3,
            end: 4,
            backward: true,
        };
        // The host refused the new composition: nothing to give the selection to.
        view.resume_composition(Some(old));
        assert!(view.composition().is_none());
        begin(&mut view, 1, 2);
        // The old composition had no selection to hand over.
        view.resume_composition(None);
        let comp = view.composition().expect("composing");
        assert_eq!(
            comp.selection,
            AcpSelection {
                start: 1,
                end: 2,
                backward: false
            }
        );
        assert!(!comp.dirty);
        // With one, the new composition takes it over and the host is told.
        view.resume_composition(Some(old));
        let comp = view.composition().expect("composing");
        assert_eq!(comp.selection, old);
        assert!(comp.dirty);
    }
}
