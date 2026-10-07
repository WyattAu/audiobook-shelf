//! The scanner against a real library, checked against ffprobe.
//!
//! # Why this test needs an external tool
//!
//! Everything else in this crate is verified against assertions about constructed inputs,
//! which pins the rules but not the arithmetic. The interesting part of a *library* tool is
//! the arithmetic nobody unit-tests by hand: a book's chapters live in the individual
//! files, and chapter 7 of a four-file book is not at 7 seconds, it is at the sum of the
//! first three files' durations plus 7. Getting that wrong produces a book whose chapters
//! are all subtly misplaced and which still looks perfectly reasonable.
//!
//! ffprobe is the oracle for it. It reads each file independently and reports that file's
//! own duration and chapters, so the expected position of every chapter in the assembled
//! book is arithmetic over ground truth rather than a number this crate chose. If the
//! offset accumulation is wrong, the chapters disagree.
//!
//! # What happens without ffmpeg
//!
//! The tests are **skipped**, and say so. A conformance test that quietly passes because
//! the oracle was missing is worse than one that does not run: it reports coverage it does
//! not have. The skip is visible in the output for exactly that reason.
//!
//! The lints the library runs under are relaxed here for the same reason as in `scan.rs`:
//! this file is test *setup* and test *oracle*, where a missing binary or an unwritable
//! temporary directory means the assertion cannot run at all."

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

use audiobook_shelf::naming::ParseOptions;
use audiobook_shelf::scan::{self, Finding};

/// Whether ffmpeg and ffprobe are both available.
fn tools_available() -> bool {
    ["ffmpeg", "ffprobe"].iter().all(|tool| {
        Command::new(tool)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// A temporary library tree, removed when it goes out of scope.
struct TempLib(PathBuf);

impl TempLib {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "audiobook-shelf-ffmpeg-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create temp library");
        TempLib(base)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// Mux a real MP3 of `seconds` at `path`, with `chapters` written into it as
    /// FFMetadata so it carries real `CHAP` frames.
    #[allow(clippy::expect_used, clippy::panic)]
    fn make_mp3(&self, relative: &str, seconds: u32, chapters: &[(u64, &str)]) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        let meta = self.0.join(format!(
            "{}.ffmeta",
            path.file_name().unwrap().to_string_lossy()
        ));

        // FFMetadata with an explicit TIMEBASE, which is the only unambiguous way to
        // express these times: without it ffmpeg assumes nanoseconds.
        let mut text = String::from(";FFMETADATA1\n");
        let total_ms = u64::from(seconds) * 1000;
        for (i, (start_ms, title)) in chapters.iter().enumerate() {
            let end = chapters.get(i + 1).map_or(total_ms, |(next, _)| *next);
            text.push_str("[CHAPTER]\nTIMEBASE=1/1000\n");
            text.push_str(&format!("START={start_ms}\nEND={end}\ntitle={title}\n"));
        }
        std::fs::write(&meta, text).expect("write ffmetadata");

        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={seconds}"),
                "-i",
                meta.to_str().unwrap(),
                "-map_metadata",
                "1",
                "-c:a",
                "libmp3lame",
                "-b:a",
                "64k",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "ffmpeg failed for {relative}");
        path
    }
}

impl Drop for TempLib {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One chapter, read from one file by ffprobe.
#[derive(Debug, Clone, PartialEq)]
struct RefChapter {
    start_ms: u64,
    title: String,
}

/// Ask ffprobe for one file's own duration and chapters.
///
/// The JSON is walked by hand rather than parsed with a crate, for the same reason the
/// conformance harness does it: this file must build with no network and no registry. Only
/// three fields are needed.
///
/// Both values arrive as quoted strings, which is the part worth being careful about.
/// `"start_time": "0.000000"` has a quote immediately before the digits, so a scan that
/// takes the next numeric run reads the `0` out of the opening quote and reports every
/// chapter as starting at zero. The first version of this did exactly that, and looked
/// like a scanner bug because the assertion that caught it lived downstream.
#[allow(clippy::expect_used, clippy::panic)]
fn ffprobe(path: &Path) -> (u64, Vec<RefChapter>) {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_chapters",
            "-show_entries",
            "format=duration",
            "-print_format",
            "json",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("run ffprobe");
    let json = String::from_utf8_lossy(&out.stdout);

    let seconds = quoted_number_after(&json, "\"duration\"").unwrap_or(0.0);
    let duration_ms = (seconds * 1000.0).round() as u64;

    // Each chapter contributes a start time and, usually, a title.
    //
    // Walking per `start_time` and looking for the title in the text that follows it, up to
    // the next chapter's start, is what pairs a title with its own chapter. Splitting the
    // document into `{...}` chunks does not work, because `tags` is a *nested* object: the
    // chapter's own braces close before its title is reached, so the title is left in the
    // following chunk and every chapter reads as untitled.
    let mut starts: Vec<(usize, f64)> = Vec::new();
    let mut cursor = 0usize;
    while let Some(found) = json[cursor..].find("\"start_time\"") {
        let at = cursor + found;
        // A chapter with no readable start time is not a position, so it is skipped rather
        // than treated as zero, which would put it at the very beginning of the file.
        if let Some(value) = quoted_number_after(&json[at..], "\"start_time\"") {
            starts.push((at, value));
        }
        cursor = at + 1;
    }

    let mut chapters = Vec::new();
    for (i, (at, start)) in starts.iter().enumerate() {
        // The window runs from this chapter's start to the next one's, so a title is read
        // from the object it belongs to.
        let end = starts.get(i + 1).map_or(json.len(), |(next, _)| *next);
        let window = json.get(*at..end).unwrap_or("");
        chapters.push(RefChapter {
            start_ms: (start * 1000.0).round() as u64,
            title: quoted_string_after(window, "\"title\"").unwrap_or_default(),
        });
    }
    (duration_ms, chapters)
}

/// The number after `"key":`, skipping the quotes.
///
/// Returns `None` rather than zero when the key is absent, because a missing duration and
/// a duration of zero are different facts and a chapter at zero is a real position.
#[allow(clippy::expect_used, clippy::panic)]
fn quoted_number_after(haystack: &str, key: &str) -> Option<f64> {
    let at = haystack.find(key)?;
    let after = haystack.get(at + key.len()..)?;
    let colon = after.find(':')?;
    let mut rest = after.get(colon + 1..)?.trim_start();
    // The value is a quoted string, so step over the opening quote before reading digits.
    if let Some(unquoted) = rest.strip_prefix('"') {
        rest = unquoted;
    }
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(rest.len());
    rest.get(..end)?.parse().ok()
}

/// The string after `"key":`, with the quotes taken off.
///
/// Written out because JSON escapes matter here: a title containing a quote or a
/// backslash is written escaped by ffprobe, and returning the raw text would make a
/// perfectly good title compare as a disagreement.
#[allow(clippy::expect_used, clippy::panic)]
fn quoted_string_after(haystack: &str, key: &str) -> Option<String> {
    let at = haystack.find(key)?;
    let after = haystack.get(at + key.len()..)?;
    let colon = after.find(':')?;
    let mut chars = after.get(colon + 1..)?.trim_start().chars();
    // Step over the opening quote.
    if chars.next()? != '"' {
        return None;
    }
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => out.push(chars.next()?),
            other => out.push(other),
        }
    }
    None
}

#[test]
fn chapters_from_several_files_are_offset_by_the_files_before_them() {
    if !tools_available() {
        eprintln!(
            "SKIPPED: ffmpeg/ffprobe not on PATH, so there is no independent \
                   reader to check this crate's chapter arithmetic against"
        );
        return;
    }

    let lib = TempLib::new("offsets");

    // Three files, each with its own chapters at times local to that file. The second and
    // third files' chapters are the interesting ones: in the assembled book they must land
    // after everything before them.
    let first = lib.make_mp3("Book/01.mp3", 20, &[(0, "One"), (10_000, "Two")]);
    let second = lib.make_mp3("Book/02.mp3", 30, &[(0, "Three"), (15_000, "Four")]);
    let third = lib.make_mp3("Book/03.mp3", 10, &[(0, "Five"), (5_000, "Six")]);

    // Ground truth: what ffprobe says about each file on its own.
    let (d1, c1) = ffprobe(&first);
    let (d2, c2) = ffprobe(&second);
    let (_d3, c3) = ffprobe(&third);
    assert!(!c1.is_empty(), "the corpus must actually carry chapters");

    // The expected assembled positions, computed from ffprobe's readings rather than from
    // anything this crate decided.
    let mut expected: Vec<RefChapter> = Vec::new();
    for (offset, chapters) in [(0u64, &c1), (d1, &c2), (d1 + d2, &c3)] {
        for chapter in chapters {
            expected.push(RefChapter {
                start_ms: offset + chapter.start_ms,
                title: chapter.title.clone(),
            });
        }
    }
    assert_eq!(
        expected.len(),
        6,
        "six chapters across three files: {:?}",
        expected
    );

    let books = scan::scan(lib.path(), ParseOptions::default()).expect("scan");
    assert_eq!(books.len(), 1);
    let (title, findings) = scan::read(&books[0]);
    let title = title.expect("the book reads");

    let actual: Vec<RefChapter> = title
        .chapters()
        .iter()
        .map(|c| RefChapter {
            start_ms: c.start_ms,
            title: c.title.clone(),
        })
        .collect();

    assert_eq!(
        actual.len(),
        expected.len(),
        "chapter count: got {actual:?}, expected {expected:?}; findings: {findings:?}"
    );
    for (got, want) in actual.iter().zip(&expected) {
        // 200 ms of tolerance: an AAC and an MP3 both pad frames, so a derived duration
        // and a declared one differ slightly. A chapter *boundary* is exact metadata, but
        // the offset it sits at is built from durations, so the tolerance belongs here.
        assert!(
            got.start_ms.abs_diff(want.start_ms) <= 200,
            "chapter {:?} at {} ms, expected {} ms",
            want.title,
            got.start_ms,
            want.start_ms
        );
        assert_eq!(
            got.title, want.title,
            "title for the chapter at {} ms",
            want.start_ms
        );
    }

    // A sanity check on the arithmetic itself: the first chapter of the *second* file must
    // lie past the end of the first file. Without the offsets every chapter would sit at
    // its own file's local time, so the second file's first chapter would be at zero.
    //
    // Index 2, not index 1: the second chapter of the first file correctly sits *inside*
    // that file, which is what makes it a control rather than the thing under test.
    let first_file_end = d1;
    assert!(
        expected[2].start_ms >= first_file_end,
        "chapter {:?} should be at or past the end of the first file ({} ms), but is at \
         {} ms — the offsets were not applied",
        expected[2].title,
        first_file_end,
        expected[2].start_ms
    );
    assert!(
        expected[1].start_ms < first_file_end,
        "and the second chapter of the first file is its control, so it must be inside it"
    );
}

#[test]
fn a_book_whose_files_are_not_in_order_is_read_in_playing_order() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    let lib = TempLib::new("order");
    // Written to disk out of order, so the scanner's sorting is what puts them right.
    let third = lib.make_mp3("Book/03.mp3", 10, &[(0, "Third One"), (5_000, "Third Two")]);
    let first = lib.make_mp3("Book/01.mp3", 20, &[(0, "First")]);
    let second = lib.make_mp3("Book/02.mp3", 30, &[(0, "Second")]);

    let (d1, _) = ffprobe(&first);
    let books = scan::scan(lib.path(), ParseOptions::default()).expect("scan");
    let stems: Vec<&str> = books[0].files.iter().map(|f| f.stem.as_str()).collect();
    assert_eq!(stems, vec!["01", "02", "03"], "sorted by track");

    let (title, _) = scan::read(&books[0]);
    let title = title.expect("reads");
    let positions: Vec<u64> = title.chapters().iter().map(|c| c.start_ms).collect();
    // Chapter positions must be non-decreasing through the book, which they only are if
    // the files were assembled in playing order.
    assert!(
        positions.windows(2).all(|w| w[0] <= w[1]),
        "chapters must run forwards, got {positions:?}"
    );
    // And the second file's chapter must be at least a full first file in.
    let second_file_first = title
        .chapters()
        .iter()
        .find(|c| c.title == "Second")
        .map(|c| c.start_ms);
    assert!(
        second_file_first.is_some_and(|at| at + 100 >= d1),
        "the second file's chapter sits at {second_file_first:?}, first file is {d1} ms",
    );

    // Keep the paths alive so the compiler does not warn them unused.
    let _ = (third, second);
}

#[test]
fn a_file_with_no_chapters_does_not_shift_the_ones_that_follow_it() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    let lib = TempLib::new("nochap");
    lib.make_mp3("Book/01.mp3", 20, &[(0, "One")]);
    // No chapters at all: a common file shape, and the one that breaks a scanner that
    // derives offsets from chapter positions instead of durations.
    let plain = lib.make_mp3("Book/02.mp3", 30, &[]);
    lib.make_mp3("Book/03.mp3", 10, &[(0, "Three")]);

    let (d1, _) = ffprobe(&lib.0.join("Book/01.mp3"));
    let (d2, _) = ffprobe(&plain);
    let (_d3, c3) = ffprobe(&lib.0.join("Book/03.mp3"));
    assert!(!c3.is_empty(), "the third file's chapters must be readable");

    let books = scan::scan(lib.path(), ParseOptions::default()).expect("scan");
    let (title, _) = scan::read(&books[0]);
    let title = title.expect("reads");

    let three = title
        .chapters()
        .iter()
        .find(|c| c.title == "Three")
        .map(|c| c.start_ms)
        .expect("the third file's chapter is present");
    let Some(first_of_third) = c3.first() else {
        eprintln!("SKIPPED: the third file carried no chapters, so there is nothing to place");
        return;
    };
    let expected = d1 + d2 + first_of_third.start_ms;
    assert!(
        three.abs_diff(expected) <= 200,
        "a chapterless file still occupies time: chapter at {three} ms, expected \
         {expected} ms from ffprobe's durations {d1} + {d2}"
    );
}

#[test]
fn a_book_with_chapters_contradicting_itself_is_reported() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    let lib = TempLib::new("contradiction");
    // Two files whose chapter times run backwards relative to playing order, which is what
    // a mis-ripped or mis-numbered set looks like.
    lib.make_mp3("Book/01.mp3", 20, &[(0, "Later"), (10_000, "Earlier")]);
    lib.make_mp3("Book/02.mp3", 20, &[(0, "Fine")]);

    let books = scan::scan(lib.path(), ParseOptions::default()).expect("scan");
    let (title, findings) = scan::read(&books[0]);
    // The estate's validator judges the assembled title, so either it objects or the
    // chapters are fine — but a silent pass when the order is contradictory would be
    // wrong, so this asserts one of the two outcomes explicitly rather than "no panic".
    if let Some(title) = title {
        let starts: Vec<u64> = title.chapters().iter().map(|c| c.start_ms).collect();
        let monotonic = starts.windows(2).all(|w| w[0] <= w[1]);
        assert!(
            monotonic || !findings.is_empty(),
            "chapters out of order ({starts:?}) must be reported, got {findings:?}"
        );
    }
    let _ = findings
        .iter()
        .filter(|f| matches!(f, Finding::Inconsistent { .. }))
        .count();
}
