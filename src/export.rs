//! Turning a book's chapters into a sidecar file.
//!
//! In the library rather than the binary for one reason: a test that re-implements what a
//! CLI verb does is a test of a copy, and a copy that drifts from the original is a green
//! check over code nobody runs. The cue-sheet round-trip gate drives this function, so
//! what it verifies is what the `--export` verb does.

use crate::layout::Book;
use std::path::PathBuf;

/// The sidecar formats a book's chapters can be written out as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarFormat {
    /// ffmpeg's metadata format, whose times are book-relative and whose chapter ends are
    /// explicit.
    Ffmetadata,
    /// `CHAPTER01`/`CHAPTER01NAME` pairs.
    ChapterX,
    /// One `MM:SS.mmm Title` per line, the form players export.
    Timecode,
    /// A CDRWIN cue sheet: the format a CD rip already ships in.
    Cue,
}

impl SidecarFormat {
    /// The format named by a `--export` argument.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "ffmetadata" => Some(Self::Ffmetadata),
            "chapterx" => Some(Self::ChapterX),
            "timecode" => Some(Self::Timecode),
            "cue" => Some(Self::Cue),
            _ => None,
        }
    }

    /// Every format, in the order the usage text lists them.
    #[must_use]
    pub fn all() -> [Self; 4] {
        [Self::Ffmetadata, Self::ChapterX, Self::Timecode, Self::Cue]
    }

    /// The name a `--export` argument uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Ffmetadata => "ffmetadata",
            Self::ChapterX => "chapterx",
            Self::Timecode => "timecode",
            Self::Cue => "cue",
        }
    }

    /// The file extension the sidecar is written to.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Ffmetadata => "ffmeta.txt",
            Self::ChapterX => "chap.txt",
            Self::Timecode => "timecodes.txt",
            Self::Cue => "cue",
        }
    }
}

/// What rendering a book's chapters produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSidecar {
    /// Where the sidecar belongs: next to the book's audio.
    pub path: PathBuf,
    /// The sidecar's text.
    pub text: String,
    /// How many chapters went in.
    pub chapters: usize,
}

/// Render a book's chapters as a sidecar.
///
/// Times are book-relative, which is what a sidecar means and what `--write @` expects to
/// read back. The book's files are probed with bounded reads, so this costs kilobytes per
/// book however long the audio is.
///
/// # Errors
///
/// A file that cannot be read or probed, and — for [`SidecarFormat::Cue`] — a book of
/// more than one file, because a cue sheet's times are relative to the single audio file
/// its `FILE` line names and a multi-file book has no honest cue-sheet form. The error
/// names the formats that do.
pub fn render_sidecar(book: &Book, format: SidecarFormat) -> Result<RenderedSidecar, String> {
    use audiobook_core::sidecar::SidecarChapters;

    if book.files.is_empty() {
        return Err(format!(
            "{}: no audio to export chapters from",
            book.folder.display()
        ));
    }

    // Book-relative times, accumulated from each file's own probed duration.
    let mut chapters = SidecarChapters::default();
    let mut offset = 0u64;
    for file in &book.files {
        let mut reader =
            FileReader::open(&file.path).map_err(|e| format!("{}: {e}", file.path.display()))?;
        let probe = audiobook_core::MediaProbe::probe_source(&mut reader);
        for (start_ms, title) in &probe.chapters {
            chapters.chapters.push(audiobook_core::Chapter::new(
                title,
                offset.saturating_add(*start_ms),
                0,
            ));
        }
        offset = offset.saturating_add(probe.duration_ms.unwrap_or(0));
    }

    let text = match format {
        SidecarFormat::Ffmetadata => chapters.to_ffmetadata(),
        SidecarFormat::ChapterX => chapters.to_chapter_x(),
        SidecarFormat::Timecode => chapters.to_timecode(),
        SidecarFormat::Cue => {
            // A cue sheet names one audio file. A book of several has no honest cue-sheet
            // form — the times would be wrong from the first line — so the refusal names
            // the formats whose times are book-relative.
            if book.files.len() > 1 {
                return Err(format!(
                    "{}: a cue sheet's times are relative to one audio file and this book \\
                     has {} — export ffmetadata instead",
                    book.folder.display(),
                    book.files.len()
                ));
            }
            let audio = book
                .files
                .first()
                .map(|f| {
                    f.path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            let mut sheet = cuesheet_core::Sheet {
                title: Some(book.name.title.clone()),
                ..cuesheet_core::Sheet::default()
            };
            sheet.files.push(cuesheet_core::CueFile {
                path: audio,
                file_type: cuesheet_core::FileType::Mp3,
                tracks: chapters
                    .chapters
                    .iter()
                    .enumerate()
                    .map(|(i, c)| cuesheet_core::Track {
                        number: u8::try_from(i + 1).unwrap_or(99),
                        mode: cuesheet_core::TrackMode::Audio,
                        title: Some(c.title.clone()),
                        performer: sheet.performer.clone(),
                        songwriter: None,
                        indices: vec![cuesheet_core::Index {
                            number: 1,
                            ms: c.start_ms,
                        }],
                        gaps: Vec::new(),
                    })
                    .collect(),
            });
            sheet
                .to_text()
                .map_err(|e| format!("{}: {e}", book.folder.display()))?
        }
    };

    Ok(RenderedSidecar {
        path: book.folder.join(format!("chapters.{}", format.extension())),
        text,
        chapters: chapters.chapters.len(),
    })
}

use crate::fs_reader::FileReader;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::layout::BookFile;
    use std::path::Path;

    /// A one-file book whose audio is real silence, probed through bounded reads.
    fn book_with(dir: &Path, name: &str, files: &[&str]) -> Book {
        let folder = dir.join(name);
        std::fs::create_dir_all(&folder).expect("temp dir");
        let files: Vec<BookFile> = files
            .iter()
            .map(|f| {
                let path = folder.join(f);
                std::fs::write(&path, []).expect("write");
                BookFile::from_path(&path, dir)
            })
            .collect();
        Book {
            folder,
            name: crate::naming::BookName {
                title: name.to_string(),
                ..crate::naming::BookName::default()
            },
            series: None,
            files,
        }
    }

    #[test]
    fn a_multi_file_book_refuses_the_cue_format_and_names_the_alternative() {
        // A cue sheet's times are relative to the one audio file its FILE line names, so
        // a book of several files has no honest cue-sheet form. The refusal names the
        // format that does, because a refusal with no next step is a message wasted.
        let dir = std::env::temp_dir().join("audiobook-shelf-export-tests");
        let book = book_with(&dir, "Two Files", &["01.mp3", "02.mp3"]);
        let err = render_sidecar(&book, SidecarFormat::Cue).unwrap_err();
        assert!(err.contains("2"), "{err}");
        assert!(err.contains("ffmetadata"), "{err}");
    }

    #[test]
    fn an_empty_book_is_refused_rather_than_exported_as_empty() {
        // A sidecar with no chapters is a valid file and a useless one; refusing names
        // the real problem, which is that the book has no audio.
        let dir = std::env::temp_dir().join("audiobook-shelf-export-tests");
        let book = book_with(&dir, "Empty Book", &[]);
        let err = render_sidecar(&book, SidecarFormat::Ffmetadata).unwrap_err();
        assert!(err.contains("no audio"), "{err}");
    }

    #[test]
    fn every_named_format_has_an_extension_and_a_name_that_round_trips() {
        for format in SidecarFormat::all() {
            assert_eq!(SidecarFormat::named(format.name()), Some(format));
            assert!(!format.extension().is_empty());
        }
        assert!(SidecarFormat::named("cue").is_some());
        assert!(SidecarFormat::named("m4b").is_none());
    }
}
