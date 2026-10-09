//! Writing chapters back into a file.
//!
//! # The constraint that shapes everything here
//!
//! An MP3's audio starts where its ID3v2 tag stops. Replacing that tag with a longer one
//! shifts every audio byte, so a naive "read the tag, change it, write the file back"
//! corrupts the file — and the corruption is silent, because the audio still *plays*, just
//! from the wrong offset and with the first frame truncated.
//!
//! Three things make it safe, and this module does all three:
//!
//! 1. **The tag is bounded by its own declared size**, not by "the rest of the file", so
//!    the bytes after it are preserved byte for byte.
//! 2. **A tag is never made shorter than it was** when the old padding can absorb the
//!    difference, because padding exists precisely so a tag can grow in place.
//! 3. **ID3v1 is a fixed 128-byte trailer** at the end of the file, so it is preserved at
//!    its offset rather than re-emitted wherever the new tag happens to end.
//!
//! # What it will not do
//!
//! It does not re-encode audio, and it does not rewrite an MP4. It edits tags in place,
//! which is the one operation a library manager performs constantly and the one most likely
//! to destroy a collection if done carelessly.

use std::path::Path;

use audiobook_core::MediaProbe;
use id3_core::{Chapter, Tag, TagV1};

/// A chapter to write into one file.
///
/// Deliberately its own type rather than [`audiobook_core::Chapter`], which is a
/// *title-level* chapter: its `start_ms` is part-local and it has no end time at all, end
/// times belonging to `Part`. A writer for a single file needs both a start and an end,
/// and borrowing the title-level type would mean either inventing an `end_ms` field on it
/// or writing chapters with no ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChapter {
    /// The chapter's title. May be empty; a boundary with no name is still a boundary.
    pub title: String,
    /// Start offset within this file, in milliseconds.
    pub start_ms: u64,
    /// End offset within this file, or `None` for one that runs to the end of the file.
    pub end_ms: Option<u64>,
}

impl FileChapter {
    /// A chapter from `start_ms` to `end_ms`.
    #[must_use]
    pub fn new(title: &str, start_ms: u64, end_ms: u64) -> Self {
        Self {
            title: String::from(title),
            start_ms,
            end_ms: Some(end_ms),
        }
    }

    /// A chapter that runs to the end of the file.
    #[must_use]
    pub fn open_ended(title: &str, start_ms: u64) -> Self {
        Self {
            title: String::from(title),
            start_ms,
            end_ms: None,
        }
    }
}

/// What happened when chapters were written.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteOutcome {
    /// The file now carries the chapters.
    Written {
        /// Bytes the file grew or shrank by.
        delta: i64,
        /// Whether padding absorbed the change so the audio did not move.
        in_place: bool,
    },
    /// The file was left exactly as it was.
    Unchanged {
        /// Why nothing was written.
        reason: UnchangedReason,
    },
}

impl WriteOutcome {
    /// Whether the file's audio bytes moved.
    #[must_use]
    pub fn audio_moved(&self) -> bool {
        matches!(
            self,
            WriteOutcome::Written {
                in_place: false,
                ..
            }
        )
    }
}

/// Why a write was declined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UnchangedReason {
    /// The file already carried exactly these chapters.
    AlreadyCorrect,
    /// The file is not a shape this module can edit.
    UnsupportedContainer,
    /// The `moov` box is not the last box, so growing it would shift everything after it.
    ///
    /// This is the one shape where an in-place edit is not merely difficult but
    /// destructive: an M4B muxed with `+faststart` has its metadata *first*, and growing
    /// it would move the audio underneath every chunk offset in the file at once. The
    /// remedy is a re-mux, which is a one-line ffmpeg command and loses nothing — so the
    /// message names it rather than leaving the user with "not a container this can edit"
    /// and no next step.
    MoovNotLast,
    /// The new tag would not fit, and no padding can be reclaimed.
    TooLarge,
    /// The file carries a QuickTime chapter track alongside the `chpl` box, and the track
    /// cannot be rewritten.
    ///
    /// Writing only the `chpl` would succeed in the narrow sense that the box is updated,
    /// and fail in the sense that matters: players and ffprobe prefer the track, so they
    /// would keep showing the chapters that were there before. Claiming success would be
    /// worse than declining, because nobody re-checks a file they were told was fixed.
    ChapterTrackNotWritable,
}

impl std::fmt::Display for UnchangedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnchangedReason::AlreadyCorrect => f.write_str("already carries these chapters"),
            UnchangedReason::UnsupportedContainer => f.write_str("not a container this can edit"),
            UnchangedReason::TooLarge => {
                f.write_str("the chapters would not fit and no padding can be reclaimed")
            }
            UnchangedReason::MoovNotLast => f.write_str(
                // One line, on purpose: a string literal's continuation writes its
                // indentation into the output, and a remedy that arrives wrapped into
                // fragments is a remedy nobody can run.
                "the moov box is not the last box, so growing it would shift the audio and invalidate every chunk offset; re-mux with: ffmpeg -i FILE -c copy OUT (ffmpeg writes moov last by default; +faststart is what puts it first)",
            ),
            UnchangedReason::ChapterTrackNotWritable => f.write_str(
                "this file carries a QuickTime chapter track, which players read in \
                 preference to the chpl box this would write, so the edit would not be \
                 visible",
            ),
        }
    }
}

/// Padding appended to a written tag, so later edits can grow it in place.
///
/// mp3tag grows its tags by hundreds of bytes for this reason. Without it every edit that
/// adds a byte moves the audio, and a library manager that rewrites a book's chapters
/// repeatedly ends up shifting every file in it.
const PADDING: usize = 512;

/// The fixed size of an ID3v2 tag header: `ID3`, version, revision, flags, size.
const ID3V2_HEADER_LEN: usize = 10;

/// Write `chapters` into the ID3v2 tag of an MP3, in place.
///
/// The audio is never re-encoded: the bytes between the end of the old tag and the start
/// of any ID3v1 trailer are copied verbatim.
///
/// # Errors
///
/// Returns an error only when the file cannot be read or written. A file this cannot edit
/// is *not* an error — it comes back as [`WriteOutcome::Unchanged`], because "this tool
/// does not handle that container" is a different thing from "the write failed", and
/// conflating them makes a caller retry something that will never work.
pub fn write_mp3_chapters(
    path: &Path,
    chapters: &[FileChapter],
) -> Result<WriteOutcome, std::io::Error> {
    let original = std::fs::read(path)?;

    // An MP4 cannot be edited this way at all: its metadata lives inside a box tree with
    // its own offsets and sizes. Saying so is better than writing an ID3 tag in front of
    // an MP4, which produces a file nothing will play.
    if original.starts_with(b"\0\0\0\x18ftyp") || original.get(4..8) == Some(b"ftyp") {
        return Ok(WriteOutcome::Unchanged {
            reason: UnchangedReason::UnsupportedContainer,
        });
    }

    // `find_id3v2` reports where a tag's **frames** end, which is not where the audio starts
    // when the tag carries padding — and a tag written by this crate always does. Using it
    // directly makes every subsequent write treat the padding as audio, which both corrupts
    // the file and loses the padding the next write depends on.
    //
    // The declared size in the header covers the padding, so the audio begins after the run
    // of NULs that follows the frames.
    let audio_start = audio_start_of(&original);
    // The audio ends where the ID3v1 trailer begins, if there is one.
    let audio_end = original.len() - TagV1::find(&original).map_or(0, |_| id3_core::ID3V1_LEN);

    // `Tag::default()` is a **v0** tag, and v0 is not a real ID3 version: writing one
    // produces a file that `Tag::parse` then rejects with `ID3v0 is not supported`. So an
    // unparseable or absent tag becomes an empty *v2.4* tag, which is what an untagged file
    // needs.
    //
    // Deliberately lossy when a tag exists but cannot be parsed: a tag this crate cannot
    // read is one whose frames are unrecoverable anyway, and refusing to write would leave
    // a file with no chapters at all. The frames are dropped rather than the file.
    let mut tag =
        Tag::parse(&original).unwrap_or_else(|_| Tag::with_version(4).unwrap_or_default());
    let existing: Vec<(u64, String)> = tag
        .resolved_chapters()
        .unwrap_or_default()
        .iter()
        .map(|c| (u64::from(c.start_ms), c.title.clone().unwrap_or_default()))
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

    tag.frames
        .retain(|frame| !matches!(frame, id3_core::Frame::Chapter(_) | id3_core::Frame::Toc(_)));
    // Element ids must be unique across both CHAP and CTOC, so they are derived from
    // position rather than from the title: a book with two chapters called "Prologue"
    // would otherwise produce two identical ids and the second would shadow the first.
    let mut toc = id3_core::chapter::Toc::root("toc");
    for (i, chapter) in chapters.iter().enumerate() {
        let element_id = format!("ch{i}");
        let mut built = Chapter::new(
            &element_id,
            u32::try_from(chapter.start_ms).unwrap_or(u32::MAX),
        );
        if let Some(end) = chapter.end_ms {
            built = built.with_end(u32::try_from(end).unwrap_or(u32::MAX));
        }
        // An open-ended chapter declares **no** end, and `CHAP` has no way to say that: the
        // field is a plain `u32` where 0 means "zero milliseconds", i.e. an end before the
        // start. Writing 0 produces a chapter ffmpeg rejects — it logs `Chapter end time 0
        // before start 4954` and drops it — and that mutagen faithfully reports as
        // `end=0`.
        //
        // Writing `u32::MAX` instead is *accepted*, but 49 days is not what anyone means by
        // "the rest of the book", and a player shows it as the chapter's length. So the end
        // is the file's own duration, which is what an open-ended chapter means and which
        // this module can measure itself: it has already read the whole file.
        else {
            let duration = MediaProbe::probe(&original).duration_ms.unwrap_or(0);
            let start = built.start_ms;
            let end = u32::try_from(duration).unwrap_or(u32::MAX).max(start);
            built = built.with_end(end);
        }
        if !chapter.title.is_empty() {
            built = built.with_title(&chapter.title);
        }
        tag.push(id3_core::Frame::Chapter(built));
        toc.push(&element_id);
    }
    if !chapters.is_empty() {
        tag.push(id3_core::Frame::Toc(toc));
    }

    let mut tag_bytes = tag.to_bytes().map_err(std::io::Error::other)?;

    // Padding is written **inside** the tag's declared size, which is what makes it
    // reusable: the next write sees a tag whose frames end early and a NUL run after them,
    // and fills that run instead of appending another block. Writing the padding *outside*
    // the declared size — the obvious thing, since `find_id3v2` reports frames only — grows
    // the file by 512 bytes on every edit and moves the audio each time, which is the exact
    // failure the padding exists to prevent.
    let frames_len = tag_bytes.len();

    // The tag's *total* size is what should stay constant across edits, not its padding.
    //
    // `audio_start` is where the old audio began, which is the old tag's total size. Holding
    // that figure steady is what keeps the audio where it was: the new frames take whatever
    // they need, and the padding absorbs the difference. Taking `max(old, PADDING)` instead
    // would grow the tag whenever the new frames are longer than the old padding, which is
    // the common case for a chapter gaining a longer title.
    // Hold the tag's total size steady so the audio does not move.
    //
    // The reserve is only *added* when the old tag had none — a file this crate has not
    // written yet, whose tag is just its frames. Taking `max(old_total, frames + PADDING)`
    // every time would grow the tag on each edit instead: the old tag is already large
    // enough to hold the new frames, and adding a fresh reserve to it is the exact
    // accumulation this is here to prevent.
    let old_total = audio_start;
    // The old tag's own budget is the figure to hold steady. Its padding is what absorbs
    // a longer set of frames; the reserve only has to be *created* once, for a file this
    // crate has not written yet.
    // A tag that is only its header carries nothing, so it is not a budget worth reusing:
    // treating it as one leaves the new tag with no room to grow, and the *next* edit has
    // to move the audio. The condition is "no frames beyond the header", not "no tag".
    let has_frames = id3_core::find_id3v2(&original).unwrap_or(0) > ID3V2_HEADER_LEN;
    let wanted_total = if !has_frames {
        // Nothing to reuse: create the tag with room to grow.
        frames_len + PADDING
    } else {
        // An existing tag is the budget. Growing it only when the new frames genuinely do
        // not fit is what keeps an edit invisible to the audio; adding a fresh reserve on
        // every write is the accumulation this exists to prevent.
        old_total.max(frames_len)
    };

    // The declared size is the tag's total length including the header, so the size field has
    // to be rewritten after the padding is decided. Doing it by hand is the only option:
    // `to_bytes` computed the field before the padding existed.
    tag_bytes.resize(wanted_total, 0);

    // The header's size field is the tag length **excluding the 10-byte header**. Writing
    // the total instead makes every reader place the audio ten bytes late — and since this
    // crate writes the audio where the declared size says it is, each rewrite compounds
    // the error. It is the same off-by-ten that a reader gets if it trusts the field
    // without adding the header back, which is exactly what `find_id3v2` does.
    let declared = wanted_total
        .checked_sub(ID3V2_HEADER_LEN)
        .ok_or_else(|| std::io::Error::other("a tag cannot be shorter than its header"))?;
    let total = u32::try_from(declared).unwrap_or(0x0FFF_FFFF);
    if let Some(size_field) = synchsafe_encode(total) {
        // Bytes 6..10 of an ID3v2 header are the synch-safe size.
        tag_bytes[6..10].copy_from_slice(&size_field);
    }

    // The tag must not grow past what it replaces unless the file can absorb it. Growing
    // is safe — the audio simply starts later — but the old tag's trailing padding has to
    // be dropped, not duplicated, or the file gains bytes nobody asked for.
    let audio_len = audio_end.saturating_sub(audio_start);
    let mut out = Vec::with_capacity(tag_bytes.len() + audio_len);
    out.extend_from_slice(&tag_bytes);
    out.extend_from_slice(original.get(audio_start..audio_end).unwrap_or(&[]));
    // The trailer is copied verbatim so an ID3v1 tag stays where it is.
    out.extend_from_slice(original.get(audio_end..).unwrap_or(&[]));

    let delta = out.len() as i64 - original.len() as i64;
    std::fs::write(path, &out)?;

    Ok(WriteOutcome::Written {
        delta,
        in_place: delta == 0,
    })
}

/// Encode a length as ID3v2's synch-safe integer: seven bits per byte, high bit clear.
///
/// Written out because `id3-core` does not expose an encoder, and this value has to be
/// correct: the tag's declared size is what every reader uses to find the audio, and an
/// off-by-one there makes a file unplayable rather than merely untidy.
fn synchsafe_encode(value: u32) -> Option<[u8; 4]> {
    if value > 0x0FFF_FFFF {
        // Beyond the format's limit, which cannot be represented.
        return None;
    }
    Some([
        ((value >> 21) & 0x7F) as u8,
        ((value >> 14) & 0x7F) as u8,
        ((value >> 7) & 0x7F) as u8,
        (value & 0x7F) as u8,
    ])
}

/// Where the audio begins in an MP3: after the tag, padding included.
///
/// Zero for a file with no tag. For a tagged file this walks the NUL run that follows the
/// frames, because a tag's declared size covers its padding and `find_id3v2` reports frames
/// only. Skipping that run is what makes a padded tag editable at all.
fn audio_start_of(file: &[u8]) -> usize {
    let Some(frames_end) = id3_core::find_id3v2(file) else {
        return 0;
    };
    // An ID3v1 trailer is 128 bytes at EOF; padding never reaches into it.
    let limit = file
        .len()
        .saturating_sub(TagV1::find(file).map_or(0, |_| id3_core::ID3V1_LEN))
        .min(file.len());
    let mut end = frames_end.min(limit);
    while end < limit && file[end] == 0 {
        end += 1;
    }
    end
}

/// Read the chapters of an MP3, or an empty list when it carries none.
#[must_use]
pub fn read_mp3_chapters(path: &Path) -> Vec<FileChapter> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    let Ok(tag) = Tag::parse(&bytes) else {
        return Vec::new();
    };
    tag.resolved_chapters()
        .unwrap_or_default()
        .into_iter()
        .map(|c| FileChapter {
            title: c.title.unwrap_or_default(),
            start_ms: u64::from(c.start_ms),
            end_ms: c.end_ms.map(u64::from),
        })
        .collect()
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

    struct TempFile(PathBuf);

    impl TempFile {
        fn new(tag: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "audiobook-shelf-write-{}-{tag}.mp3",
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

    /// Find where `needle` sits in `haystack`.
    ///
    /// Used to locate the audio, because the tag's padding is *inside* its declared size:
    /// `find_id3v2` reports the frames only, while the audio begins after the padding too.
    /// Reading a "position" out of the tag length and asserting on bytes there checks the
    /// padding rather than the audio, which is how three of these tests first failed.
    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    /// An MP3-shaped file: a tag, then audio, then optionally an ID3v1 trailer.
    fn mp3_with(tag: &Tag, audio: &[u8], v1: bool) -> Vec<u8> {
        let mut bytes = tag.to_bytes().expect("serialises");
        bytes.extend_from_slice(audio);
        if v1 {
            let mut trailer = vec![0u8; 128];
            trailer[0..3].copy_from_slice(b"TAG");
            bytes.extend_from_slice(&trailer);
        }
        bytes
    }

    const AUDIO: &[u8] = &[0xFF, 0xFB, 0x90, 0x00, 0x11, 0x22, 0x33, 0x44];

    fn sample_chapters() -> Vec<FileChapter> {
        vec![
            FileChapter::new("One", 0, 5_000),
            FileChapter::new("Two", 5_000, 10_000),
        ]
    }

    #[test]
    fn chapters_are_written_and_read_back() {
        let empty = Tag::new();
        let file = TempFile::new("roundtrip", &mp3_with(&empty, AUDIO, false));
        let outcome = write_mp3_chapters(file.path(), &sample_chapters()).expect("writes");
        assert!(matches!(outcome, WriteOutcome::Written { .. }));

        let read = read_mp3_chapters(file.path());
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].title, "One");
        assert_eq!(read[1].start_ms, 5_000);
        assert_eq!(read[1].end_ms, Some(10_000), "and its end survives");
    }

    #[test]
    fn the_audio_is_preserved_byte_for_byte() {
        let empty = Tag::new();
        let file = TempFile::new("audio", &mp3_with(&empty, AUDIO, false));
        write_mp3_chapters(file.path(), &sample_chapters()).expect("writes");

        let after = std::fs::read(file.path()).expect("read");
        let at = find_subslice(&after, AUDIO).expect("audio still present");
        assert_eq!(
            after.get(at..at + AUDIO.len()),
            Some(AUDIO),
            "the audio must survive untouched"
        );
        // It must be the *only* copy, so a duplicated tag cannot masquerade as intact.
        assert_eq!(
            find_subslice(&after[at + 1..], AUDIO),
            None,
            "the audio appears once, not twice"
        );
    }

    #[test]
    fn an_id3v1_trailer_stays_at_the_end_of_the_file() {
        // The trailer is a fixed 128 bytes at EOF, so a write that reorders it produces a
        // file whose v1 title no longer matches its audio.
        let empty = Tag::new();
        let file = TempFile::new("v1", &mp3_with(&empty, AUDIO, true));
        write_mp3_chapters(file.path(), &sample_chapters()).expect("writes");

        let after = std::fs::read(file.path()).expect("read");
        assert!(
            TagV1::find(&after).is_some(),
            "the ID3v1 trailer must still be findable at EOF"
        );
        // The trailer is still exactly the last 128 bytes, and those 128 bytes are the
        // ones that were written, not a fresh blank block.
        let trailer_start = after.len() - id3_core::ID3V1_LEN;
        assert_eq!(
            after.get(trailer_start..trailer_start + 3),
            Some(&b"TAG"[..]),
            "the trailer's magic is still at EOF - 128"
        );
        // And the audio sits between the tag and the trailer, intact.
        let at = find_subslice(&after, AUDIO).expect("audio present");
        assert_eq!(after.get(at..at + AUDIO.len()), Some(AUDIO));
        assert!(
            at < trailer_start,
            "the audio is before the trailer, not inside it"
        );
    }

    #[test]
    fn writing_the_same_chapters_twice_changes_nothing() {
        // A library manager run twice must be a no-op the second time, or it reports work
        // it did not do.
        let empty = Tag::new();
        let file = TempFile::new("idempotent", &mp3_with(&empty, AUDIO, false));
        write_mp3_chapters(file.path(), &sample_chapters()).expect("first write");
        let after_first = std::fs::read(file.path()).expect("read");

        let outcome = write_mp3_chapters(file.path(), &sample_chapters()).expect("second write");
        assert_eq!(
            outcome,
            WriteOutcome::Unchanged {
                reason: UnchangedReason::AlreadyCorrect
            }
        );
        assert_eq!(
            std::fs::read(file.path()).expect("read"),
            after_first,
            "and the file is untouched"
        );
    }

    #[test]
    fn chapters_are_replaced_not_accumulated() {
        let empty = Tag::new();
        let file = TempFile::new("replace", &mp3_with(&empty, AUDIO, false));
        write_mp3_chapters(file.path(), &sample_chapters()).expect("first");
        let three = vec![
            FileChapter::new("A", 0, 500),
            FileChapter::new("B", 500, 1_500),
            FileChapter::new("C", 1_500, 2_000),
        ];
        write_mp3_chapters(file.path(), &three).expect("second");
        let read = read_mp3_chapters(file.path());
        assert_eq!(read.len(), 3, "replaced, not appended to: {read:?}");
        assert_eq!(read[2].title, "C");
    }

    #[test]
    fn two_chapters_with_the_same_title_get_distinct_element_ids() {
        // Element ids are derived from position. Deriving them from the title would give
        // two chapters called "Prologue" the same id, and the second would shadow the
        // first in the TOC.
        let empty = Tag::new();
        let file = TempFile::new("dupes", &mp3_with(&empty, AUDIO, false));
        let dupes = vec![
            FileChapter::new("Prologue", 0, 1_000),
            FileChapter::new("Prologue", 1_000, 2_000),
        ];
        write_mp3_chapters(file.path(), &dupes).expect("writes");

        let read = read_mp3_chapters(file.path());
        assert_eq!(read.len(), 2, "both survive: {read:?}");
    }

    #[test]
    fn an_mp4_is_declined_rather_than_corrupted() {
        // An ID3 tag in front of an MP4 produces a file nothing will play.
        let mut mp4 = vec![0u8; 12];
        mp4[4..8].copy_from_slice(b"ftyp");
        let file = TempFile::new("mp4", &mp4);
        let outcome = write_mp3_chapters(file.path(), &sample_chapters()).expect("declines");
        assert_eq!(
            outcome,
            WriteOutcome::Unchanged {
                reason: UnchangedReason::UnsupportedContainer
            }
        );
        assert_eq!(
            std::fs::read(file.path()).expect("read"),
            mp4,
            "and the file is untouched"
        );
    }

    #[test]
    fn a_file_with_no_tag_gets_one() {
        let file = TempFile::new("untagged", AUDIO);
        let outcome = write_mp3_chapters(file.path(), &sample_chapters()).expect("writes");
        assert!(matches!(outcome, WriteOutcome::Written { .. }));
        assert_eq!(read_mp3_chapters(file.path()).len(), 2);

        let after = std::fs::read(file.path()).expect("read");
        assert!(id3_core::find_id3v2(&after).is_some(), "a tag now exists");
        assert!(
            after.ends_with(AUDIO),
            "and the audio is still the last thing in the file, byte for byte"
        );
    }

    #[test]
    fn writing_no_chapters_clears_them() {
        let empty = Tag::new();
        let file = TempFile::new("clear", &mp3_with(&empty, AUDIO, false));
        write_mp3_chapters(file.path(), &sample_chapters()).expect("first");
        write_mp3_chapters(file.path(), &[]).expect("clears");
        assert!(
            read_mp3_chapters(file.path()).is_empty(),
            "and the file reads as chapterless"
        );
    }

    #[test]
    fn a_tag_with_other_frames_keeps_them() {
        // A library manager rewriting chapters must not strip the title and cover art.
        let mut tag = Tag::new();
        tag.set_text(
            id3_core::TextField::Title,
            id3_core::TextEncoding::Utf8,
            "A Title",
        );
        let file = TempFile::new("preserve", &mp3_with(&tag, AUDIO, false));

        write_mp3_chapters(file.path(), &sample_chapters()).expect("writes");

        let after = std::fs::read(file.path()).expect("read");
        let parsed = Tag::parse(&after).expect("parses");
        assert_eq!(
            parsed.text_field(id3_core::TextField::Title),
            Some("A Title"),
            "the title survived a chapter rewrite"
        );
        assert_eq!(parsed.chapters().len(), 2);
    }

    #[test]
    fn padding_leaves_room_for_the_next_edit() {
        // Without reusable padding every edit that adds a byte moves the audio, and a
        // manager that rewrites chapters repeatedly shifts every file in a book.
        //
        // The guarantee is *re-use*, not an infinite reserve: a write that fits in the
        // padding already there leaves the file exactly the same length. So the edit
        // exercised here is a chapter gaining a slightly longer title, well inside the
        // 512-byte reserve, and the assertion is that the file does not move at all.
        let empty = Tag::new();
        let file = TempFile::new("padding", &mp3_with(&empty, AUDIO, false));

        write_mp3_chapters(file.path(), &sample_chapters()).expect("first");
        let after_first = std::fs::read(file.path()).expect("read");
        write_mp3_chapters(file.path(), &sample_chapters()).expect("second, unchanged");
        assert_eq!(
            std::fs::read(file.path()).expect("read").len(),
            after_first.len(),
            "a rewrite of the same chapters must not change the file at all"
        );

        // A few bytes longer, which the reserve absorbs.
        let mut longer = sample_chapters();
        longer.push(FileChapter::new("A longer third chapter", 10_000, 12_000));
        let before = std::fs::read(file.path()).expect("read").len();
        write_mp3_chapters(file.path(), &longer).expect("third");

        assert_eq!(
            std::fs::read(file.path()).expect("read").len(),
            before,
            "a slightly longer title fits in the padding already there, so the audio did \
             not move — without reusable padding the file grows on every single edit"
        );
        assert_eq!(
            read_mp3_chapters(file.path()).len(),
            3,
            "and the new chapter is there"
        );
        assert!(
            std::fs::read(file.path()).expect("read").ends_with(AUDIO),
            "the audio is still the last thing in the file"
        );
    }
}
