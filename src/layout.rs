//! Turning a directory tree into books.
//!
//! # The decision this crate exists to make
//!
//! No audio file says which files belong to the same book. Nothing in an MP3 or an M4B
//! records that it is chapter 4 of 31. That fact lives in the folder structure, and a
//! library manager that ignores it produces a shelf of fragments — which is not a
//! hypothetical: the convention this follows was written down precisely because people
//! hit that failure.
//!
//! So this module answers three questions, in this order:
//!
//! 1. **What is one book?** A directory. Not a filename, not a tag — a directory, with
//!    `Disc`/`CD`/`Disk` subfolders inside it belonging to the same book.
//! 2. **In what order do its files play?** **By disc first and track second**, which is
//!    the rule the largest audiobook server documents and the one that matches how a
//!    multi-disc book is actually recorded. Sorting by filename does not: `10.mp3` sorts
//!    before `9.mp3`, and a single-file-per-chapter book breaks.
//! 3. **What is each file called?** Its track number, falling back to the filename.
//!
//! # Why the ordering lives here and not in the estate
//!
//! `audiobook-core` reads bytes and explicitly refuses to decide file order: it cannot
//! see a filesystem. This is the layer where that policy belongs, and stating the rule
//! explicitly is what makes it testable — "sort by disc then track" is an assertion,
//! whereas "sort by name" is a bug waiting for a track 10.

use std::path::{Path, PathBuf};

use crate::naming::{self, BookName};

/// An audio file that belongs to a book, before anything has been read from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookFile {
    /// Where it is.
    pub path: PathBuf,
    /// The disc it came from, when it was inside a `Disc` subfolder.
    pub disc: Option<u32>,
    /// The track number, from a filename or a folder name.
    pub track: Option<u32>,
    /// The filename, without its extension.
    pub stem: String,
}

impl BookFile {
    /// Read a path into a file entry, working out its disc and track.
    ///
    /// The disc comes from the *parent* folder, because that is where the convention
    /// puts it. The track is taken from the filename's leading number, which is the only
    /// place a track number reliably appears in a chapter-per-file book.
    #[must_use]
    pub fn from_path(path: &Path, library_root: &Path) -> Self {
        let parent = path.parent().unwrap_or(library_root);
        // A disc is named by the folder immediately above the file, so walk up from the
        // file rather than assuming the book folder.
        let disc = naming::disc_number(&file_name_of(parent));

        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        BookFile {
            track: leading_number(&stem),
            path: path.to_path_buf(),
            disc,
            stem,
        }
    }

    /// The ordering key: disc first, then track.
    ///
    /// A file with no disc sorts as disc 0, so an undisc'd book is one disc and does not
    /// interleave with the disc folders of another.
    #[must_use]
    pub fn order_key(&self) -> (u32, u32) {
        (self.disc.unwrap_or(0), self.track.unwrap_or(u32::MAX))
    }
}

/// One book: a folder, its parsed name, and its files in playing order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Book {
    /// The folder that defines this book.
    pub folder: PathBuf,
    /// The folder name, parsed.
    pub name: BookName,
    /// Its files, in playing order.
    pub files: Vec<BookFile>,
}

impl Book {
    /// Order files by disc, then track, then name.
    ///
    /// The final tiebreak on name is deliberate: two files claiming the same track is a
    /// defect in the library, and sorting them deterministically means the defect shows up
    /// as the *same* order on every run instead of drifting with the filesystem.
    pub fn sort_files(&mut self) {
        self.files.sort_by(|a, b| {
            a.order_key()
                .cmp(&b.order_key())
                .then_with(|| a.stem.cmp(&b.stem))
        });
    }

    /// Whether the files look like a plausible sequence with gaps.
    ///
    /// Track numbering restarts on every disc, so gaps are counted **within** a disc.
    /// Counting them across the whole book is what makes a healthy three-disc set look
    /// broken: disc 2's tracks 1 and 2 look like disc 1 already has them, and the space
    /// between disc 1's last track and disc 2's first looks like eight missing files.
    ///
    /// A book with no track numbers at all is normal — a single M4B, or a rip whose
    /// filenames carry chapter titles — so that is not a finding.
    #[must_use]
    pub fn missing_tracks(&self) -> Vec<MissingTrack> {
        let mut by_disc: Vec<(u32, Vec<u32>)> = Vec::new();
        for file in &self.files {
            let Some(track) = file.track else {
                continue;
            };
            let disc = file.disc.unwrap_or(0);
            match by_disc.iter_mut().find(|(d, _)| *d == disc) {
                Some((_, tracks)) => tracks.push(track),
                None => by_disc.push((disc, vec![track])),
            }
        }

        let mut missing = Vec::new();
        for (disc, tracks) in &mut by_disc {
            tracks.sort_unstable();
            tracks.dedup();
            // One track number in a disc says nothing about what else should be there.
            if tracks.len() < 2 {
                continue;
            }
            for window in tracks.windows(2) {
                let (Some(a), Some(b)) = (window.first(), window.get(1)) else {
                    continue;
                };
                // Strictly between: the bounds themselves are present, and reporting `b`
                // as missing would name a file that exists.
                //
                // `checked_add` because a library really can hold a file numbered
                // u32::MAX, and `+ 1` on it wraps into a very long loop.
                let Some(mut next) = a.checked_add(1) else {
                    continue;
                };
                while next < *b {
                    missing.push(MissingTrack {
                        disc: Some(*disc),
                        track: next,
                    });
                    next = match next.checked_add(1) {
                        Some(v) => v,
                        None => break,
                    };
                }
            }
        }
        missing
    }

    /// Track numbers claimed by more than one file, within a disc.
    #[must_use]
    pub fn duplicate_tracks(&self) -> Vec<MissingTrack> {
        let mut seen: Vec<(u32, u32)> = Vec::new();
        let mut duplicates = Vec::new();
        for file in &self.files {
            let Some(track) = file.track else {
                continue;
            };
            let disc = file.disc.unwrap_or(0);
            let key = (disc, track);
            if seen.contains(&key) {
                // Normalised to `Some(disc)` like `missing_tracks`, so the two are
                // comparable and a caller can match a duplicate against its gap.
                duplicates.push(MissingTrack {
                    disc: Some(disc),
                    track,
                });
            } else {
                seen.push(key);
            }
        }
        duplicates
    }
}

/// A track number on a disc that something is wrong with.
///
/// Carries the disc because a track number is only meaningful within one: "track 1" of a
/// three-disc book is three different files, and reporting it without the disc is what
/// makes a healthy library look broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingTrack {
    /// The disc, when the book has more than one.
    pub disc: Option<u32>,
    /// The track number.
    pub track: u32,
}

/// The leading run of digits in a string.
fn leading_number(text: &str) -> Option<u32> {
    let digits: String = text
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// A file name without its directory.
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Audio file extensions treated as playable, lowercased.
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "m4b", "m4a", "mp4", "aac", "ogg", "opus", "flac"];

/// Whether a path looks like an audio file this crate would open.
#[must_use]
pub fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            AUDIO_EXTENSIONS.contains(&lower.as_str())
        })
        .unwrap_or(false)
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

    fn file(disc: Option<u32>, track: Option<u32>, stem: &str) -> BookFile {
        BookFile {
            path: PathBuf::from(stem),
            disc,
            track,
            stem: stem.to_string(),
        }
    }

    #[test]
    fn files_sort_by_disc_then_track() {
        // The rule the convention states: disc first, track second.
        let mut book = Book {
            folder: PathBuf::from("/lib/Book"),
            name: BookName::default(),
            files: vec![
                file(Some(2), Some(1), "disc2-track01"),
                file(Some(1), Some(10), "disc1-track10"),
                file(Some(1), Some(2), "disc1-track02"),
                file(Some(2), Some(2), "disc2-track02"),
            ],
        };
        book.sort_files();
        let order: Vec<&str> = book.files.iter().map(|f| f.stem.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "disc1-track02",
                "disc1-track10",
                "disc2-track01",
                "disc2-track02"
            ]
        );
    }

    #[test]
    fn track_ten_sorts_after_track_nine() {
        // The reason ordering is not left to the filesystem: a lexicographic sort puts
        // track 10 between 1 and 2, which splits a book in the wrong place.
        let mut book = Book {
            folder: PathBuf::from("/lib/Book"),
            name: BookName::default(),
            files: vec![
                file(None, Some(9), "09"),
                file(None, Some(10), "10"),
                file(None, Some(1), "01"),
            ],
        };
        book.sort_files();
        let order: Vec<Option<u32>> = book.files.iter().map(|f| f.track).collect();
        assert_eq!(order, vec![Some(1), Some(9), Some(10)]);
    }

    #[test]
    fn an_undisc_book_does_not_interleave_with_a_disc_folder() {
        // An undisc'd file sorts as disc 0, so it cannot land between the discs of a
        // multi-disc book.
        let mut book = Book {
            folder: PathBuf::from("/lib/Book"),
            name: BookName::default(),
            files: vec![
                file(Some(2), Some(1), "d2t1"),
                file(None, Some(1), "loose"),
                file(Some(1), Some(1), "d1t1"),
            ],
        };
        book.sort_files();
        let order: Vec<&str> = book.files.iter().map(|f| f.stem.as_str()).collect();
        assert_eq!(order, vec!["loose", "d1t1", "d2t1"]);
    }

    #[test]
    fn a_missing_track_is_reported() {
        let book = Book {
            folder: PathBuf::from("/lib/Book"),
            name: BookName::default(),
            files: vec![
                file(None, Some(1), "01"),
                file(None, Some(4), "04"),
                file(None, Some(5), "05"),
            ],
        };
        assert_eq!(
            book.missing_tracks(),
            vec![
                MissingTrack {
                    disc: Some(0),
                    track: 2
                },
                MissingTrack {
                    disc: Some(0),
                    track: 3
                }
            ],
            "the bounds themselves are present, so they are not reported"
        );
    }

    #[test]
    fn track_numbers_restart_on_each_disc_and_gaps_are_counted_within_one() {
        // The bug this pins: counting across discs reported eight missing files and two
        // duplicates for a perfectly healthy three-disc set, because disc 2's tracks 1
        // and 2 collide with disc 1's.
        let book = Book {
            folder: PathBuf::from("/lib/Multi"),
            name: BookName::default(),
            files: vec![
                file(Some(1), Some(1), "d1t1"),
                file(Some(1), Some(2), "d1t2"),
                file(Some(2), Some(1), "d2t1"),
                file(Some(2), Some(2), "d2t2"),
            ],
        };
        assert_eq!(book.missing_tracks(), Vec::new(), "nothing is missing");
        assert_eq!(
            book.duplicate_tracks(),
            Vec::new(),
            "track 1 on disc 1 and track 1 on disc 2 are different files"
        );
    }

    #[test]
    fn a_gap_on_the_second_disc_is_reported_with_its_disc() {
        let book = Book {
            folder: PathBuf::from("/lib/Multi"),
            name: BookName::default(),
            files: vec![
                file(Some(1), Some(1), "d1t1"),
                file(Some(2), Some(1), "d2t1"),
                file(Some(2), Some(3), "d2t3"),
            ],
        };
        assert_eq!(
            book.missing_tracks(),
            vec![MissingTrack {
                disc: Some(2),
                track: 2
            }],
            "disc 2 is missing track 2, and the disc is part of the claim"
        );
    }

    #[test]
    fn two_files_claiming_one_track_on_one_disc_is_a_duplicate() {
        let book = Book {
            folder: PathBuf::from("/lib/Book"),
            name: BookName::default(),
            files: vec![file(None, Some(1), "01"), file(None, Some(1), "01 copy")],
        };
        assert_eq!(
            book.duplicate_tracks(),
            vec![MissingTrack {
                disc: Some(0),
                track: 1
            }]
        );
    }

    #[test]
    fn a_book_with_no_track_numbers_is_not_reported_as_missing_everything() {
        // A rip whose filenames carry chapter titles rather than numbers is normal, and
        // reporting tracks 1..N missing for it would be noise.
        let book = Book {
            folder: PathBuf::from("/lib/Book"),
            name: BookName::default(),
            files: vec![
                file(None, None, "Chapter One"),
                file(None, None, "Chapter Two"),
                file(None, None, "Chapter Three"),
            ],
        };
        assert!(book.missing_tracks().is_empty());
        assert!(book.duplicate_tracks().is_empty());
    }

    #[test]
    fn a_single_track_on_a_disc_is_never_reported_as_missing_others() {
        let book = Book {
            folder: PathBuf::from("/lib/Multi"),
            name: BookName::default(),
            files: vec![
                file(Some(1), Some(1), "d1t1"),
                file(Some(2), Some(1), "d2t1"),
                file(Some(3), Some(7), "d3t7"),
            ],
        };
        assert_eq!(
            book.missing_tracks(),
            Vec::new(),
            "one track per disc says nothing about what else belongs there"
        );
    }

    #[test]
    fn the_track_number_comes_from_the_leading_digits_of_the_filename() {
        assert_eq!(leading_number("01 - Opening"), Some(1));
        assert_eq!(leading_number("09"), Some(9));
        assert_eq!(leading_number("10"), Some(10));
        assert_eq!(leading_number("Chapter 3"), None);
        assert_eq!(leading_number(""), None);
        assert_eq!(leading_number("1984"), Some(1984));
        // A name that is only digits would be a volume in a folder but a track in a file.
        assert_eq!(leading_number("004"), Some(4));
    }

    #[test]
    fn the_disc_comes_from_the_folder_above_the_file() {
        let path = PathBuf::from("/lib/Book/Disc 2/07.mp3");
        let entry = BookFile::from_path(&path, Path::new("/lib"));
        assert_eq!(entry.disc, Some(2), "the parent folder names the disc");
        assert_eq!(entry.track, Some(7));
        assert_eq!(entry.stem, "07");
    }

    #[test]
    fn a_file_directly_in_the_book_folder_has_no_disc() {
        let path = PathBuf::from("/lib/Book/03.mp3");
        let entry = BookFile::from_path(&path, Path::new("/lib"));
        assert_eq!(entry.disc, None);
        assert_eq!(entry.track, Some(3));
    }

    #[test]
    fn a_book_called_discworld_is_not_mistaken_for_a_disc() {
        // The case that decides whether multi-disc support works at all: a book whose
        // folder starts with the same four letters as a disc folder.
        let path = PathBuf::from("/lib/Discworld/01.mp3");
        assert_eq!(
            BookFile::from_path(&path, Path::new("/lib")).disc,
            None,
            "Discworld is a book, not Disc 0"
        );
    }

    #[test]
    fn audio_extensions_are_recognised_case_insensitively_and_nothing_else_is() {
        for yes in [
            "a.mp3", "a.MP3", "a.m4b", "a.M4B", "a.m4a", "a.flac", "a.opus",
        ] {
            assert!(is_audio_file(Path::new(yes)), "for {yes}");
        }
        for no in ["a.txt", "a.jpg", "a.nfo", "a", "a.mp3.bak", ".mp3"] {
            assert!(!is_audio_file(Path::new(no)), "for {no}");
        }
    }
}
