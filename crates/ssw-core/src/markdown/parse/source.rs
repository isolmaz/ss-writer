//! A paragraph-like text assembled from several source line segments, with a byte-exact map
//! back to document offsets.

use crate::text::TextRange;

/// Concatenation of line segments joined by `\n`, plus the document offset of every byte.
#[derive(Debug)]
pub(super) struct Source {
    /// Assembled text.
    pub buf: String,
    /// `map[i]` is the document offset of `buf[i]`; the separator `\n` maps to the end of the
    /// line content it terminates. `map[buf.len()]` is the end of the last segment.
    map: Vec<usize>,
    /// Offset in `buf` where each segment starts.
    pub line_starts: Vec<usize>,
    /// Length in bytes of the line terminator after each segment (0 for the last line when the
    /// document does not end with a newline).
    term: Vec<usize>,
}

impl Source {
    /// Builds the buffer from `lines` (each a document range without line terminator).
    pub(super) fn from_lines(src: &str, lines: &[TextRange]) -> Source {
        let mut buf = String::new();
        let mut map = Vec::new();
        let mut line_starts = Vec::with_capacity(lines.len());
        let mut term = Vec::with_capacity(lines.len());
        for (n, line) in lines.iter().enumerate() {
            if n > 0 {
                let prev: &TextRange = &lines[n - 1];
                buf.push('\n');
                map.push(prev.end);
            }
            line_starts.push(buf.len());
            let text = line.get(src).unwrap_or("");
            buf.push_str(text);
            map.extend(line.start..line.start + text.len());
            let rest = src.as_bytes().get(line.end..).unwrap_or(&[]);
            term.push(if rest.starts_with(b"\r\n") {
                2
            } else {
                usize::from(rest.starts_with(b"\n"))
            });
        }
        map.push(lines.last().map_or(0, |l| l.end));
        Source {
            buf,
            map,
            line_starts,
            term,
        }
    }

    /// Document offset of the byte at `p` (start-type position).
    pub(super) fn start(&self, p: usize) -> usize {
        self.map[p.min(self.map.len() - 1)]
    }

    /// Document offset just after the byte before `p` (end-type position). A separator
    /// newline ends after the real line terminator.
    pub(super) fn end(&self, p: usize) -> usize {
        if p == 0 {
            return self.map[0];
        }
        let q = p - 1;
        if self.buf.as_bytes().get(q) == Some(&b'\n') {
            let line = self
                .line_starts
                .partition_point(|&s| s <= q)
                .saturating_sub(1);
            self.map[q] + self.term.get(line).copied().unwrap_or(1)
        } else {
            self.map[q] + 1
        }
    }

    /// Document range of `buf[a..b]`.
    pub(super) fn range(&self, a: usize, b: usize) -> TextRange {
        if a >= b {
            return TextRange::at(self.start(a));
        }
        TextRange::new(self.start(a), self.end(b))
    }
}
