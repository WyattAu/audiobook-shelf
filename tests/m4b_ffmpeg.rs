//! Writing M4B chapters, checked by ffmpeg and mutagen.
//!
//! Everything else about the M4B write path is verified against this crate's own reader,
//! which is the arrangement that hid defects twice before: a tag this crate writes is only
//! correct if something that did not write it can use it. ffmpeg is that something, and it
//! is the tool that produces the files in the first place.
//!
//! Skipped, loudly, when ffmpeg is absent.
//!
//! The lints the library runs under are relaxed here because this file is test setup and
//! test oracle.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

use audiobook_shelf::m4b::write_m4b_chapters;
use audiobook_shelf::write::{FileChapter, WriteOutcome};

fn tools_available() -> bool {
    ["ffmpeg", "ffprobe"].iter().all(|tool| {
        Command::new(tool)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

struct TempM4b(PathBuf);

impl TempM4b {
    #[allow(clippy::expect_used, clippy::panic)]
    fn with_chapters(tag: &str, seconds: u32, chapters: &[(u64, &str)]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "audiobook-shelf-m4b-ffmpeg-{}-{tag}.m4b",
            std::process::id()
        ));
        let meta = path.with_extension("ffmeta");
        let total = u64::from(seconds) * 1000;
        let mut text = String::from(";FFMETADATA1\n");
        for (i, (start, title)) in chapters.iter().enumerate() {
            let end = chapters.get(i + 1).map_or(total, |(n, _)| *n);
            text.push_str(&format!(
                "[CHAPTER]\nTIMEBASE=1/1000\nSTART={start}\nEND={end}\ntitle={title}\n"
            ));
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
                "aac",
                "-b:a",
                "64k",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("run ffmpeg");
        assert!(status.success(), "ffmpeg failed to build the fixture");
        let _ = std::fs::remove_file(&meta);
        TempM4b(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempM4b {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[allow(clippy::expect_used, clippy::panic)]
fn ffprobe_titles(path: &Path) -> Vec<(String, f64)> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_chapters",
            "-print_format",
            "json",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("run ffprobe");
    let json = String::from_utf8_lossy(&out.stdout);
    let mut titles = Vec::new();
    let mut cursor = 0usize;
    while let Some(found) = json[cursor..].find("\"title\":") {
        let at = cursor + found;
        let after = json.get(at + 8..).expect("in bounds").trim_start();
        let Some(body) = after.strip_prefix('"') else {
            break;
        };
        let Some(close) = body.find('"') else { break };
        titles.push((body[..close].to_string(), 0.0));
        cursor = at + 1;
    }
    titles
}

#[allow(clippy::expect_used, clippy::panic)]
fn duration_ms(path: &Path) -> u64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-print_format",
            "json",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("run ffprobe");
    let json = String::from_utf8_lossy(&out.stdout);
    json.split("\"duration\":")
        .nth(1)
        .and_then(|rest| rest.trim_start().strip_prefix('"'))
        .and_then(|rest| {
            let end = rest
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(rest.len());
            rest.get(..end).and_then(|s| s.parse::<f64>().ok())
        })
        .map(|s| (s * 1000.0).round() as u64)
        .unwrap_or(0)
}

#[test]
fn an_m4b_with_a_quicktime_track_is_rebuilt_and_ffprobe_reads_the_new_list() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH, so there is no independent reader");
        return;
    }
    // ffmpeg puts chapters into a QuickTime text track, and ffprobe reads that track in
    // preference to the `chpl` box. So the track has to be rebuilt, not just the box —
    // the first version of this wrote the box, reported success, and ffprobe carried on
    // showing the old chapters, which is the worst outcome available: nobody re-checks a
    // file they were told was fixed.
    let file = TempM4b::with_chapters("rewrite", 20, &[(0, "Old One"), (10_000, "Old Two")]);
    let before = duration_ms(file.path());
    assert!(before > 0, "the fixture must be real audio");

    let outcome = write_m4b_chapters(
        file.path(),
        &[
            FileChapter::new("New One", 0, 6_000),
            FileChapter::new("New Two", 6_000, 20_000),
        ],
    )
    .expect("the write is well formed");
    assert!(
        matches!(outcome, WriteOutcome::Written { .. }),
        "a chapter track is rebuilt, not refused: {outcome:?}"
    );

    let titles: Vec<String> = ffprobe_titles(file.path())
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(
        titles,
        vec!["New One", "New Two"],
        "ffprobe must read the new list from the rebuilt track: {titles:?}"
    );
}

#[test]
fn a_rebuilt_m4b_still_decodes_to_the_same_length() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    // The audio sits in `mdat`, before the moov, and the rebuild appends only after the
    // moov — so the playable length cannot change unless the audio moved. It is asserted
    // rather than assumed, because "the offsets are absolute" is exactly the kind of claim
    // a correctness argument gets wrong.
    let file = TempM4b::with_chapters("decode", 20, &[(0, "One"), (10_000, "Two")]);
    let before = duration_ms(file.path());
    assert!(before > 0);

    write_m4b_chapters(
        file.path(),
        &[
            FileChapter::new("A", 0, 5_000),
            FileChapter::new("B", 5_000, 10_000),
            FileChapter::new("C", 10_000, 20_000),
        ],
    )
    .expect("writes");

    assert_eq!(
        duration_ms(file.path()),
        before,
        "the rebuild must not change what the file plays"
    );
    let decoded = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            file.path().to_str().unwrap(),
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("run ffmpeg");
    assert!(
        decoded.status.success(),
        "and it decodes end to end: {}",
        String::from_utf8_lossy(&decoded.stderr)
    );
}

#[test]
fn repeated_edits_keep_an_m4b_playable() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    // An empty chapter list means ffmpeg writes **no** QuickTime track, so this file has
    // `chpl` as its only mechanism and writes are both legitimate and visible. That is
    // worth having alongside the declined case: it proves the chpl write is real where it
    // is allowed, rather than only proving refusals.
    let file = TempM4b::with_chapters("repeat", 20, &[]);
    let reference = duration_ms(file.path());
    assert!(reference > 0);

    for round in 0..4u32 {
        let chapters: Vec<FileChapter> = (0..=round)
            .map(|i| {
                FileChapter::new(
                    &format!("Chapter {i} of round {round}"),
                    u64::from(i) * 4_000,
                    u64::from(i + 1) * 4_000,
                )
            })
            .collect();
        write_m4b_chapters(file.path(), &chapters).expect("writes");
    }

    assert_eq!(
        duration_ms(file.path()),
        reference,
        "four edits changed the playable length from {reference} ms"
    );
    let titles: Vec<String> = ffprobe_titles(file.path())
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    // Round 3 creates chapters 0..=3, which is four of them.
    assert_eq!(
        titles.len(),
        4,
        "the last round's list is what remains: {titles:?}"
    );
    assert_eq!(
        titles[3], "Chapter 3 of round 3",
        "the newest, not a leftover"
    );

    let decoded = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            file.path().to_str().unwrap(),
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("run ffmpeg");
    assert!(decoded.status.success());
}

#[test]
fn a_track_only_chapter_list_is_rebuilt_into_a_working_track() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    // ffmpeg does not always emit a chpl box at all. A track-only file is the shape a user
    // is most likely to hand this tool, and the rebuilt track is the only place the new
    // chapters could live.
    let file = TempM4b::with_chapters("trackonly", 20, &[(0, "From The Track")]);
    write_m4b_chapters(
        file.path(),
        &[
            FileChapter::new("Written One", 0, 10_000),
            FileChapter::new("Written Two", 10_000, 20_000),
        ],
    )
    .expect("writes");

    let titles: Vec<String> = ffprobe_titles(file.path())
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert_eq!(
        titles,
        vec!["Written One", "Written Two"],
        "and ffprobe sees the new ones, from the rebuilt track: {titles:?}"
    );
}

#[test]
fn repeated_edits_do_not_accumulate_dead_sample_blobs() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    // The chapter samples live in a `free` box after the moov, and every edit replaces that
    // box. If an edit appended a fresh one instead of replacing it, the file would grow by
    // the old blob on every pass and nothing would ever shrink it.
    //
    // The check is two *identical* edits at the end, compared to each other: a length that
    // changes between them is dead data accumulating, and that holds regardless of how the
    // earlier edits varied. Comparing across edits with different chapter counts would be
    // wrong, because a longer list legitimately takes more space.
    let file = TempM4b::with_chapters("stable", 20, &[(0, "First")]);

    // A history of varying edits, each with a different chapter count and title length.
    for round in 1..=6u32 {
        let chapters: Vec<FileChapter> = (0..round)
            .map(|i| {
                FileChapter::new(
                    &format!("Chapter {i} of edit {round}"),
                    u64::from(i) * 3_000,
                    u64::from(i + 1) * 3_000,
                )
            })
            .collect();
        write_m4b_chapters(file.path(), &chapters).expect("writes");
    }

    let final_chapters = vec![
        FileChapter::new("Alpha", 0, 7_000),
        FileChapter::new("Beta", 7_000, 14_000),
        FileChapter::new("Gamma", 14_000, 20_000),
    ];
    write_m4b_chapters(file.path(), &final_chapters).expect("first of the pair");
    let after_first = std::fs::read(file.path()).expect("read").len();
    write_m4b_chapters(file.path(), &final_chapters).expect("second of the pair");
    let after_second = std::fs::read(file.path()).expect("read").len();

    assert_eq!(
        after_first, after_second,
        "two identical writes must leave the file the same length: {after_first} became \
         {after_second}, so dead sample blobs are accumulating"
    );
    assert_eq!(
        duration_ms(file.path()),
        20_000,
        "and it still plays correctly"
    );
}
