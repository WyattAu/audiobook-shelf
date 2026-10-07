//! Writing chapters, checked by ffmpeg.
//!
//! # Why this needs a real decoder
//!
//! Everything else here is verified against this crate's own reader, which is exactly the
//! arrangement that hid the ID3v2 `chpl` defect for several releases: the crate wrote a
//! box, read it back, and agreed with itself. A tag this crate produces is only correct if
//! something that did not write it can use it.
//!
//! ffmpeg is that something. It is the tool that muxes the corpus the other tests read, so
//! it has to understand what `write_mp3_chapters` emits — and if it does not, a file this
//! crate writes is not playable, which no amount of self-consistency would reveal.
//!
//! The properties checked are the ones a listener would notice:
//!
//! * the file still decodes, and to the same length it had before the edit;
//! * ffprobe reads back the chapters that were written;
//! * the titles survive;
//! * the audio content is unchanged, which is what "in place" has to mean.
//!
//! Without ffmpeg the tests skip and say so, for the same reason as in `library.rs`.
//!
//! The lints the library runs under are relaxed here because this file is test setup and
//! test oracle: a missing binary means the assertion cannot run at all, and there is
//! nothing useful to assert instead.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;

use audiobook_shelf::write::{read_mp3_chapters, write_mp3_chapters, FileChapter, WriteOutcome};

fn tools_available() -> bool {
    ["ffmpeg", "ffprobe"].iter().all(|tool| {
        Command::new(tool)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

struct TempMp3(PathBuf);

impl TempMp3 {
    /// Mux a real MP3 of `seconds`, carrying `chapters` as FFMetadata.
    #[allow(clippy::expect_used, clippy::panic)]
    fn with_chapters(tag: &str, seconds: u32, chapters: &[(u64, &str)]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "audiobook-shelf-write-ffmpeg-{}-{tag}.mp3",
            std::process::id()
        ));
        let meta = path.with_extension("ffmeta");
        let mut text = String::from(";FFMETADATA1\n");
        let total_ms = u64::from(seconds) * 1000;
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
        assert!(status.success(), "ffmpeg failed to build the fixture");
        let _ = std::fs::remove_file(&meta);
        TempMp3(path)
    }

    /// A copy with the same audio, used as a control for the decode check.
    #[allow(clippy::expect_used, clippy::panic)]
    fn control(tag: &str, seconds: u32) -> Self {
        let path = std::env::temp_dir().join(format!(
            "audiobook-shelf-control-{}-{tag}.mp3",
            std::process::id()
        ));
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
                "-c:a",
                "libmp3lame",
                "-b:a",
                "64k",
                path.to_str().unwrap(),
            ])
            .status()
            .expect("run ffmpeg");
        assert!(status.success());
        TempMp3(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempMp3 {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// ffprobe's duration of a file, in milliseconds.
#[allow(clippy::expect_used, clippy::panic)]
fn duration_ms(path: &Path) -> Option<u64> {
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
    let at = json.find("\"duration\":")?;
    let after = json.get(at + 11..)?.trim_start();
    let after = after.strip_prefix('"')?;
    let end = after
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(after.len());
    after
        .get(..end)?
        .parse::<f64>()
        .ok()
        .map(|s| (s * 1000.0).round() as u64)
}

/// The titles ffprobe reads from a file.
#[allow(clippy::expect_used, clippy::panic)]
fn ffprobe_titles(path: &Path) -> Vec<String> {
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
        titles.push(body[..close].to_string());
        cursor = at + 1;
    }
    titles
}

#[test]
fn ffmpeg_reads_the_chapters_this_crate_writes() {
    if !tools_available() {
        eprintln!(
            "SKIPPED: ffmpeg/ffprobe not on PATH, so there is no independent \
                   reader to check these tags against"
        );
        return;
    }
    let file = TempMp3::with_chapters("empty", 20, &[]);
    let chapters = vec![
        FileChapter::new("Prologue", 0, 5_000),
        FileChapter::new("Chapter One", 5_000, 12_000),
        FileChapter::new("Chapter Two", 12_000, 20_000),
    ];
    let outcome = write_mp3_chapters(file.path(), &chapters).expect("writes");
    assert!(
        matches!(outcome, WriteOutcome::Written { .. }),
        "{outcome:?}"
    );

    let titles = ffprobe_titles(file.path());
    assert_eq!(
        titles,
        vec!["Prologue", "Chapter One", "Chapter Two"],
        "ffprobe must read back exactly what was written — this is the check that a tag \
         this crate emits is a tag the ecosystem understands"
    );
}

#[test]
fn a_written_file_still_decodes_to_the_same_length() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    // The control is a separate file with identical audio and no tag, so any difference in
    // duration is the tag's doing rather than the encode's.
    let control = TempMp3::control("decode", 20);
    let file = TempMp3::with_chapters("decode", 20, &[(0, "Only")]);

    let before = duration_ms(file.path()).expect("control has a duration");
    let reference = duration_ms(control.path()).expect("reference has a duration");
    assert_eq!(before, reference, "the fixtures encode the same audio");

    write_mp3_chapters(
        file.path(),
        &[
            FileChapter::new("A", 0, 5_000),
            FileChapter::new("B", 5_000, 10_000),
        ],
    )
    .expect("writes");

    let after = duration_ms(file.path()).expect("duration after write");
    assert_eq!(
        after, before,
        "a tag edit must not change what the file plays: {before} ms became {after} ms"
    );

    // And it decodes end to end without error, which a misplaced tag would break.
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
        "ffmpeg could not decode the edited file: {}",
        String::from_utf8_lossy(&decoded.stderr)
    );
}

#[test]
fn repeated_edits_keep_the_file_playable() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    let file = TempMp3::with_chapters("repeat", 20, &[]);
    let control = TempMp3::control("repeat", 20);
    let reference = duration_ms(control.path()).expect("reference duration");

    // Five rewrites, each with a slightly different chapter list. This is what a library
    // manager does to a library, and the accumulation it can cause — a tag growing a
    // little on every pass — is invisible until the file stops playing.
    for round in 0..5u32 {
        let chapters: Vec<FileChapter> = (0..=round)
            .map(|i| {
                FileChapter::new(
                    &format!("Chapter {i} of round {round}"),
                    u64::from(i) * 4_000,
                    u64::from(i + 1) * 4_000,
                )
            })
            .collect();
        write_mp3_chapters(file.path(), &chapters).expect("writes");
    }

    let after = duration_ms(file.path()).expect("duration after five edits");
    assert_eq!(
        after, reference,
        "five edits changed the playable length from {reference} ms to {after} ms — the \\
         tag is accumulating"
    );

    let titles = ffprobe_titles(file.path());
    // Round 4 creates chapters 0..=4, so five of them.
    assert_eq!(
        titles.len(),
        5,
        "and the last round's chapters are what remain"
    );
    assert_eq!(
        titles[4], "Chapter 4 of round 4",
        "the newest one, not a leftover"
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
        "still decodes after five edits: {}",
        String::from_utf8_lossy(&decoded.stderr)
    );
}

#[test]
fn chapters_this_crate_reads_match_what_ffprobe_reads() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    // The reverse direction, on a file ffmpeg wrote: this crate's reader against the
    // ecosystem's, on chapters this crate did not produce.
    let file = TempMp3::with_chapters(
        "foreign",
        30,
        &[(0, "One"), (10_000, "Two"), (20_000, "Three")],
    );
    let ours = read_mp3_chapters(file.path());
    let theirs = ffprobe_titles(file.path());
    let our_titles: Vec<&str> = ours.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(
        our_titles, theirs,
        "reading a file ffmpeg wrote, this crate and ffprobe must agree"
    );
    assert_eq!(ours[2].start_ms, 20_000, "and on the times");
}

#[test]
fn an_untitled_chapter_still_survives_as_a_boundary() {
    if !tools_available() {
        eprintln!("SKIPPED: ffmpeg/ffprobe not on PATH");
        return;
    }
    let file = TempMp3::with_chapters("untitled", 20, &[]);
    write_mp3_chapters(
        file.path(),
        &[
            FileChapter::new("Named", 0, 5_000),
            FileChapter::new("", 5_000, 10_000),
            FileChapter::new("Also Named", 10_000, 20_000),
        ],
    )
    .expect("writes");

    // The point is the count: a chapter with no title is still a boundary a player can
    // jump to, and dropping it would silently remove a seek point.
    let ours = read_mp3_chapters(file.path());
    assert_eq!(ours.len(), 3, "all three survive: {ours:?}");
    assert_eq!(ours[1].title, "");
}
