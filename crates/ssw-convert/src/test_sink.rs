//! A bounded sink shared by the tests of the archive writers.

use std::io::{Cursor, Seek, SeekFrom, Write};

/// A sink that takes `budget` bytes in total and refuses every write beyond that.
#[derive(Debug)]
pub(crate) struct Limited {
    pub(crate) inner: Cursor<Vec<u8>>,
    pub(crate) budget: u64,
}

impl Write for Limited {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.inner.position() + buf.len() as u64 > self.budget {
            return Err(std::io::Error::other("sink is full"));
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Limited {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// `len` deterministic bytes that deflate cannot shrink: a compressing writer hands output to
/// its sink while the data is still being written, not only when the entry is closed.
pub(crate) fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limited_sink_takes_its_budget_in_total_and_refuses_the_rest() {
        let mut sink = Limited {
            inner: Cursor::new(Vec::new()),
            budget: 5,
        };
        sink.write_all(b"abc").unwrap();
        sink.flush().unwrap();
        assert_eq!(sink.write(b"def").unwrap_err().to_string(), "sink is full");
        sink.write_all(b"de").unwrap();
        assert_eq!(sink.write(b"f").unwrap_err().to_string(), "sink is full");
        // the budget is checked against the position, so overwriting stays within it
        assert_eq!(sink.seek(SeekFrom::Start(1)).unwrap(), 1);
        sink.write_all(b"X").unwrap();
        assert_eq!(sink.inner.into_inner(), b"aXcde");
    }
}
