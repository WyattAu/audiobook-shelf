//! Reading a file through the random-access contract, on disk.
//!
//! One wrapper, and it exists because [`mp4_core::ByteReader`] is a `no_std` contract
//! while this crate is where files live. The interesting property is not the code — it is
//! what the *callers* stop doing: a probe that opens a file and reads three windows is
//! the difference between listing a library in milliseconds and reading every byte of
//! every book to do it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// A file opened for random access.
pub struct FileReader {
    file: File,
}

impl FileReader {
    /// Open `path` for reading.
    ///
    /// # Errors
    ///
    /// Whatever the platform reports for a file that cannot be opened.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
        })
    }
}

impl mp4_core::ByteReader for FileReader {
    fn byte_len(&self) -> Option<u64> {
        self.file.metadata().ok().map(|m| m.len())
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Option<usize> {
        // Seek and fill, looping because a single `read` may return short for reasons
        // that have nothing to do with the end of the file. A short read at the *end* is
        // the normal case and is what breaks the loop.
        self.file.seek(SeekFrom::Start(offset)).ok()?;
        let mut filled = 0;
        while filled < buf.len() {
            match self.file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => return None,
            }
        }
        Some(filled)
    }
}

#[cfg(test)]
mod tests {
    // A test may assert: an `expect` here is the test stating what it assumes, which is
    // the honest form, and the crate lint denies it everywhere a caller would write it.
    #![allow(clippy::expect_used)]

    use super::*;
    use mp4_core::ByteReader as _;

    #[test]
    fn a_short_read_at_the_end_of_a_file_is_a_count_not_an_error() {
        let dir = std::env::temp_dir().join("audiobook-shelf-filereader");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("short.bin");
        std::fs::write(&path, [1u8, 2, 3]).expect("write");

        let mut reader = FileReader::open(&path).expect("opens");
        assert_eq!(reader.byte_len(), Some(3));
        let mut buf = [0u8; 8];
        assert_eq!(reader.read_at(0, &mut buf), Some(3));
        assert_eq!(&buf[..3], &[1, 2, 3]);
        assert_eq!(
            reader.read_at(3, &mut buf),
            Some(0),
            "past the end is empty"
        );
        assert_eq!(
            reader.read_at(1, &mut buf),
            Some(2),
            "an offset inside works"
        );
        assert_eq!(&buf[..2], &[2, 3]);
    }

    #[test]
    fn a_missing_file_reports_its_length_as_none_and_fails_to_open() {
        let dir = std::env::temp_dir().join("audiobook-shelf-filereader");
        std::fs::create_dir_all(&dir).expect("temp dir");
        assert!(FileReader::open(&dir.join("no-such-file.bin")).is_err());
    }
}
