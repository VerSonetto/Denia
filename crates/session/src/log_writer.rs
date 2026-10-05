//! Buffered log writes stop permanently on an I/O error. A failed buffer is
//! discarded rather than retried by `BufWriter::drop`; loading repairs the tail.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;

pub(crate) struct LogWriter(Option<BufWriter<File>>);

impl LogWriter {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self::new(
            OpenOptions::new().append(true).create(true).open(path)?,
        ))
    }

    pub fn new(file: File) -> Self {
        Self(Some(BufWriter::with_capacity(16 * 1024, file)))
    }

    pub fn buffered_len(&self) -> usize {
        self.0.as_ref().map_or(0, |writer| writer.buffer().len())
    }

    fn write_with<T>(
        &mut self,
        operation: impl FnOnce(&mut BufWriter<File>) -> io::Result<T>,
    ) -> io::Result<T> {
        let result = operation(self.0.as_mut().ok_or_else(|| {
            io::Error::other("session log writer failed; reload the session before writing")
        })?);
        if result.is_err()
            && let Some(writer) = self.0.take()
        {
            let (_file, _discarded_buffer) = writer.into_parts();
        }
        result
    }
}

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_with(|writer| writer.write(bytes))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.write_with(Write::flush)
    }
}
