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

    // The moov must be the last box that matters, or nothing before it can be trusted.
    let moov_index = top
        .iter()
        .position(|b| b.box_type == *b"moov")
        .ok_or_else(|| std::io::Error::other("no moov box"))?;

    // A QuickTime chapter track outranks the `chpl` box in ffprobe and in most players.
    // Writing the box alone would leave those showing the old list, so when the file has a
    // track it is rebuilt to match; if its structure is not one this module understands,
    // the write is declined rather than half-applied.
    let parsed =
        mp4_core::Mp4File::parse(&original).map_err(|e| std::io::Error::other(e.to_string()))?;
    let has_chapter_track = parsed.chapter_track_id.is_some();

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

    // Boxes after the moov are disposable if they are all `free` — and one is, when
    // this module wrote the file before, because that is where the chapter samples live.
    // Anything else after the moov means growing it would shift that box, which is declined:
    // the alternative is invalidating every chunk offset in the file at once.
    let trailing: Vec<&Box_> = top[moov_index + 1..].iter().collect();
    if trailing.iter().any(|b| b.box_type != *b"free") {
        return Ok(WriteOutcome::Unchanged {
            reason: UnchangedReason::UnsupportedContainer,
        });
    }

    // Everything before the moov is the audio and whatever came with it. It is copied
    // through untouched, because moving it is what invalidates chunk offsets.
    let prefix_len: usize = top[..moov_index]
        .iter()
        .map(|b| b.encoded_len() as usize)
        .sum();

    // The trailing `free` boxes are dropped here: they held the previous samples, and a
    // fresh one is written below sized to the new samples. Keeping them would grow the file
    // by one box on every edit, which is the accumulation padding exists to prevent.
    top.truncate(moov_index + 1);

    let mut moov = top.remove(moov_index);

    // Strip every existing `chpl` from the tree, wherever it sits — ffmpeg nests it
    // under `moov > udta`, but the format does not require that.
    strip_chpl(&mut moov);

    let file_duration_ms = probe.duration_ms.unwrap_or(0);
    let mut samples: Vec<u8> = Vec::new();
    if has_chapter_track {
        if let Err(_detail) = rebuild_chapter_track(&mut moov, chapters, file_duration_ms) {
            return Ok(WriteOutcome::Unchanged {
                reason: UnchangedReason::ChapterTrackNotWritable,
            });
        }
        samples = chapter_sample_data(chapters);
    }

    // An empty chapter list is a request to remove chapters, so no box is written.
    if !chapters.is_empty() {
        moov.children.push(chpl_box(chapters)?);
    }

    // `to_bytes` computes the size from the content, so the moov's length is right by
    // construction rather than by bookkeeping.
    let new_moov = moov.to_bytes();

    // The chapter track's samples sit after the moov rather than inside `mdat`: chunk
    // offsets are absolute file positions, so pointing past the moov is valid, and touching
    // `mdat` is the thing this module exists to avoid.
    //
    // They are wrapped in a `free` box, which a parser is required to skip, because raw
    // bytes there make the *next* parse walk into garbage — the second edit on a file
    // failed exactly that way, with a box named from the blob's own bytes declaring more
    // bytes than the file had left.
    //
    // The offset could not be written into `stco` sooner, because it depends on the length
    // of the moov that contains it. It is patched afterwards, and only the value changes,
    // so the moov's length is stable and the offset stays correct.
    let free_payload_len = samples.len() + SAMPLE_PADDING;
    let sample_offset = prefix_len as u64 + new_moov.len() as u64 + 8;
    if !samples.is_empty() {
        patch_chapter_stco(&mut moov, sample_offset)?;
    }
    let patched = moov.to_bytes();
    debug_assert_eq!(
        patched.len(),
        new_moov.len(),
        "only the offset value changed"
    );

    let mut out = Vec::with_capacity(prefix_len + patched.len() + free_payload_len + 8);
    for box_ in &top {
        out.extend_from_slice(&box_.to_bytes());
    }
    out.extend_from_slice(&patched);
    if !samples.is_empty() {
        // A `free` box holding the samples, padded so the next edit can reuse it in place.
        out.extend_from_slice(&((8 + free_payload_len) as u32).to_be_bytes());
        out.extend_from_slice(b"free");
        out.extend_from_slice(&samples);
        out.extend(std::iter::repeat_n(0u8, SAMPLE_PADDING));
    }

    let delta = out.len() as i64 - original.len() as i64;
    std::fs::write(path, &out)?;

    Ok(WriteOutcome::Written {
        delta,
        // The audio is before the moov, so it cannot have moved.
        in_place: true,
    })
}

/// Padding inside the sample `free` box, so the next edit reuses it in place.
const SAMPLE_PADDING: usize = 256;

/// Rebuild the QuickTime chapter track inside `moov` to carry `chapters`.
///
/// The track is found by its `gmhd`, which only a text track carries — an audio track
/// has `smhd` — so it is identified by structure rather than by track id, which a file
/// with two text tracks could make ambiguous.
///
/// Three things change:
///
/// * `stts` gains one duration per chapter, in the track's own timescale. A player works
///   out where chapter *i* starts by summing the durations before it, so a wrong duration
///   shifts every chapter after it;
/// * `stsz` gains one size per chapter, and the sizes are real: each sample is a length
///   prefix, the title, and a 12-byte `encd` box;
/// * `stco` is left at zero here and patched once the sample offset is known, because the
///   offset depends on the length of the moov that contains it.
///
/// The existing `stsd` is kept as-is. It describes the text sample format, which has not
/// changed, and rebuilding it would mean re-deriving the values a player needs to make
/// sense of the samples.
///
/// # Errors
///
/// Returns an error when the track is not shaped as ffmpeg writes it — no `mdia`, no
/// `mdhd`, no `stbl`, or a missing `stsd`. The caller declines rather than half-applying,
/// and the reason is the caller's to report.
fn rebuild_chapter_track(
    moov: &mut Box_,
    chapters: &[FileChapter],
    file_duration_ms: u64,
) -> Result<(), String> {
    let Some(trak) = moov.children.iter_mut().find(|c| {
        c.box_type == *b"trak"
            && c.children.iter().any(|mdia| {
                mdia.box_type == *b"mdia"
                    && mdia.children.iter().any(|minf| {
                        minf.box_type == *b"minf"
                            && minf.children.iter().any(|g| g.box_type == *b"gmhd")
                    })
            })
    }) else {
        return Err(String::from("no text track found in the moov"));
    };
    let Some(mdia) = trak.children.iter_mut().find(|c| c.box_type == *b"mdia") else {
        return Err(String::from("the text track has no mdia"));
    };

    // The timescale comes from the track's own `mdhd`, because the chapter times are in it
    // and guessing a rate puts every chapter in the wrong place.
    let timescale = {
        let Some(mdhd) = mdia.children.iter().find(|c| c.box_type == *b"mdhd") else {
            return Err(String::from("the text track has no mdhd"));
        };
        mdhd_timescale(&mdhd.payload)
            .ok_or_else(|| String::from("the mdhd has no readable timescale"))?
    };
    if timescale == 0 {
        return Err(String::from("the text track's timescale is zero"));
    }

    let Some(stbl) = mdia
        .children
        .iter_mut()
        .find(|c| c.box_type == *b"minf")
        .and_then(|minf| minf.children.iter_mut().find(|c| c.box_type == *b"stbl"))
    else {
        return Err(String::from("the text track has no stbl"));
    };

    // The chapter times are relative to the file, so they are scaled into the track's
    // timescale. Each sample's duration runs to the next chapter's start; the last runs to
    // the end of the file, which is what "the rest of the book" means here too.
    let ticks = |ms: u64| -> u64 { ms.saturating_mul(u64::from(timescale)) / 1000 };
    let mut deltas: Vec<u64> = chapters
        .windows(2)
        .map(|pair| ticks(pair[1].start_ms.saturating_sub(pair[0].start_ms)))
        .collect();
    if let Some(last) = chapters.last() {
        let remaining = file_duration_ms.saturating_sub(last.start_ms);
        deltas.push(ticks(remaining));
    }
    if deltas.is_empty() && !chapters.is_empty() {
        deltas.push(ticks(file_duration_ms));
    }

    // The sizes are per sample, because the titles differ in length.
    let sizes: Vec<u32> = chapters
        .iter()
        .map(|c| (2 + c.title.len() + 12) as u32)
        .collect();

    // The `stsd` is reused: it describes the text sample format, which is unchanged.
    let Some(stsd) = stbl.children.iter().find(|c| c.box_type == *b"stsd") else {
        return Err(String::from("the text track's stbl has no stsd"));
    };
    let stsd = stsd.clone();

    let stts_payload: Vec<u8> = {
        let mut v = vec![0u8, 0, 0, 0]; // version 0, no flags
        v.extend_from_slice(&(deltas.len() as u32).to_be_bytes());
        for delta in &deltas {
            v.extend_from_slice(&(chapters.len() as u32).to_be_bytes());
            v.extend_from_slice(&(*delta as u32).to_be_bytes());
        }
        v
    };
    let stsz_payload: Vec<u8> = {
        let mut v = vec![0u8, 0, 0, 0]; // version 0, no flags
        v.extend_from_slice(&0u32.to_be_bytes()); // not uniform
        v.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
        for size in &sizes {
            v.extend_from_slice(&size.to_be_bytes());
        }
        v
    };
    // One chunk holding every sample, because the samples are appended as one run.
    let stsc_payload: Vec<u8> = {
        let mut v = vec![0u8, 0, 0, 0];
        v.extend_from_slice(&1u32.to_be_bytes()); // entry count
        v.extend_from_slice(&1u32.to_be_bytes()); // first chunk
        v.extend_from_slice(&(chapters.len() as u32).to_be_bytes()); // samples per chunk
        v.extend_from_slice(&1u32.to_be_bytes()); // sample description index
        v
    };
    // The offset is patched once the moov's length is known.
    let stco_payload: Vec<u8> = {
        let mut v = vec![0u8, 0, 0, 0];
        v.extend_from_slice(&1u32.to_be_bytes()); // entry count
        v.extend_from_slice(&0u32.to_be_bytes()); // offset, patched below
        v
    };

    stbl.children = vec![
        stsd,
        leaf_box(*b"stts", stts_payload),
        leaf_box(*b"stsz", stsz_payload),
        leaf_box(*b"stsc", stsc_payload),
        leaf_box(*b"stco", stco_payload),
    ];

    // The track runs as long as its samples do.
    let track_ticks: u64 = deltas.iter().sum();
    set_mdhd_duration(mdia, track_ticks);

    Ok(())
}

/// The timescale in a `mdhd` payload, which differs by version.
fn mdhd_timescale(payload: &[u8]) -> Option<u32> {
    let version = *payload.first()?;
    let timescale_at = if version == 0 { 12 } else { 20 };
    let ts = payload.get(timescale_at..timescale_at + 4)?;
    Some(u32::from_be_bytes([ts[0], ts[1], ts[2], ts[3]]))
}

/// Set the `mdhd` duration to `ticks`, in the track's own timescale.
///
/// The `tkhd` duration is left alone on purpose: it is in the *movie* timescale, and
/// scaling it wrongly would be worse than leaving a field players barely consult.
fn set_mdhd_duration(mdia: &mut Box_, ticks: u64) {
    let Some(mdhd) = mdia.children.iter_mut().find(|c| c.box_type == *b"mdhd") else {
        return;
    };
    let version = mdhd.payload.first().copied().unwrap_or(0);
    let duration_at = if version == 0 { 16 } else { 24 };
    let width = if version == 0 { 4 } else { 8 };
    let end = duration_at + width;
    if mdhd.payload.len() >= end {
        let bytes = if version == 0 {
            (ticks as u32).to_be_bytes().to_vec()
        } else {
            ticks.to_be_bytes().to_vec()
        };
        mdhd.payload[duration_at..end].copy_from_slice(&bytes);
    }
}

/// Patch the chapter track's `stco` to point at `offset`.
fn patch_chapter_stco(moov: &mut Box_, offset: u64) -> Result<(), std::io::Error> {
    let Some(trak) = moov.children.iter_mut().find(|c| {
        c.box_type == *b"trak"
            && c.children.iter().any(|mdia| {
                mdia.box_type == *b"mdia"
                    && mdia.children.iter().any(|minf| {
                        minf.box_type == *b"minf"
                            && minf.children.iter().any(|g| g.box_type == *b"gmhd")
                    })
            })
    }) else {
        return Err(std::io::Error::other("no text track to patch"));
    };
    let stco = trak
        .children
        .iter_mut()
        .find(|c| c.box_type == *b"mdia")
        .and_then(|mdia| {
            mdia.children
                .iter_mut()
                .find(|c| c.box_type == *b"minf")
                .and_then(|minf| {
                    minf.children
                        .iter_mut()
                        .find(|c| c.box_type == *b"stbl")
                        .and_then(|stbl| stbl.children.iter_mut().find(|c| c.box_type == *b"stco"))
                })
        });
    let Some(stco) = stco else {
        return Err(std::io::Error::other("no stco in the text track"));
    };
    // stco payload: version (1) + flags (3) + entry count (4) + the offset (4) = 12.
    if stco.payload.len() < 12 {
        return Err(std::io::Error::other(
            "the stco is too short to hold an offset",
        ));
    }
    stco.payload[8..12].copy_from_slice(&(offset as u32).to_be_bytes());
    Ok(())
}

/// The concatenated chapter samples: a length prefix, the title, and the `encd` box.
///
/// The `encd` box is what ffmpeg appends to every chapter sample, byte for byte; the length
/// prefix counts the title only, so a sample is `2 + title + 12` bytes.
fn chapter_sample_data(chapters: &[FileChapter]) -> Vec<u8> {
    const ENCD: &[u8] = &[
        0x00, 0x00, 0x00, 0x0C, b'e', b'n', b'c', b'd', 0x00, 0x00, 0x01, 0x00,
    ];
    let mut blob = Vec::new();
    for chapter in chapters {
        let title = chapter.title.as_bytes();
        blob.extend_from_slice(&(title.len() as u16).to_be_bytes());
        blob.extend_from_slice(title);
        blob.extend_from_slice(ENCD);
    }
    blob
}

/// A box with a payload and no children.
fn leaf_box(box_type: mp4_core::BoxType, payload: Vec<u8>) -> Box_ {
    Box_ {
        box_type,
        size: Size::Fixed(0),
        size_form: SizeForm::Normal,
        extended_type: None,
        payload,
        children: Vec::new(),
    }
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
    fn a_box_that_cannot_be_disposed_of_after_the_moov_is_declined() {
        // A trailing `free` box is disposable, so a file with one is writable. A trailing
        // `meta` is not: growing the moov would shift it, and `meta` may be referenced by
        // anything. Declined, with the file untouched, because the alternative is
        // invalidating every chunk offset in the file at once.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&28u32.to_be_bytes());
        bytes.extend_from_slice(b"ftypM4A ");
        bytes.extend_from_slice(&[0, 0, 2, 0]);
        bytes.extend_from_slice(b"M4A mp42isom");
        bytes.extend_from_slice(&64u32.to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        bytes.extend_from_slice(&[0xAA; 56]);
        let moov = mp4_core::container(*b"moov", vec![]);
        bytes.extend_from_slice(&moov.to_bytes());
        // A `meta` is a real box with content, which cannot be assumed disposable.
        bytes.extend_from_slice(&16u32.to_be_bytes());
        bytes.extend_from_slice(b"meta");
        bytes.extend_from_slice(&[0u8; 12]);

        let file = TempFile::new("notlast", &bytes);
        let outcome = write_m4b_chapters(file.path(), &chapters()).expect("declines");
        assert_eq!(
            outcome,
            WriteOutcome::Unchanged {
                reason: UnchangedReason::UnsupportedContainer
            }
        );
        assert_eq!(
            std::fs::read(file.path()).expect("read"),
            bytes,
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
