//! A cue sheet's times, through the estate, back out of ffprobe.
//!
//! `cuesheet-core` documents honestly that no independent cue-sheet parser exists on the
//! machine the estate runs on, so its *grammar* is verified only by its own tests. What
//! this file checks against a real oracle is the part a chapter list exists for: the
//! times. A sheet's `INDEX 01` values are applied to an M4B and ffprobe is asked what it
//! sees. If the parser misreads `MM:SS:FF`, ffprobe reports chapters in the wrong places
//! — and the frames field is exactly where a misread hides, because thirty frames is
//! 400 ms and not the half-second a reader assuming hundredths would report.

// A fixture is allowed to assert: an `expect` here is the test stating what it assumes,
// the honest form, and the crate lint denies it everywhere a caller would write it.
#![allow(clippy::expect_used)]

use audiobook_shelf::fs_reader::FileReader;
use audiobook_shelf::m4b::write_m4b_chapters;
use audiobook_shelf::write::FileChapter;

/// The sheet: three tracks whose `INDEX 01` values exercise the whole `MM:SS:FF` clock.
const SHEET: &str = r#"REM COMMENT "ExactAudioCopy v1.6"
PERFORMER "Some Author"
TITLE "Some Book"
FILE "book.m4b" MP3
  TRACK 01 AUDIO
    TITLE "Frames"
    INDEX 01 00:15:30
  TRACK 02 AUDIO
    TITLE "Minutes"
    INDEX 01 05:00:00
  TRACK 03 AUDIO
    TITLE "The end"
    INDEX 01 14:00:00
"#;

fn has_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
        && std::process::Command::new("ffprobe")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
}

#[test]
fn a_cue_sheet_s_times_survive_into_an_m4b_that_ffprobe_reads_back() {
    if !has_ffmpeg() {
        eprintln!("skipping: no ffmpeg");
        return;
    }
    let dir = std::env::temp_dir().join("audiobook-shelf-cue-e2e");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let m4b = dir.join("book.m4b");

    let out = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=300:duration=900",
            "-c:a",
            "aac",
            "-b:a",
            "32k",
        ])
        .arg(&m4b)
        .output()
        .expect("ffmpeg runs");
    assert!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Parse the sheet and apply its chapters the way `--write @` does.
    let sheet = cuesheet_core::Sheet::parse(SHEET).expect("the sheet parses");
    let durations: Vec<Option<u64>> = sheet
        .files
        .iter()
        .map(|f| {
            FileReader::open(&dir.join(&f.path))
                .ok()
                .and_then(|mut r| audiobook_core::MediaProbe::probe_source(&mut r).duration_ms)
        })
        .collect();
    let chapters: Vec<FileChapter> = sheet
        .chapter_starts_book_relative(&durations)
        .into_iter()
        .map(|(ms, title)| FileChapter {
            start_ms: ms,
            end_ms: None,
            title: title.unwrap_or_default(),
        })
        .collect();
    assert_eq!(chapters.len(), 3);
    write_m4b_chapters(&m4b, &chapters).expect("the write succeeds");

    // The oracle. `INDEX 01 00:15:30` is fifteen seconds and thirty frames — 15 400 ms,
    // not 15.5 s, because thirty frames at 75 to the second are 400 ms. A reader that
    // read the frames field as hundredths puts this chapter a hundred milliseconds late,
    // and every check in this file exists because that is invisible until it is not.
    let json = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_chapters", "-print_format", "json"])
        .arg(&m4b)
        .output()
        .expect("ffprobe runs");
    assert!(json.status.success());
    let text = String::from_utf8_lossy(&json.stdout).to_string();
    // ffprobe writes a space after the colon (`"start_time": "15.400000"`), which has
    // bitten this estate three times in three repositories; splitting on the quoted key
    // *with* the space and trimming before parsing is the shape that survives it.
    let starts: Vec<f64> = text
        .split("\"start_time\": \"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next()?.parse::<f64>().ok())
        .collect();
    assert_eq!(starts.len(), 3, "{text}");
    assert!((starts[0] - 15.4).abs() < 0.01, "frames field: {starts:?}");
    assert!((starts[1] - 300.0).abs() < 0.01, "{starts:?}");
    assert!((starts[2] - 840.0).abs() < 0.01, "{starts:?}");
}

/// An exported sheet, applied back, must produce the chapters it was exported from.
///
/// This is the other half of the cue-sheet workflow: `--export cue` writes a sheet from a
/// book's chapters, a human edits it, and `--write @` puts it back. The writer and the
/// reader are the same crate, which is exactly why the check must go the long way round —
/// through `Sheet::to_text`, back through `Sheet::parse`, into an M4B, and out of ffprobe.
/// A crate agreeing with itself is the weakest evidence there is; ffprobe is not the crate.
#[test]
fn an_exported_sheet_applies_back_to_the_chapters_it_was_exported_from() {
    if !has_ffmpeg() {
        eprintln!("skipping: no ffmpeg");
        return;
    }
    let dir = std::env::temp_dir().join("audiobook-shelf-cue-export");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let m4b = dir.join("book.m4b");

    let out = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=300:duration=600",
            "-c:a",
            "aac",
            "-b:a",
            "32k",
        ])
        .arg(&m4b)
        .output()
        .expect("ffmpeg runs");
    assert!(out.status.success());

    // The book's chapters, as a player would see them.
    let original: Vec<(u64, &str)> = vec![(0, "Opening"), (90_000, "Middle"), (295_000, "The end")];

    // Export: chapters in, sheet text out.
    let mut sheet = cuesheet_core::Sheet {
        title: Some("Round trip".to_string()),
        ..cuesheet_core::Sheet::default()
    };
    sheet.files.push(cuesheet_core::CueFile {
        path: "book.m4b".to_string(),
        file_type: cuesheet_core::FileType::Mp3,
        tracks: original
            .iter()
            .enumerate()
            .map(|(i, (ms, title))| cuesheet_core::Track {
                number: u8::try_from(i + 1).expect("under 99"),
                mode: cuesheet_core::TrackMode::Audio,
                title: Some((*title).to_string()),
                performer: None,
                songwriter: None,
                indices: vec![cuesheet_core::Index { number: 1, ms: *ms }],
                gaps: Vec::new(),
            })
            .collect(),
    });
    let text = sheet.to_text().expect("the sheet is writable");

    // Apply: sheet text in, chapters written.
    let back = cuesheet_core::Sheet::parse(&text).expect("the exported sheet parses");
    let chapters: Vec<FileChapter> = back.files[0]
        .chapter_starts()
        .into_iter()
        .map(|(ms, title)| FileChapter {
            start_ms: ms,
            end_ms: None,
            title: title.unwrap_or_default(),
        })
        .collect();
    assert_eq!(chapters.len(), 3);
    write_m4b_chapters(&m4b, &chapters).expect("the write succeeds");

    let json = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_chapters", "-print_format", "json"])
        .arg(&m4b)
        .output()
        .expect("ffprobe runs");
    let text = String::from_utf8_lossy(&json.stdout).to_string();
    let starts: Vec<f64> = text
        .split("\"start_time\": \"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next()?.parse::<f64>().ok())
        .collect();
    assert_eq!(starts.len(), 3, "{text}");
    for (seen, (ms, _)) in starts.iter().zip(original.iter()) {
        assert!(
            (seen - (*ms as f64) / 1_000.0).abs() < 0.014,
            "{seen} s is not {ms} ms within one CD frame"
        );
    }
}
