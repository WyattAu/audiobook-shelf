//! Writing chapters into an M4B.
//!
//! # The one condition that makes this safe
//!
//! An MP4's `mdat` holds the audio, and every `stco`/`co64` chunk offset inside `moov`
//! points at an absolute byte position within it. Moving `mdat` therefore invalidates every
//! offset in the file at once, which is why editing MP4 metadata has a reputation.
//!
//! There is exactly one case where nothing moves: when `moov` is the **last** box in the
//! file. Then growing or shrinking it only changes where the file ends, and the chunk
//! offsets — which point backwards into `mdat` — are untouched. ffmpeg writes `moov` last
//! unless asked not to, so this is the common case and not a special one.
//!
//! This module writes only in that case, and declines the other by name rather than
//! attempting it. The failure it avoids is not a corrupt chapter list; it is an audiobook
//! that plays as silence.
//!
//! # What is written
//!
//! A `chpl` box, the Nero chapter form, which ffmpeg itself writes into every M4B it
//! produces and which `mp4-core`'s writer reproduces byte-identically. The QuickTime
//! chapter track this file also carries is left alone: it describes the same chapters, and
//! rewriting a track means rebuilding its sample tables.

use std::path::Path;

use audiobook_core::MediaProbe;
use mp4_core::{Box_, Size, SizeForm};

use crate::write::{FileChapter, UnchangedReason, WriteOutcome};

/// Write `chapters` into the `chpl` box of an MP4/M4B, in place.
///
/// Only safe when `moov` is the last box, which is the shape ffmpeg produces. Every other
/// shape comes back as [`WriteOutcome::Unchanged`] with
/// [`UnchangedReason::UnsupportedContainer`], because the alternative is invalidating every
/// chunk offset in the file.
///
/// The QuickTime chapter track, when the file has one, is left untouched: it describes the
/// same chapters, and a reader that prefers the track — which most players are — will
/// therefore show the new list only if the two agree. Writing one and not the other is
/// reported rather than hidden.
///
/// # Errors
///
/// Returns an error only when the file cannot be read or written.
pub fn write_m4b_chapters(
    path: &Path,
    chapters: &[FileChapter],
) -> Result<WriteOutcome, std::io::Error> {
    let original = std::fs::read(path)?;

    let mut top =
        mp4_core::parse_boxes(&original).map_err(|e| std::io::Error::other(e.to_string()))?;

    // The moov must be the last top-level box, or nothing after it can be trusted.
    let moov_index = top
        .iter()
        .position(|b| b.box_type == *b"moov")
        .ok_or_else(|| std::io::Error::other("no moov box"))?;
    let moov_is_last = moov_index + 1 == top.len();
    if !moov_is_last {
        return Ok(WriteOutcome::Unchanged {
            reason: UnchangedReason::UnsupportedContainer,
        });
    }

    // A QuickTime chapter track outranks the `chpl` box in ffprobe and in most players.
    // Writing the box alone would leave those showing the old list, so a file that has a
    // track is declined rather than reported as written. Detecting it needs the file
    // parsed, which is cheap next to getting this wrong.
    let parsed =
        mp4_core::Mp4File::parse(&original).map_err(|e| std::io::Error::other(e.to_string()))?;
    if parsed.chapter_track_id.is_some() {
        return Ok(WriteOutcome::Unchanged {
            reason: UnchangedReason::ChapterTrackNotWritable,
        });
    }

    // What the file already carries, so an identical write is a no-op.
    let probe = MediaProbe::probe(&original);
    let existing: Vec<(u64, String)> = probe
        .chapters
        .iter()
        .map(|(start, title)| (*start, title.clone()))
        .collect();
    let wanted: Vec<(u64, String)> = chapters
        .iter()
        .map(|c| (c.start_ms, c.title.clone()))
        .collect();
    if existing == wanted && !chapters.is_empty() {
        return Ok(WriteOutcome::Unchanged {
            reason: UnchangedReason::AlreadyCorrect,
        });
    }

    let mut moov = top.remove(moov_index);

    // Strip every existing `chpl` from the tree, wherever it sits — ffmpeg nests it under
    // `moov > udta`, but the format does not require that, and a reader that walked only
    // one shape would miss the other.
    strip_chpl(&mut moov);

    // An empty chapter list is a request to remove chapters, so no box is written.
    if !chapters.is_empty() {
        moov.children.push(chpl_box(chapters)?);
    }

    // `to_bytes` computes the size from the content, so the moov's length is right by
    // construction rather than by bookkeeping.
    let new_moov = moov.to_bytes();

    let mut out =
        Vec::with_capacity(original.len() - (moov.encoded_len() as usize) + new_moov.len());
    for box_ in &top {
        out.extend_from_slice(&box_.to_bytes());
    }
    out.extend_from_slice(&new_moov);

    let delta = out.len() as i64 - original.len() as i64;
    std::fs::write(path, &out)?;

    Ok(WriteOutcome::Written {
        delta,
        // The audio is before moov, so it cannot have moved.
        in_place: true,
    })
}

/// Remove every `chpl` box from a tree, recursing into children.
fn strip_chpl(box_: &mut Box_) {
    box_.children.retain(|c| c.box_type != *b"chpl");
    for child in &mut box_.children {
        strip_chpl(child);
    }
}

/// Build a `chpl` box carrying `chapters`.
///
/// The chapter list is the Nero form, which is what ffmpeg writes and what `mp4-core`'s
/// writer reproduces byte for byte. Two ceilings apply and neither can be quietly dodged:
/// 255 chapters, and 255 bytes per title. Exceeding either is an error rather than a
/// truncation, because a library that silently drops the last forty chapters of a long
/// book is worse than one that reports it cannot store them.
fn chpl_box(chapters: &[FileChapter]) -> Result<Box_, std::io::Error> {
    if chapters.len() > 255 {
        return Err(std::io::Error::other(format!(
            "{} chapters is more than the chpl format can hold (255)",
            chapters.len()
        )));
    }
    let list = mp4_core::ChapterList {
        chapters: chapters
            .iter()
            .map(|c| mp4_core::Chapter::new(c.start_ms, &c.title))
            .collect(),
    };
    // `to_chpl_box_checked` refuses a title over 255 bytes, which is the failure mode that
    // matters: a silent truncation would produce a chapter whose title is half a sentence.
    let box_ = list
        .to_chpl_box_checked()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(Box_ {
        box_type: box_.box_type,
        size: Size::Fixed(0),
        size_form: SizeForm::Normal,
        extended_type: None,
        payload: box_.payload,
        children: box_.children,
    })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]
    use super::*;
    use std::path::PathBuf;

    /// The smallest MP4 with `moov` last, which is the shape this module writes.
    fn mp4_moov_last(moov_children: Vec<Box_>) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&28u32.to_be_bytes());
        out.extend_from_slice(b"ftypM4A ");
        out.extend_from_slice(&[0, 0, 2, 0]);
        out.extend_from_slice(b"M4A mp42isom");
        // mdat with a little audio in it, so a test can prove it survived.
        let audio: &[u8] = &[0xAA; 64];
        out.extend_from_slice(&((8 + audio.len()) as u32).to_be_bytes());
        out.extend_from_slice(b"mdat");
        out.extend_from_slice(audio);
        let moov = mp4_core::container(*b"moov", moov_children);
        out.extend_from_slice(&moov.to_bytes());
        out
    }

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "audiobook-shelf-m4b-{}-{tag}.m4b",
                std::process::id()
            ));
            std::fs::write(&path, bytes).expect("write");
            TempFile(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn chapters() -> Vec<FileChapter> {
        vec![
            FileChapter::new("One", 0, 5_000),
            FileChapter::new("Two", 5_000, 10_000),
        ]
    }

    /// Where the audio starts in a file this module's fixtures produce.
    fn audio_offset(file: &[u8]) -> usize {
        let mut p = 0usize;
        while p + 8 <= file.len() {
            let size =
                u32::from_be_bytes([file[p], file[p + 1], file[p + 2], file[p + 3]]) as usize;
            if &file[p + 4..p + 8] == b"mdat" {
                return p + 8;
            }
            if size == 0 {
                break;
            }
            p += size;
        }
        usize::MAX
    }

    #[test]
    fn chapters_are_written_into_a_moov_last_file() {
        let file = TempFile::new("write", &mp4_moov_last(vec![]));
        let outcome = write_m4b_chapters(file.path(), &chapters()).expect("writes");
        assert!(
            matches!(outcome, WriteOutcome::Written { .. }),
            "{outcome:?}"
        );

        let probe = MediaProbe::probe(&std::fs::read(file.path()).expect("read"));
        assert_eq!(probe.chapters.len(), 2, "{:?}", probe.chapters);
        assert_eq!(probe.chapters[0].1, "One");
        assert_eq!(probe.chapters[1].1, "Two");
    }

    #[test]
    fn the_audio_is_preserved_and_does_not_move() {
        let audio: &[u8] = &[0xAA; 64];
        let file = TempFile::new("audio", &mp4_moov_last(vec![]));
        write_m4b_chapters(file.path(), &chapters()).expect("writes");

        let after = std::fs::read(file.path()).expect("read");
        let at = audio_offset(&after);
        assert_ne!(at, usize::MAX, "mdat is still there");
        assert_eq!(
            after.get(at..at + audio.len()),
            Some(audio),
            "the audio is byte for byte what it was"
        );
        // And it is the only copy, so a duplicated mdat cannot masquerade as intact.
        assert_eq!(
            audio_offset(&after[at..]),
            usize::MAX,
            "the audio appears once"
        );
    }

    #[test]
    fn chunk_offsets_survive_because_mdat_does_not_move() {
        // The whole reason this module declines to write when moov is not last: the chunk
        // offsets are absolute, and moving mdat invalidates all of them at once.
        let file = TempFile::new("offsets", &mp4_moov_last(vec![]));
        let before = std::fs::read(file.path()).expect("read");
        let before_audio = audio_offset(&before);
        write_m4b_chapters(file.path(), &chapters()).expect("writes");
        let after = std::fs::read(file.path()).expect("read");
        assert_eq!(
            audio_offset(&after),
            before_audio,
            "mdat did not move, so the chunk offsets still hold"
        );
    }

    #[test]
    fn a_moov_that_is_not_last_is_declined_rather_than_corrupted() {
        // A `free` box after moov means moov is not last, and growing it would shift that
        // box. Declined, with the file untouched, because the alternative is invalidating
        // every chunk offset in the file.
        let mut with_free = Vec::new();
        with_free.extend_from_slice(&28u32.to_be_bytes());
        with_free.extend_from_slice(b"ftypM4A ");
        with_free.extend_from_slice(&[0, 0, 2, 0]);
        with_free.extend_from_slice(b"M4A mp42isom");
        with_free.extend_from_slice(&64u32.to_be_bytes());
        with_free.extend_from_slice(b"mdat");
        with_free.extend_from_slice(&[0xAA; 56]);
        let moov = mp4_core::container(*b"moov", vec![]);
        with_free.extend_from_slice(&moov.to_bytes());
        with_free.extend_from_slice(b"\x00\x00\x00\x08free");

        let file = TempFile::new("notlast", &with_free);
        let outcome = write_m4b_chapters(file.path(), &chapters()).expect("declines");
        assert_eq!(
            outcome,
            WriteOutcome::Unchanged {
                reason: UnchangedReason::UnsupportedContainer
            }
        );
        assert_eq!(
            std::fs::read(file.path()).expect("read"),
            with_free,
            "and the file is untouched"
        );
    }

    #[test]
    fn writing_the_same_chapters_twice_is_a_no_op() {
        let file = TempFile::new("idempotent", &mp4_moov_last(vec![]));
        write_m4b_chapters(file.path(), &chapters()).expect("first");
        let after_first = std::fs::read(file.path()).expect("read");

        let outcome = write_m4b_chapters(file.path(), &chapters()).expect("second");
        assert_eq!(
            outcome,
            WriteOutcome::Unchanged {
                reason: UnchangedReason::AlreadyCorrect
            }
        );
        assert_eq!(std::fs::read(file.path()).expect("read"), after_first);
    }

    #[test]
    fn an_existing_chpl_inside_udta_is_replaced() {
        // ffmpeg puts chpl under moov > udta, so a writer that only appended to moov would
        // leave the old one in place and a reader would see whichever it found first.
        let udta = mp4_core::container(
            *b"udta",
            vec![mp4_core::ChapterList {
                chapters: vec![mp4_core::Chapter::new(0, "Old")],
            }
            .to_chpl_box()],
        );
        let file = TempFile::new("udta", &mp4_moov_last(vec![udta]));

        write_m4b_chapters(file.path(), &chapters()).expect("writes");

        let probe = MediaProbe::probe(&std::fs::read(file.path()).expect("read"));
        assert_eq!(probe.chapters.len(), 2, "replaced, not added to");
        assert_eq!(probe.chapters[0].1, "One", "and the new list, not the old");

        // Exactly one chpl remains, so the file cannot disagree with itself.
        let after = std::fs::read(file.path()).expect("read");
        let count = after.windows(4).filter(|w| w == b"chpl").count();
        assert_eq!(count, 1, "one chpl box, not two");
    }

    #[test]
    fn writing_no_chapters_removes_the_chpl_box() {
        let udta = mp4_core::container(
            *b"udta",
            vec![mp4_core::ChapterList {
                chapters: vec![mp4_core::Chapter::new(0, "Old")],
            }
            .to_chpl_box()],
        );
        let file = TempFile::new("remove", &mp4_moov_last(vec![udta]));
        write_m4b_chapters(file.path(), &[]).expect("clears");

        let after = std::fs::read(file.path()).expect("read");
        assert_eq!(
            after.windows(4).filter(|w| w == b"chpl").count(),
            0,
            "the box is gone, not emptied"
        );
        assert!(
            MediaProbe::probe(&after).chapters.is_empty(),
            "and the file reads as chapterless"
        );
    }

    #[test]
    fn more_chapters_than_the_format_holds_is_an_error_not_a_truncation() {
        let file = TempFile::new("toomany", &mp4_moov_last(vec![]));
        let too_many: Vec<FileChapter> = (0..300)
            .map(|i| FileChapter::new(&format!("Chapter {i}"), i * 1_000, (i + 1) * 1_000))
            .collect();
        let err = write_m4b_chapters(file.path(), &too_many).expect_err("refuses");
        assert!(
            err.to_string().contains("255"),
            "the reason names the ceiling: {err}"
        );
        // And the file is untouched, so a failed write cannot leave it half-written.
        assert!(
            MediaProbe::probe(&std::fs::read(file.path()).expect("read"))
                .chapters
                .is_empty()
        );
    }
}
