//! What a probe costs, measured rather than asserted.
//!
//! A development aid, and a correction to a measurement that was wrong: the first version
//! of this example started its clock *after* `std::fs::read`, so the whole-file path was
//! timed without the read that is its whole cost — 10 MB from the page cache inside a
//! timing loop that claimed to measure it. Both paths are now timed from open to probe,
//! and both report the bytes they actually touched, which is the number that does not
//! depend on what the page cache happens to hold.
//!
//! The two paths are asserted to agree before anything is printed, so a fast wrong answer
//! cannot masquerade as a win.
//!
//! ```text
//! cargo run --release --example probe_cost -- <file.mp3> [more files...]
//! ```

use std::time::Instant;

/// A reader that counts what it was asked for.
struct Counting {
    inner: audiobook_shelf::fs_reader::FileReader,
    requested: usize,
}

impl mp4_core::ByteReader for Counting {
    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Option<usize> {
        self.requested += buf.len();
        self.inner.read_at(offset, buf)
    }
}

fn main() {
    let paths: Vec<_> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: probe_cost <file> [files...]");
        std::process::exit(2);
    }
    println!(
        "{:<40} {:>10} {:>9} {:>11} {:>9} {:>11} {:>8}",
        "file", "bytes", "whole", "whole read", "window", "window read", "read ratio"
    );
    for path in &paths {
        let path = std::path::Path::new(path);

        let start = Instant::now();
        let whole = std::fs::read(path)
            .map_err(|e| {
                eprintln!("{}: {e}", path.display());
                std::process::exit(1);
            })
            .map(|b| {
                let probe = audiobook_core::MediaProbe::probe(&b);
                (probe, b.len())
            });
        let (whole_probe, whole_bytes) = match whole {
            Ok(v) => v,
            Err(_) => continue,
        };
        let whole_time = start.elapsed();

        let mut counting = Counting {
            inner: match audiobook_shelf::fs_reader::FileReader::open(path) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("{}: {e}", path.display());
                    continue;
                }
            },
            requested: 0,
        };
        let start = Instant::now();
        let windowed = audiobook_core::MediaProbe::probe_source(&mut counting);
        let windowed_time = start.elapsed();

        assert_eq!(whole_probe.duration_ms, windowed.duration_ms, "duration");
        assert_eq!(whole_probe.chapters, windowed.chapters, "chapters");
        assert_eq!(whole_probe.title, windowed.title, "title");

        println!(
            "{:<40} {:>10} {:>8.1?} {:>9} {:>8.1?} {:>9} {:>7.0}x",
            path.display(),
            whole_bytes,
            whole_time,
            whole_bytes,
            windowed_time,
            counting.requested,
            whole_bytes as f64 / counting.requested.max(1) as f64
        );
    }
}
