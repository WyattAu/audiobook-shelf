//! Scanning a library directory into books, and reading each one with the estate crates.
//!
//! # Two phases, on purpose
//!
//! [`scan`] decides what is a book and in what order its files play. It does not open a
//! single byte of audio, which is what makes the ordering rule testable on its own.
//! [`read`] then reads one book with `audiobook-core` and reports what is inconsistent
//! about it.
//!
//! The split is not tidiness. Ordering by filename and *then* discovering the files were
//! in the wrong order is how a book ends up assembled backwards, and reading every file
//! before knowing what it belongs to is how a 400 GB library takes a minute per folder.

use std::path::{Path, PathBuf};

use audiobook_core::{Chapter, MediaProbe, Title};

use crate::layout::{self, Book, BookFile, MissingTrack};
use crate::naming::{self, BookName, ParseOptions};

/// One file as read from disk: where it was, its chapters, and its duration.
type ReadPart = (PathBuf, Vec<(u64, String)>, u64);

/// Something worth reporting about a library.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Finding {
    /// A folder that looks like a book but holds no audio at all.
    EmptyBook {
        /// The folder.
        folder: PathBuf,
    },
    /// An audio file lying loose in the library root, belonging to no book folder.
    LooseFile {
        /// The file.
        path: PathBuf,
    },
    /// A book whose folder name yielded no title.
    ///
    /// One finding rather than two: "unrecognised name" and "no title" were the same
    /// problem described twice, and a library reporting both would count one broken book
    /// twice. The raw name comes along so the report can show what was actually read.
    NoTitle {
        /// The folder.
        folder: PathBuf,
        /// The folder name as written.
        name: String,
    },
    /// A gap in a book's track numbering.
    MissingTracks {
        /// The folder.
        folder: PathBuf,
        /// The track numbers with no file, each with its disc.
        tracks: Vec<MissingTrack>,
    },
    /// Two or more files claiming the same track on the same disc.
    DuplicateTrack {
        /// The folder.
        folder: PathBuf,
        /// The tracks claimed twice, each with its disc.
        tracks: Vec<MissingTrack>,
    },
    /// A file this crate could not identify as any container.
    Unreadable {
        /// The file.
        path: PathBuf,
        /// Why.
        detail: String,
    },
    /// Something `audiobook-core`'s own validator objected to.
    Inconsistent {
        /// The book.
        folder: PathBuf,
        /// Its complaint.
        detail: String,
    },
}

impl Finding {
    /// The book or file this is about, for grouping a report.
    #[must_use]
    pub fn subject(&self) -> &Path {
        match self {
            Finding::EmptyBook { folder }
            | Finding::NoTitle { folder, .. }
            | Finding::MissingTracks { folder, .. }
            | Finding::DuplicateTrack { folder, .. }
            | Finding::Inconsistent { folder, .. } => folder,
            Finding::Unreadable { path, .. } | Finding::LooseFile { path } => path,
        }
    }
}

/// Walk `root` and return one [`Book`] per book folder, each with its files in order.
///
/// Books come back sorted by path so a report is reproducible: a filesystem walk in
/// directory order differs between machines and between runs, and a report that reshuffles
/// itself cannot be read.
#[derive(Debug, Default)]
pub struct ScanError;

/// Every book under `root`, in path order.
///
/// # Errors
///
/// Returns an error if `root` cannot be read, or is not a directory. A library that
/// cannot be opened is not an empty library, and treating it as one would report every
/// book as missing.
pub fn scan(root: &Path, options: ParseOptions) -> Result<Vec<Book>, ScanError> {
    let mut books = Vec::new();
    let mut loose = Vec::new();
    walk(root, root, options, &mut books, &mut loose)?;
    books.sort_by(|a, b| a.folder.cmp(&b.folder));
    if !loose.is_empty() {
        // Reported as books with nowhere to live rather than dropped: the convention says
        // a book is a folder, so a loose file is a library mistake worth naming.
        for path in loose {
            books.push(Book {
                folder: path.clone(),
                name: BookName::default(),
                series: None,
                files: vec![BookFile::from_path(&path, root)],
            });
        }
        books.sort_by(|a, b| a.folder.cmp(&b.folder));
    }
    Ok(books)
}

/// Recurse one level, deciding which directories are books.
///
/// Three cases, and the distinction matters because getting it wrong either hides a broken
/// library or invents books that are not there:
///
/// * audio present, or disc subfolders present \u{2014} a book.
/// * subfolders that are **not** disc folders \u{2014} a container, recursed into.
/// * neither \u{2014} an empty directory, which is a book with its files missing. The
///   library root is excluded, because a root is a container by definition and reporting it
///   as an empty book would put one false finding in every run.
///
/// The root is still not skipped: audio lying loose in it is reported, because the
/// convention says a book is a folder and a loose file has no folder to be found in.
fn walk(
    dir: &Path,
    root: &Path,
    options: ParseOptions,
    out: &mut Vec<Book>,
    loose: &mut Vec<PathBuf>,
) -> Result<(), ScanError> {
    let entries = std::fs::read_dir(dir).map_err(|_| ScanError)?;

    let mut audio: Vec<PathBuf> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if layout::is_audio_file(&path) {
            audio.push(path);
        }
    }

    let is_root = dir == root;
    let disc_subdirs: Vec<&PathBuf> = subdirs
        .iter()
        .filter(|d| naming::is_disc_folder(&path_name(d)))
        .collect();

    if is_root {
        // The root is a container. Loose audio belongs to no book, and saying so is more
        // use than pretending it does.
        loose.extend(audio.iter().cloned());
    } else if !audio.is_empty() || !disc_subdirs.is_empty() || subdirs.is_empty() {
        let mut files: Vec<BookFile> = audio.iter().map(|p| BookFile::from_path(p, root)).collect();

        for sub in &disc_subdirs {
            if let Ok(sub_entries) = std::fs::read_dir(sub) {
                for entry in sub_entries.flatten() {
                    let p = entry.path();
                    if p.is_file() && layout::is_audio_file(&p) {
                        files.push(BookFile::from_path(&p, root));
                    }
                }
            }
        }

        let mut book = Book {
            folder: dir.to_path_buf(),
            name: BookName::parse_with(&path_name(dir), options),
            // The series is named by the folder above the book, which is the one place a
            // series name is written down in this convention. `None` when the book is in
            // the library root, which is no series rather than a series of one.
            series: {
                let parent = dir.parent().map(path_name);
                match parent {
                    Some(parent) if !parent.is_empty() && parent != path_name(root) => Some(parent),
                    _ => None,
                }
            },
            files,
        };
        book.sort_files();
        out.push(book);
    }

    // Descend into non-disc subfolders: nested books rather than discs.
    for sub in subdirs {
        if !naming::is_disc_folder(&path_name(&sub)) {
            walk(&sub, root, options, out, loose)?;
        }
    }
    Ok(())
}

/// A path's final component as a string.
fn path_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Everything checked about one book that does not require reading its audio.
#[must_use]
pub fn check_layout(book: &Book) -> Vec<Finding> {
    let mut findings = Vec::new();
    let folder = book.folder.clone();

    if book.files.is_empty() {
        // An empty book folder is a real defect \u{2014} a book whose files are missing or
        // misfiled \u{2014} and it is found because an empty directory is treated as a
        // book rather than skipped as an inert container.
        findings.push(Finding::EmptyBook { folder });
        return findings;
    }
    if book.name.title.is_empty() {
        findings.push(Finding::NoTitle {
            folder: folder.clone(),
            name: folder
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        });
    }

    // Grouped rather than one finding per track: a disc missing eight files is one thing
    // wrong, and eight lines of the same complaint bury everything else.
    let missing = book.missing_tracks();
    if !missing.is_empty() {
        findings.push(Finding::MissingTracks {
            folder: folder.clone(),
            tracks: missing,
        });
    }
    let duplicates = book.duplicate_tracks();
    if !duplicates.is_empty() {
        findings.push(Finding::DuplicateTrack {
            folder: folder.clone(),
            tracks: duplicates,
        });
    }
    findings
}

/// Read one book with the estate crates and report what is inconsistent about it.
///
/// Every file is probed, the durations laid out into a single [`Title`], and the estate's
/// own validator asked whether the result is coherent. Reading is where a book stops
/// being a set of paths and becomes a thing with a length.
#[must_use]
pub fn read(book: &Book) -> (Option<Title>, Vec<Finding>) {
    let mut findings = Vec::new();
    let mut parts: Vec<ReadPart> = Vec::new();

    for file in &book.files {
        // Bounded reads: a probe needs a file's header, not its gigabytes. A library
        // listing that reads every byte of every book is a listing that takes minutes
        // on the libraries this tool exists for.
        let probe = match crate::fs_reader::FileReader::open(&file.path) {
            Ok(mut reader) => MediaProbe::probe_source(&mut reader),
            Err(e) => {
                findings.push(Finding::Unreadable {
                    path: file.path.clone(),
                    detail: e.to_string(),
                });
                continue;
            }
        };
        // No container signature at all is a real finding: a file called `.mp3` that is
        // not one is how an HTML error page from a failed download ends up in a library,
        // and it plays as silence in every player.
        let Some(container) = probe.container else {
            findings.push(Finding::Unreadable {
                path: file.path.clone(),
                detail: "no audio container signature".to_string(),
            });
            continue;
        };
        // A duration is what lays out the offsets, so a file without one is reported
        // rather than silently contributing nothing to the book's length.
        if probe.duration_ms.is_none() {
            findings.push(Finding::Inconsistent {
                folder: book.folder.clone(),
                detail: format!(
                    "{}: {container:?} with no derivable duration, so it cannot be \
                     positioned within the book",
                    file.path.display()
                ),
            });
        }
        parts.push((
            file.path.clone(),
            probe.chapters.clone(),
            probe.duration_ms.unwrap_or(0),
        ));
    }

    if parts.is_empty() {
        return (None, findings);
    }

    // Concatenate across files, with each file's chapters offset by the files before it.
    // The durations own the offsets, which is why `concatenate` takes no lengths.
    let mut all_chapters: Vec<Chapter> = Vec::new();
    let mut offset_ms: u64 = 0;
    let durations: Vec<u64> = parts.iter().map(|(_, _, d)| *d).collect();
    for (_, chapters, duration_ms) in &parts {
        for (start_ms, title) in chapters {
            all_chapters.push(Chapter::new(title, offset_ms.saturating_add(*start_ms), 0));
        }
        offset_ms = offset_ms.saturating_add(*duration_ms);
    }

    let title = Title::from_flat_chapters(
        all_chapters,
        &durations,
        &book.name.title,
        book.name.narrator.as_deref().unwrap_or(""),
    );
    // The estate's own validator is the judge of whether the assembled title hangs
    // together; its complaint is reported verbatim so the reason is not lost in a
    // rephrasing.
    if let Err(complaint) = title.validate() {
        findings.push(Finding::Inconsistent {
            folder: book.folder.clone(),
            detail: complaint.to_string(),
        });
    }
    (Some(title), findings)
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

    #[test]
    fn a_book_with_no_audio_is_reported_as_empty() {
        let book = Book {
            folder: PathBuf::from("/lib/Nothing"),
            name: BookName::parse("Nothing"),
            series: None,
            files: Vec::new(),
        };
        let findings = check_layout(&book);
        assert!(
            findings.contains(&Finding::EmptyBook {
                folder: PathBuf::from("/lib/Nothing")
            }),
            "got {findings:?}"
        );
    }

    #[test]
    fn a_gap_in_the_track_numbering_is_reported() {
        let book = Book {
            folder: PathBuf::from("/lib/Book"),
            name: BookName::parse("Book"),
            series: None,
            files: vec![
                BookFile::from_path(Path::new("/lib/Book/01.mp3"), Path::new("/lib")),
                BookFile::from_path(Path::new("/lib/Book/03.mp3"), Path::new("/lib")),
            ],
        };
        assert!(check_layout(&book)
            .iter()
            .any(|f| matches!(f, Finding::MissingTracks { tracks, .. }
                if tracks.iter().any(|t| t.track == 2))));
    }

    #[test]
    fn two_files_claiming_one_track_is_reported() {
        let book = Book {
            folder: PathBuf::from("/lib/Book"),
            name: BookName::parse("Book"),
            series: None,
            files: vec![
                BookFile::from_path(Path::new("/lib/Book/01.mp3"), Path::new("/lib")),
                BookFile::from_path(Path::new("/lib/Book/01.mp3"), Path::new("/lib")),
            ],
        };
        assert!(check_layout(&book)
            .iter()
            .any(|f| matches!(f, Finding::DuplicateTrack { tracks, .. } if tracks.iter().any(|t| t.track == 1))));
    }

    #[test]
    fn a_folder_with_no_readable_title_is_reported_once() {
        let book = Book {
            folder: PathBuf::from("/lib/{Sam Tsoutsouvas}"),
            name: BookName::parse("{Sam Tsoutsouvas}"),
            series: None,
            files: vec![BookFile::from_path(
                Path::new("/lib/{Sam Tsoutsouvas}/01.mp3"),
                Path::new("/lib"),
            )],
        };
        let findings = check_layout(&book);
        assert_eq!(
            findings
                .iter()
                .filter(|f| matches!(f, Finding::NoTitle { .. }))
                .count(),
            1,
            "one broken name is one finding, not two: {findings:?}"
        );
    }

    #[test]
    fn a_clean_book_produces_no_findings() {
        let book = Book {
            folder: PathBuf::from("/lib/1994 - Animal Farm"),
            name: BookName::parse("1994 - Animal Farm"),
            series: None,
            files: vec![
                BookFile::from_path(
                    Path::new("/lib/1994 - Animal Farm/01.mp3"),
                    Path::new("/lib"),
                ),
                BookFile::from_path(
                    Path::new("/lib/1994 - Animal Farm/02.mp3"),
                    Path::new("/lib"),
                ),
            ],
        };
        assert_eq!(check_layout(&book), Vec::new());
    }

    #[test]
    fn scanning_a_missing_directory_is_an_error_not_an_empty_library() {
        // An unreadable root reported as "no books" would say every book in the library
        // is missing, which is the opposite of what happened.
        let err = scan(
            Path::new("/nonexistent-library-root"),
            ParseOptions::default(),
        );
        assert!(
            err.is_err(),
            "a missing root must not read as an empty library"
        );
    }
}
