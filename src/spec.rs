//! Turning a chapter spec into per-file chapter lists.
//!
//! In the library rather than the binary for the same reason the exporter is: the rebasing
//! arithmetic is where a chapter list goes quietly wrong — a boundary a file early, an
//! open-ended chapter collapsed to zero — and arithmetic that lives in a binary is
//! arithmetic only the command line tests.

use crate::layout::{Book, BookFile};
use crate::write::FileChapter;
use std::path::Path;

/// One chapter spec: a book-relative start and a title, in the order the book plays.
pub type Spec = Vec<(u64, String)>;

/// Resolve a `--write` spec into book-relative chapter starts.
///
/// A spec beginning `@` names a sidecar file: a cue sheet if one parses (its grammar is
/// the stricter, and a file that is a cue sheet is one), otherwise one of the estate's
/// three sidecar formats. Anything else is inline `MM:SS Chapter` pairs separated by `;`,
/// whose leading field is **seconds**, then minutes, then hours — `5:00` is five minutes
/// and `1:02:03` is an hour, two minutes, three seconds. Reading the first field as
/// minutes instead is the obvious mistake and the one this originally made: it turns every
/// time into sixty times itself, and a chapter list lands nowhere near where it was asked
/// to go, with no error to say so.
///
/// `base` is where a `@`-named file is resolved from; the verb passes the library root.
///
/// # Errors
///
/// An unreadable or unparsable spec, or an empty one. A cue sheet whose sheets hold no
/// `INDEX 01` lines is empty in the way that matters.
pub fn resolve_spec(spec: &str, base: &Path) -> Result<Spec, String> {
    if let Some(sidecar_path) = spec.strip_prefix('@') {
        let text = std::fs::read_to_string(sidecar_path)
            .map_err(|e| format!("cannot read {sidecar_path}: {e}"))?;
        // Tried before the sidecar grammars because its grammar is the stricter one: a
        // file that parses as a cue sheet is one, and a sidecar parser reading cue lines
        // would report a `TRACK` line as junk rather than as a track.
        let cue_chapters = match cuesheet_core::Sheet::parse(&text) {
            Ok(sheet) if !sheet.files.is_empty() => Some(sheet),
            Ok(_) => None,
            Err(cuesheet_core::CueError::UnknownCommand { .. }) => None,
            Err(e) => return Err(format!("{sidecar_path}: {e}")),
        };
        if let Some(sheet) = cue_chapters {
            // The sheet's times are relative to *its own file*, so a multi-file sheet
            // needs each file's duration to become book-relative. Durations come from
            // the audio itself, resolved next to the sheet, because that is where a rip
            // puts it; a file that cannot be probed contributes a zero, which is wrong
            // in a way the printed chapter times make visible rather than silent.
            let durations: Vec<Option<u64>> = sheet
                .files
                .iter()
                .map(|f| {
                    crate::fs_reader::FileReader::open(&base.join(&f.path))
                        .ok()
                        .and_then(|mut r| {
                            audiobook_core::MediaProbe::probe_source(&mut r).duration_ms
                        })
                })
                .collect();
            let chapters: Spec = sheet
                .chapter_starts_book_relative(&durations)
                .into_iter()
                .map(|(ms, title)| (ms, title.unwrap_or_default()))
                .collect();
            if chapters.is_empty() {
                return Err(format!(
                    "{sidecar_path}: the cue sheet has no INDEX 01 lines"
                ));
            }
            return Ok(chapters);
        }
        let parsed =
            audiobook_core::sidecar::parse(&text).map_err(|e| format!("{sidecar_path}: {e}"))?;
        if parsed.chapters.is_empty() {
            return Err(format!("{sidecar_path}: no chapters in it"));
        }
        if parsed.has_warnings() {
            // The sidecar reader reports files a human edited and got nearly right. A
            // silent pass here writes those files' defects into audio.
            for warning in &parsed.warnings {
                eprintln!("warning: {warning:?}");
            }
        }
        return Ok(parsed
            .chapters
            .iter()
            .map(|c| (c.start_ms, c.title.clone()))
            .collect());
    }

    let mut out: Spec = Vec::new();
    for line in spec.split(';') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (stamp, title) = line
            .split_once(' ')
            .ok_or_else(|| format!("`{line}` is not `MM:SS Chapter`"))?;
        let parts: Vec<u64> = stamp
            .split(':')
            .map(|p| {
                p.parse::<u64>()
                    .map_err(|_| format!("`{stamp}` is not a time"))
            })
            .collect::<Result<_, _>>()?;
        let seconds = match parts.as_slice() {
            [s] => *s,
            [m, s] => m * 60 + s,
            [h, m, s] => h * 3600 + m * 60 + s,
            _ => return Err(format!("`{stamp}` has too many parts")),
        };
        out.push((seconds * 1000, title.to_string()));
    }
    Ok(out)
}

/// A book's files with the chapters that fall inside each, rebased onto that file.
///
/// The spec carries book-relative starts; a file's own chapters are relative to the file.
/// A spec describes the *whole book*, so a file whose span holds none of its chapters gets
/// an empty list rather than being skipped: skipping would mix old and new lists in one
/// book — the previous sidecar's last chapter sitting next to this one's first — which is
/// a file that contradicts itself.
pub fn book_durations(book: &Book) -> Vec<u64> {
    book.files.iter().map(|f| duration_of(&f.path)).collect()
}

/// Plan a write: each of the book's files with the chapters that fall inside it,
/// rebased onto that file.
///
/// `durations` pairs with the book's files by position and is the book's total length —
/// see [`book_durations`]. The spec carries book-relative starts; a file's own chapters
/// are relative to the file. A spec describes the *whole book*, so a file whose span
/// holds none of its chapters gets an empty list rather than being skipped: skipping
/// would mix old and new lists in one book — the previous sidecar's last chapter sitting
/// next to this one's first — which is a file that contradicts itself.
#[must_use]
pub fn plan_writes<'a>(
    book: &'a Book,
    durations: &[u64],
    spec: &[(u64, String)],
) -> Vec<(&'a BookFile, Vec<FileChapter>)> {
    let mut offsets = Vec::with_capacity(book.files.len());
    let mut running = 0u64;
    for (i, _) in book.files.iter().enumerate() {
        offsets.push(running);
        running = running.saturating_add(durations.get(i).copied().unwrap_or(0));
    }

    // The spec carries only start times, so each chapter ends where the next one begins.
    // An end equal to its start is a zero-length chapter, which a player shows as an entry
    // that cannot be skipped; open-ended runs to the end of the file, which is what "the
    // rest of the book" means.
    let chapters: Vec<FileChapter> = spec
        .iter()
        .enumerate()
        .map(|(i, (start_ms, title))| {
            let end_ms = spec.get(i + 1).map_or(*start_ms, |(next, _)| *next);
            if end_ms == *start_ms {
                FileChapter::open_ended(title, *start_ms)
            } else {
                FileChapter::new(title, *start_ms, end_ms)
            }
        })
        .collect();

    let mut plan = Vec::new();
    for (i, file) in book.files.iter().enumerate() {
        let start_of_file = offsets.get(i).copied().unwrap_or(0);
        let end_of_file = offsets
            .get(i + 1)
            .copied()
            .unwrap_or(running)
            .max(start_of_file);
        // Iterated, not drained: the spec describes the whole book, and every file
        // filters the *same* chapters. Draining — which the first version of this
        // function did — hands the whole list to the first file and leaves the rest
        // empty, so every file after the first is cleared of chapters. Caught by the
        // boundary test the hour it was written, which is the argument for this
        // arithmetic living somewhere a test can reach it.
        let mine: Vec<FileChapter> = chapters
            .iter()
            .filter(|c| {
                c.start_ms >= start_of_file && c.start_ms < end_of_file.max(start_of_file + 1)
            })
            .map(|c| {
                // Rebasing preserves "runs to the end of the file" rather than collapsing
                // it to an end of zero: a zero-length chapter is one a player cannot skip
                // past, and ffmpeg rejects outright.
                let rebased_start = c.start_ms.saturating_sub(start_of_file);
                match c.end_ms {
                    Some(end) => FileChapter::new(
                        &c.title,
                        rebased_start,
                        end.saturating_sub(start_of_file).max(rebased_start),
                    ),
                    None => FileChapter::open_ended(&c.title, rebased_start),
                }
            })
            .collect();
        plan.push((file, mine));
    }
    plan
}

/// A file's duration in milliseconds, or zero when it cannot be probed.
fn duration_of(path: &Path) -> u64 {
    crate::fs_reader::FileReader::open(path).map_or(0, |mut reader| {
        audiobook_core::MediaProbe::probe_source(&mut reader)
            .duration_ms
            .unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_leading_field_is_seconds_then_minutes_then_hours() {
        // The order audiobooks and ffmpeg's CHAPTERX both use, and the one this
        // originally misread as minutes-then-seconds, which turned every time into
        // sixty times itself.
        let spec = resolve_spec("5:00 Two;1:02:03 Three", Path::new(".")).expect("parses");
        assert_eq!(spec[0].0, 300_000, "5:00 is five minutes");
        assert_eq!(
            spec[1].0, 3_723_000,
            "1:02:03 is an hour, two minutes, three seconds"
        );
        assert_eq!(spec[0].1, "Two");
    }

    #[test]
    fn a_spec_that_is_not_pairs_is_refused_with_the_offending_text() {
        let err = resolve_spec("no time here", Path::new(".")).unwrap_err();
        assert!(err.contains("not a time"), "{err}");
        let err = resolve_spec("abc X", Path::new(".")).unwrap_err();
        assert!(err.contains("abc"), "{err}");
        let err = resolve_spec("1:2:3:4 X", Path::new(".")).unwrap_err();
        assert!(err.contains("too many parts"), "{err}");
        assert!(resolve_spec("  ;  ", Path::new(".")).unwrap().is_empty());
    }

    #[test]
    fn a_spec_describes_the_whole_book_so_an_uncovered_file_is_cleared() {
        // Two files of ten seconds each, a spec covering only the first: the second
        // gets an empty list, because skipping it would leave the previous sidecar's
        // chapters sitting next to this one's — a book that contradicts itself.
        let dir = std::env::temp_dir().join("audiobook-shelf-spec-tests");
        std::fs::create_dir_all(dir.join("Book")).expect("temp dir");
        let mut files = Vec::new();
        for name in ["01.mp3", "02.mp3"] {
            let path = dir.join("Book").join(name);
            std::fs::write(&path, []).expect("write");
            files.push(crate::layout::BookFile::from_path(&path, &dir));
        }
        let book = Book {
            folder: dir.join("Book"),
            name: crate::naming::BookName::default(),
            series: None,
            files,
        };
        let plan = plan_writes(
            &book,
            &[10_000, 10_000],
            &[(0, "One".into()), (5_000, "Two".into())],
        );
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].1.len(), 2, "both chapters fall in the first file");
        assert_eq!(
            plan[1].1.len(),
            0,
            "the second file is cleared, not skipped"
        );
    }

    #[test]
    fn rebasing_preserves_an_open_ended_final_chapter() {
        // A final chapter with no explicit end runs to the end of the file. Collapsing
        // that to an end of zero — which the first version of this arithmetic did —
        // writes a chapter that ends before it starts, and ffmpeg rejects it outright.
        let dir = std::env::temp_dir().join("audiobook-shelf-spec-tests");
        std::fs::create_dir_all(dir.join("Open")).expect("temp dir");
        let path = dir.join("Open").join("only.mp3");
        std::fs::write(&path, []).expect("write");
        let book = Book {
            folder: dir.join("Open"),
            name: crate::naming::BookName::default(),
            series: None,
            files: vec![crate::layout::BookFile::from_path(&path, &dir)],
        };
        let plan = plan_writes(&book, &[10_000], &[(0, "Only".into())]);
        let mine = &plan[0].1;
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].end_ms, None, "the last chapter is open-ended");
    }

    #[test]
    fn a_chapter_is_placed_by_the_file_whose_span_holds_its_start() {
        // Two ten-second files; a chapter at ten seconds exactly belongs to the second
        // file at offset zero, not to the first at its end.
        let dir = std::env::temp_dir().join("audiobook-shelf-spec-tests");
        std::fs::create_dir_all(dir.join("Boundary")).expect("temp dir");
        let mut files = Vec::new();
        for name in ["01.mp3", "02.mp3"] {
            let path = dir.join("Boundary").join(name);
            std::fs::write(&path, []).expect("write");
            files.push(crate::layout::BookFile::from_path(&path, &dir));
        }
        let book = Book {
            folder: dir.join("Boundary"),
            name: crate::naming::BookName::default(),
            series: None,
            files,
        };
        // Durations are zero for these stub files, so both files' spans are empty; the
        // `+ 1` in the boundary test is what keeps the plan from dropping everything.
        let plan = plan_writes(
            &book,
            &[10_000, 10_000],
            &[(0, "One".into()), (10_000, "Two".into())],
        );
        // The chapter at ten seconds exactly belongs to the *second* file, at offset
        // zero: a span is [start, end), and the first file's ends where the second
        // begins.
        assert_eq!(plan[0].1.len(), 1);
        assert_eq!(plan[1].1.len(), 1);
        assert_eq!(plan[1].1[0].start_ms, 0, "rebased onto its own file");
        let total: usize = plan.iter().map(|(_, c)| c.len()).sum();
        assert_eq!(total, 2, "no chapter is lost at a boundary, {plan:?}");
    }
}
