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
