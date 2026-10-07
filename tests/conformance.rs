//! The scanner against the conformance corpus.
//!
//! # Why a second harness, when there already is one
//!
//! `audiobook-conformance` checks that the estate's *readers* agree with ffprobe on files
//! ffmpeg produced. It does not check that the estate's *library view* agrees — that a
//! book assembled from several files has its chapters laid out at the offsets a player
//! would use, and that a file with no chapters is seen as having none rather than as
//! having one giant chapter.
//!
//! That gap is real, and it is the same shape as one this estate has hit before: the
//! conformance corpus exercises the crates file by file, while a library is a claim about
//! *relationships between* files. A claim no test makes is a claim nothing verifies.
//!
//! The corpus is generated here with ffmpeg, which keeps ffprobe as the only judge. The
//! scanner's reading of a book's chapters is compared against ffprobe's per-file readings
//! composed the way a player composes them: each file's chapters offset by the total
//! duration of the files before it.
//!
//! The lints the library runs under are relaxed here because this file is test setup and
//! test oracle: a missing binary means the assertion cannot run at all.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

use audiobook_shelf::naming::ParseOptions;
use audiobook_shelf::scan;

/// Skip loudly when the oracle is missing, rather than passing without checking.
fn tools_available() -> bool {
    ["ffmpeg", "ffprobe"].iter().all(|tool| {
        Command::new(tool)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

struct TempLib(PathBuf);

impl TempLib {
    #[allow(clippy::expect_used, clippy::panic)]
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "audiobook-shelf-conformance-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create corpus");
        TempLib(base)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// Mux a real MP3 carrying `chapters` as ID3 `CHAP` frames.
    #[allow(clippy::expect_used, clippy::panic)]
    fn mp3(&self, relative: &str, seconds: u32, chapters: &[(u64, &str)]) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        let meta = path.with_extension("ffmeta");
        let total_ms = u64::from(seconds) * 1000;
        let mut text = String::from(";FFMETADATA1\n");
        for (i, (start_ms, title)) in chapters.iter().enumerate() {
            let end = chapters.get(i + 1).map_or(total_ms, |(n, _)| *n);
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
        let _ = std::fs::remove_file(&meta);
        path
    }
}

impl Drop for TempLib {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One chapter as ffprobe reports it, with its start in milliseconds.
#[derive(Debug, Clone)]
struct Reading {
    start_ms: u64,
    title: String,
}

/// ffprobe's chapters and duration for one file.
#[allow(clippy::expect_used, clippy::panic)]
fn ffprobe(path: &Path) -> (u64, Vec<Reading>) {
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

    let duration = json
        .split("\"duration\":")
        .nth(1)
        // ffprobe writes a space after the colon, so the quote is not the next character.
        // Trimming before looking for it is the difference between a duration and a
        // duration of zero, which is what the first version read here.
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix('"'))
        .and_then(|rest| {
            let end = rest
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(rest.len());
            rest.get(..end).and_then(|s| s.parse::<f64>().ok())
        })
        .map(|s| (s * 1000.0).round() as u64)
        .unwrap_or(0);

    // Chapter objects are walked by `start_time`, and a title is read from the text up to
    // the next chapter, because `tags` is a nested object and a flat search pairs a title
    // with the chapter after it.
    let mut starts: Vec<(usize, f64)> = Vec::new();
    let mut cursor = 0usize;
    while let Some(found) = json[cursor..].find("\"start_time\":") {
        let at = cursor + found;
        if let Some(after) = json.get(at..) {
            if let Some(colon) = after.find(':') {
                if let Some(rest) = after.get(colon + 1..) {
                    let rest = rest.trim_start();
                    let rest = rest.strip_prefix('"').unwrap_or(rest);
                    let end = rest
                        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                        .unwrap_or(rest.len());
                    if let Ok(value) = rest.get(..end).unwrap_or("").parse::<f64>() {
                        starts.push((at, value));
                    }
                }
            }
        }
        cursor = at + 1;
    }

    let mut chapters = Vec::new();
    for (i, (at, seconds)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(json.len(), |(next, _)| *next);
        let window = json.get(*at..end).unwrap_or("");
        let title = window
            .find("\"title\":")
            .and_then(|t| {
                let after = window.get(t + 8..)?.trim_start();
                let body = after.strip_prefix('"')?;
                let close = body.find('"')?;
                Some(body[..close].to_string())
            })
            .unwrap_or_default();
        chapters.push(Reading {
            start_ms: (seconds * 1000.0).round() as u64,
            title,
        });
    }
    (duration, chapters)
}

/// What the scanner says a book's chapters are, flattened.
#[allow(clippy::expect_used, clippy::panic)]
fn scanner_reading(book: &audiobook_shelf::layout::Book) -> Vec<Reading> {
    let (title, _) = scan::read(book);
    let title = title.expect("the book reads");
    title
        .chapters()
        .iter()
        .map(|c| Reading {
            start_ms: c.start_ms,
            title: c.title.clone(),
        })
        .collect()
}

/// What a player would show: each file's chapters offset by the durations before it.
#[allow(clippy::expect_used, clippy::panic)]
fn composed(durations: &[u64], per_file: &[Vec<Reading>]) -> Vec<Reading> {
    let mut out = Vec::new();
    let mut offset = 0u64;
    for (i, chapters) in per_file.iter().enumerate() {
        for chapter in chapters {
            out.push(Reading {
                start_ms: offset + chapter.start_ms,
                title: chapter.title.clone(),
            });
        }
        offset += durations.get(i).copied().unwrap_or(0);
    }
    out
}

/// The tolerance for a chapter position.
///
/// Durations are derived from frame counts and both MP3 and AAC pad frames, so two
/// readers legitimately differ by a frame or two. Chapter boundaries are exact metadata,
/// but the *offset* a boundary sits at is built from durations, which is why the tolerance
/// is on the offset rather than on the boundary itself.
const TOLERANCE_MS: u64 = 200;

#[test]
fn a_book_across_several_files_lays_out_as_a_player_would() {
    if !tools_available() {
        eprintln!(
            "SKIPPED: ffmpeg/ffprobe not on PATH, so there is no independent reader \
                   to check the library view against"
        );
        return;
    }
    let lib = TempLib::new("threefiles");
    let _a = lib.mp3("1994 - Book/01.mp3", 20, &[(0, "One"), (10_000, "Two")]);
    let _b = lib.mp3("1994 - Book/02.mp3", 25, &[(0, "Three"), (12_000, "Four")]);
    let _c = lib.mp3("1994 - Book/03.mp3", 15, &[(0, "Five"), (7_000, "Six")]);

    let books = scan::scan(lib.path(), ParseOptions::default()).expect("scan");
    assert_eq!(books.len(), 1, "three files, one book");
    let book = &books[0];

    // Ground truth: ffprobe on each file independently.
    let mut durations = Vec::new();
    let mut per_file = Vec::new();
    for file in &book.files {
        let (duration, chapters) = ffprobe(&file.path);
        durations.push(duration);
        per_file.push(chapters);
    }
    let want = composed(&durations, &per_file);
    assert_eq!(want.len(), 6, "six chapters across three files");
    // The composition is the thing under test as well as the oracle, so assert it is
    // actually offsetting: chapter 3 belongs to file 2 and must sit past file 1.
    assert!(
        durations[0] > 0 && durations[1] > 0,
        "ffprobe reported durations {:?}",
        durations
    );

    let got = scanner_reading(book);
    assert_eq!(
        got.len(),
        want.len(),
        "chapter count: the scanner saw {got:?}, a player would show {want:?}"
    );

    for (i, (mine, theirs)) in got.iter().zip(&want).enumerate() {
        assert!(
            mine.start_ms.abs_diff(theirs.start_ms) <= TOLERANCE_MS,
            "chapter {i} ({:?}): scanner says {} ms, a player would show {} ms",
            theirs.title,
            mine.start_ms,
            theirs.start_ms
        );
        assert_eq!(mine.title, theirs.title, "title of chapter {i}");
    }

    // The property the offsets exist for: a chapter that belongs to a later file must not
    // be inside an earlier one, which is what a scanner that forgot to offset produces.
    // `want[2]` is composed, so it carries the offset; a file-local ffprobe reading would
    // be at zero and would make the check vacuous.
    let first_file_end = durations[0];
    assert!(
        want[2].start_ms >= first_file_end,
        "control: chapter 3 belongs to file 2 and sits at or past {} ms in the composed \
         view",
        first_file_end
    );
    assert!(
        got[2].start_ms >= first_file_end,
        "and so the scanner must agree, or it is not offsetting at all: it said {} ms",
        got[2].start_ms
    );
}

#[test]
fn a_file_with_no_chapters_does_not_look_like_one_giant_chapter() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    // ffprobe reports no chapters for this file. A scanner that synthesised a chapter from
    // the duration would show the book as having one enormous chapter, which is the
    // "invent structure" failure audiobook-core explicitly refuses.
    let lib = TempLib::new("nochapters");
    let _a = lib.mp3("1994 - Book/01.mp3", 20, &[(0, "One")]);
    let _b = lib.mp3("1994 - Book/02.mp3", 20, &[]);

    let books = scan::scan(lib.path(), ParseOptions::default()).expect("scan");
    let got = scanner_reading(&books[0]);

    let (d1, c1) = ffprobe(&books[0].files[0].path);
    let (d2, c2) = ffprobe(&books[0].files[1].path);
    assert!(c2.is_empty(), "the fixture must really have no chapters");
    assert_eq!(d2, d1, "and the two files are the same length");

    assert_eq!(
        got.len(),
        1 + c2.len(),
        "one chapter from the first file, none invented for the second: got {got:?}"
    );
    assert_eq!(got[0].title, c1[0].title);
}

#[test]
fn chapters_from_both_mechanisms_are_not_double_counted() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    // An M4B written by ffmpeg carries both a Nero `chpl` box and a QuickTime chapter
    // track describing the *same* chapters. A scanner that reads both and concatenates
    // shows every chapter twice, which looks plausible until you try to seek.
    let lib = TempLib::new("both");
    let meta = lib.path().join("chapters.ffmeta");
    std::fs::write(
        &meta,
        ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=10000\ntitle=One\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=10000\nEND=20000\ntitle=Two\n",
    )
    .expect("write ffmetadata");
    let m4b = lib.path().join("Both Mechanisms/01.m4b");
    std::fs::create_dir_all(m4b.parent().unwrap()).expect("create book folder");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=20",
            "-i",
            meta.to_str().unwrap(),
            "-map_metadata",
            "1",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            m4b.to_str().unwrap(),
        ])
        .status()
        .expect("run ffmpeg");
    assert!(status.success());

    let books = scan::scan(lib.path(), ParseOptions::default()).expect("scan");
    let got = scanner_reading(&books[0]);
    let (_, reference) = ffprobe(&m4b);
    assert_eq!(reference.len(), 2, "ffprobe sees two chapters");

    assert_eq!(
        got.len(),
        reference.len(),
        "two mechanisms, one chapter list each describing, so two chapters and not four: \
         got {got:?}"
    );
    for (mine, theirs) in got.iter().zip(&reference) {
        assert_eq!(mine.title, theirs.title);
    }
}

#[test]
fn a_single_m4b_book_reads_as_one_book_with_its_chapters() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    // The most common audiobook shape of all: one file, chapters inside it. If this does
    // not work the multi-file path is irrelevant.
    let lib = TempLib::new("single");
    let meta = lib.path().join("c.ffmeta");
    std::fs::write(
        &meta,
        ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=10000\ntitle=Part One\n\
         [CHAPTER]\nTIMEBASE=1/1000\nSTART=10000\nEND=20000\ntitle=Part Two\n",
    )
    .expect("write ffmetadata");
    let m4b = lib.path().join("1997 - Single File Book/01.m4b");
    std::fs::create_dir_all(m4b.parent().unwrap()).expect("create folder");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=20",
            "-i",
            meta.to_str().unwrap(),
            "-map_metadata",
            "1",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            m4b.to_str().unwrap(),
        ])
        .status()
        .expect("run ffmpeg");
    assert!(status.success());

    let books = scan::scan(lib.path(), ParseOptions::default()).expect("scan");
    assert_eq!(books.len(), 1);
    assert_eq!(books[0].name.title, "Single File Book");
    assert_eq!(books[0].name.year, Some(1997));

    let got = scanner_reading(&books[0]);
    let (_, reference) = ffprobe(&m4b);
    assert_eq!(got.len(), reference.len(), "got {got:?}");
    for (mine, theirs) in got.iter().zip(&reference) {
        assert_eq!(mine.title, theirs.title);
        assert!(mine.start_ms.abs_diff(theirs.start_ms) <= TOLERANCE_MS);
    }
}

#[test]
fn an_mp3_and_an_m4b_in_one_folder_are_still_one_book() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    // Mixed containers in one book folder is a real library state, and a scanner keyed on
    // file extension would split it.
    let lib = TempLib::new("mixed");
    let meta = lib.path().join("c.ffmeta");
    std::fs::write(&meta, ";FFMETADATA1\n").expect("write ffmetadata");
    let book = lib.path().join("Mixed Containers");
    std::fs::create_dir_all(&book).expect("create folder");
    let _mp3 = lib.mp3("Mixed Containers/part1.mp3", 15, &[(0, "First")]);
    let m4b = book.join("part2.m4b");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=15",
            "-i",
            meta.to_str().unwrap(),
            "-map_metadata",
            "1",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            m4b.to_str().unwrap(),
        ])
        .status()
        .expect("run ffmpeg");
    assert!(status.success());

    let books = scan::scan(lib.path(), ParseOptions::default()).expect("scan");
    assert_eq!(books.len(), 1, "one folder, one book: {books:?}");
    assert_eq!(books[0].files.len(), 2, "both files belong to it");
}
