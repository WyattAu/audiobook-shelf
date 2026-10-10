//! Scanner tests against a real directory tree.
//!
//! The unit tests in the library cover the ordering and naming rules against constructed
//! inputs, which is enough to pin the rules but not enough to show the *walk* works: that
//! a disc subfolder's files are gathered, that an empty book folder is found rather than
//! skipped, and that the library root is not mistaken for a book. Those are filesystem
//! behaviours and they only misbehave against a filesystem.
//!
//! The tree is built in a temporary directory and read back, so nothing here depends on a
//! checked-in library that could drift out of step with the rules.
//!
//! The lints the library itself runs under are relaxed here for one reason: this file is
//! test *setup*, where a failure to create a temporary directory means the test cannot
//! run and there is nothing useful to assert. Nothing in this file reaches for `unwrap`
//! or `expect` on data it is asserting about.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use audiobook_shelf::layout::{is_audio_file, Book, BookFile};
use audiobook_shelf::naming::{disc_number, BookName, ParseOptions};
use audiobook_shelf::scan::{self, Finding};

/// A unique temporary directory for one test, removed when it goes out of scope.
struct TempTree(PathBuf);

impl TempTree {
    fn new(tag: &str) -> Self {
        // The process id keeps two tests from colliding when they run at once, and the
        // tag keeps two trees in the same test from colliding.
        let base =
            std::env::temp_dir().join(format!("audiobook-shelf-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("create temp tree");
        TempTree(base)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn dir(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    /// Create a directory, making parents as needed.
    fn mkdir(&self, relative: &str) -> PathBuf {
        let path = self.dir(relative);
        std::fs::create_dir_all(&path).expect("create directory");
        path
    }

    /// Write a file, making parents as needed.
    fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let path = self.dir(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, contents).expect("write file");
        path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The smallest bytes `MediaProbe` will accept as an MP3: a sync word and a header.
///
/// A real encoder is not needed to exercise the walk, and shelling out to ffmpeg would
/// make the suite depend on a tool being installed. What matters here is the *shape* of
/// the tree, not the audio in it.
fn fake_mp3() -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFB, 0x90, 0x00];
    bytes.extend_from_slice(&[0x00; 512]);
    bytes
}

fn scan(tree: &TempTree) -> Vec<Book> {
    scan::scan(tree.path(), ParseOptions::default()).expect("scans")
}

#[test]
fn a_book_folder_becomes_one_book_with_its_files_in_order() {
    let tree = TempTree::new("basic");
    for name in ["03", "01", "02"] {
        tree.write(&format!("1994 - Animal Farm/{name}.mp3"), &fake_mp3());
    }

    let books = scan(&tree);
    assert_eq!(books.len(), 1, "one folder is one book: {books:?}");
    let book = &books[0];
    assert_eq!(book.name.title, "Animal Farm");
    assert_eq!(book.name.year, Some(1994));
    let stems: Vec<&str> = book.files.iter().map(|f| f.stem.as_str()).collect();
    assert_eq!(
        stems,
        vec!["01", "02", "03"],
        "ordered by track, not by directory order"
    );
}

#[test]
fn track_ten_comes_after_track_nine_in_a_real_directory() {
    // The whole reason this crate exists, checked against files the filesystem actually
    // holds: `read_dir` returns these in whatever order the disk gives, and a
    // lexicographic sort would interleave them.
    let tree = TempTree::new("trackten");
    for name in ["9", "10", "1", "2", "11"] {
        tree.write(&format!("Book/{name}.mp3"), &fake_mp3());
    }
    let books = scan(&tree);
    let stems: Vec<&str> = books[0].files.iter().map(|f| f.stem.as_str()).collect();
    assert_eq!(stems, vec!["1", "2", "9", "10", "11"]);
}

#[test]
fn a_disc_subfolder_belongs_to_the_book_above_it() {
    let tree = TempTree::new("disc");
    tree.write("Multi/Disc 1/01.mp3", &fake_mp3());
    tree.write("Multi/Disc 1/02.mp3", &fake_mp3());
    tree.write("Multi/Disc 2/01.mp3", &fake_mp3());

    let books = scan(&tree);
    assert_eq!(books.len(), 1, "a disc folder is not a book of its own");
    let book = &books[0];
    assert_eq!(book.files.len(), 3);
    assert_eq!(book.files[0].disc, Some(1));
    assert_eq!(book.files[2].disc, Some(2));
    assert_eq!(book.files[0].order_key(), (1, 1));
    assert_eq!(book.files[2].order_key(), (2, 1));
}

#[test]
fn a_multi_disc_book_reports_no_missing_tracks() {
    // The regression this pins: counting gaps across discs reported eight missing files
    // and two duplicates for a healthy two-disc set, because disc 2's track 1 looks like
    // disc 1 already has one.
    let tree = TempTree::new("multidisc-clean");
    for track in ["01", "02"] {
        tree.write(&format!("Multi/Disc 1/{track}.mp3"), &fake_mp3());
        tree.write(&format!("Multi/Disc 2/{track}.mp3"), &fake_mp3());
    }
    let books = scan(&tree);
    let findings = scan::check_layout(&books[0]);
    assert!(
        !findings.iter().any(|f| matches!(
            f,
            Finding::MissingTracks { .. } | Finding::DuplicateTrack { .. }
        )),
        "a healthy multi-disc book must be clean: {findings:?}"
    );
}

#[test]
fn a_gap_on_one_disc_is_reported_with_that_disc() {
    let tree = TempTree::new("multidisc-gap");
    tree.write("Multi/Disc 1/01.mp3", &fake_mp3());
    tree.write("Multi/Disc 1/02.mp3", &fake_mp3());
    tree.write("Multi/Disc 2/01.mp3", &fake_mp3());
    tree.write("Multi/Disc 2/03.mp3", &fake_mp3());

    let books = scan(&tree);
    let findings = scan::check_layout(&books[0]);
    let gaps: Vec<(Option<u32>, u32)> = findings
        .iter()
        .filter_map(|f| match f {
            Finding::MissingTracks { tracks, .. } => {
                Some(tracks.iter().map(|t| (t.disc, t.track)).collect::<Vec<_>>())
            }
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(
        gaps,
        vec![(Some(2), 2)],
        "disc 2 is missing track 2, and only that"
    );
}

#[test]
fn an_empty_book_folder_is_found_rather_than_skipped() {
    // A directory with nothing in it is inert, and skipping it hides the one defect this
    // tool most needs to surface: a book whose files went missing.
    let tree = TempTree::new("empty");
    tree.mkdir("2020 - Nothing Here");
    let books = scan(&tree);
    assert_eq!(books.len(), 1, "an empty folder is still a book: {books:?}");
    assert!(books[0].files.is_empty());
    assert!(scan::check_layout(&books[0])
        .iter()
        .any(|f| matches!(f, Finding::EmptyBook { .. })));
}

#[test]
fn the_library_root_is_not_itself_a_book() {
    let tree = TempTree::new("root");
    tree.write("Real Book/01.mp3", &fake_mp3());
    let books = scan(&tree);
    assert_eq!(books.len(), 1);
    assert_eq!(books[0].name.title, "Real Book");
    assert!(
        !books.iter().any(|b| b.folder == tree.path()),
        "the root is a container, not a book"
    );
}

#[test]
fn audio_loose_in_the_root_is_reported_rather_than_ignored() {
    let tree = TempTree::new("loose");
    tree.write("stray.mp3", &fake_mp3());
    tree.write("Real Book/01.mp3", &fake_mp3());
    let books = scan(&tree);
    let stray = books
        .iter()
        .find(|b| b.files.iter().any(|f| f.stem == "stray"));
    assert!(
        stray.is_some(),
        "a loose file belongs to no book folder, which is worth saying: {books:?}"
    );
}

#[test]
fn nested_book_folders_are_each_their_own_book() {
    // A series container holding one folder per book.
    let tree = TempTree::new("series");
    tree.write("A Series/Book 1/01.mp3", &fake_mp3());
    tree.write("A Series/Book 2/01.mp3", &fake_mp3());
    let books = scan(&tree);
    let titles: Vec<&str> = books.iter().map(|b| b.name.title.as_str()).collect();
    assert!(titles.contains(&"Book 1"), "got {titles:?}");
    assert!(titles.contains(&"Book 2"), "got {titles:?}");
    assert_eq!(books.len(), 2, "the series folder itself is not a book");
}

#[test]
fn a_file_that_is_not_audio_is_reported() {
    let tree = TempTree::new("notaudio");
    tree.write("Broken/01.mp3", b"<html>404</html>");
    let books = scan(&tree);
    let (_, findings) = scan::read(&books[0]);
    assert!(
        findings
            .iter()
            .any(|f| matches!(f, Finding::Unreadable { .. })),
        "an HTML error page named .mp3 is a library defect, not audio: {findings:?}"
    );
}

#[test]
fn non_audio_files_in_a_book_folder_are_ignored() {
    // Cover art, .nfo files and stray text are normal in an audiobook folder.
    let tree = TempTree::new("extras");
    tree.write("Book/01.mp3", &fake_mp3());
    tree.write("Book/cover.jpg", b"\xFF\xD8\xFF");
    tree.write("Book/notes.nfo", b"metadata");
    let books = scan(&tree);
    assert_eq!(books.len(), 1);
    assert_eq!(
        books[0].files.len(),
        1,
        "only audio is a book file: {:?}",
        books[0].files
    );
}

#[test]
fn an_empty_library_is_not_an_error() {
    let tree = TempTree::new("emptylib");
    let books = scan(&tree);
    assert!(books.is_empty());
}

#[test]
fn a_missing_library_is_an_error_and_not_an_empty_one() {
    let err = scan::scan(
        Path::new("/nonexistent-audiobook-library-root"),
        ParseOptions::default(),
    );
    assert!(err.is_err());
}

#[test]
fn two_books_with_the_same_name_are_both_kept() {
    // Two editions of the same book is a normal library state, and collapsing them would
    // hide one.
    let tree = TempTree::new("dupenames");
    tree.write("Book/01.mp3", &fake_mp3());
    tree.mkdir("Book");
    tree.write("Book (unabridged)/01.mp3", &fake_mp3());
    let books = scan(&tree);
    assert_eq!(books.len(), 2);
}

#[test]
fn the_ordering_is_the_same_on_every_run() {
    // A report that reshuffles between runs cannot be read, so the ordering has to be a
    // function of the tree rather than of the filesystem's mood.
    let tree = TempTree::new("stable");
    for name in ["05", "01", "04", "02", "03", "10", "09"] {
        tree.write(&format!("Book/{name}.mp3"), &fake_mp3());
    }
    let first: Vec<Vec<String>> = scan(&tree)
        .iter()
        .map(|b| b.files.iter().map(|f| f.stem.clone()).collect())
        .collect();
    for _ in 0..3 {
        let again: Vec<Vec<String>> = scan(&tree)
            .iter()
            .map(|b| b.files.iter().map(|f| f.stem.clone()).collect())
            .collect();
        assert_eq!(first, again, "ordering must not depend on read_dir order");
    }
}

#[test]
fn a_disc_folder_named_like_a_book_is_still_a_disc() {
    let tree = TempTree::new("discworld");
    tree.write("Discworld/Disc 1/01.mp3", &fake_mp3());
    let books = scan(&tree);
    assert_eq!(books.len(), 1, "Discworld is a book, not a disc");
    assert_eq!(books[0].name.title, "Discworld");
    assert_eq!(books[0].files.len(), 1);
    assert_eq!(books[0].files[0].disc, Some(1));
}

#[test]
fn the_helpers_agree_about_what_is_audio() {
    assert!(is_audio_file(Path::new("x.mp3")));
    assert!(is_audio_file(Path::new("x.M4B")));
    assert!(!is_audio_file(Path::new("x.jpg")));
    assert_eq!(disc_number("Disc 2"), Some(2));
    assert_eq!(disc_number("Discworld"), None);

    // And the parsed name of a real folder, end to end.
    let name = BookName::parse("1994 - Book 1 - Wizards First Rule {Sam Tsoutsouvas}");
    assert_eq!(name.title, "Wizards First Rule");
    assert_eq!(name.series_index, Some(1));
    assert_eq!(name.year, Some(1994));
    assert_eq!(name.narrator.as_deref(), Some("Sam Tsoutsouvas"));

    // A book file, parsed the same way a file entry is.
    let entry = BookFile::from_path(Path::new("/lib/Multi/Disc 2/07.mp3"), Path::new("/lib"));
    assert_eq!((entry.disc, entry.track), (Some(2), Some(7)));
}

#[test]
fn a_series_folder_names_the_books_inside_it() {
    // `Author/Series/Book 1` is the layout the convention documents, and the series name is
    // written nowhere else: a book folder's own name carries a sequence number and never
    // the series it belongs to.
    let tree = TempTree::new("seriesname");
    tree.write("Sword of Truth/1 - Wizards First Rule/01.mp3", &fake_mp3());
    tree.write("Sword of Truth/2 - Stone of Tears/01.mp3", &fake_mp3());

    let books = scan(&tree);
    assert_eq!(books.len(), 2);
    let series: Vec<&str> = books
        .iter()
        .map(|b| b.series.as_deref().expect("series is named"))
        .collect();
    assert!(series.iter().all(|s| *s == "Sword of Truth"), "{series:?}");
    // And the sequence numbers are the books' own, not the series'.
    let mut titles: Vec<(Option<u32>, &str)> = books
        .iter()
        .map(|b| (b.name.series_index, b.name.title.as_str()))
        .collect();
    titles.sort();
    assert_eq!(
        titles,
        vec![(Some(1), "Wizards First Rule"), (Some(2), "Stone of Tears")]
    );
}

#[test]
fn a_book_directly_in_the_library_root_has_no_series() {
    // `None` rather than a series of one: the root is a container, not a series, and
    // inventing a series from it would group unrelated books together.
    let tree = TempTree::new("rootseries");
    tree.write("Standalone/01.mp3", &fake_mp3());
    let books = scan(&tree);
    assert_eq!(books.len(), 1);
    assert!(books[0].series.is_none(), "the root is not a series");
}

#[test]
fn a_nested_series_container_names_the_immediate_parent() {
    // `Author/Series/Book`: the series is the folder directly above the book, not the
    // author two levels up. Two levels of container must not be collapsed into one name.
    let tree = TempTree::new("nested");
    tree.write(
        "Terry Goodkind/Sword of Truth/1 - Wizards First Rule/01.mp3",
        &fake_mp3(),
    );
    let books = scan(&tree);
    assert_eq!(books.len(), 1);
    assert_eq!(
        books[0].series.as_deref(),
        Some("Sword of Truth"),
        "the immediate parent, not the author"
    );
}

#[test]
fn a_series_folder_holding_discs_still_names_the_series() {
    // Multi-disc books inside a series folder: the disc folders are the book's, and the
    // series folder is two levels up from the audio. Getting this wrong reports either the
    // series as a book or the book with no series.
    let tree = TempTree::new("seriesdisc");
    tree.write(
        "Sword of Truth/1 - Wizards First Rule/Disc 1/01.mp3",
        &fake_mp3(),
    );
    tree.write(
        "Sword of Truth/1 - Wizards First Rule/Disc 2/01.mp3",
        &fake_mp3(),
    );
    let books = scan(&tree);
    assert_eq!(books.len(), 1, "one book across two discs");
    assert_eq!(books[0].series.as_deref(), Some("Sword of Truth"));
    assert_eq!(books[0].files.len(), 2);
}

#[test]
fn a_cue_sheet_naming_missing_audio_is_a_finding() {
    // A rip's `.cue` is its chapter list. A sheet naming audio that has been moved or
    // deleted is a book whose chapters cannot be applied or exported, and a library
    // report is where that should be named - not the moment someone finally tries.
    let tree = TempTree::new("cuegone");
    tree.write("Rip/book.mp3", &fake_mp3());
    tree.write(
        "Rip/book.cue",
        b"FILE \"book.mp3\" MP3\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 01 05:00:00\n",
    );
    tree.write(
        "Rip/gone.cue",
        b"FILE \"deleted.mp3\" MP3\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n",
    );

    let books = scan(&tree);
    assert_eq!(books.len(), 1);
    let findings = audiobook_shelf::scan::check_layout(&books[0]);
    let cue_finding = findings
        .iter()
        .find(|f| matches!(f, Finding::CueNamesMissingFile { .. }))
        .expect("the broken sheet is named");
    let Finding::CueNamesMissingFile { sheet, missing, .. } = cue_finding else {
        unreachable!()
    };
    assert!(sheet.ends_with("gone.cue"), "{sheet:?}");
    assert_eq!(missing, &["deleted.mp3".to_string()]);
    // And the good sheet is not named: one defect, one finding.
    assert_eq!(
        findings
            .iter()
            .filter(|f| matches!(f, Finding::CueNamesMissingFile { .. }))
            .count(),
        1
    );
}

#[test]
fn an_unparsable_cue_sheet_is_a_finding_with_the_reason() {
    let tree = TempTree::new("cuejunk");
    tree.write("Rip/book.mp3", &fake_mp3());
    tree.write(
        "Rip/junk.cue",
        b"FILE \"book.mp3\" MP3\n  NOT_A_CUE_COMMAND x\n",
    );

    let books = scan(&tree);
    let findings = audiobook_shelf::scan::check_layout(&books[0]);
    let Finding::UnparsableCue { sheet, detail } = findings
        .iter()
        .find(|f| matches!(f, Finding::UnparsableCue { .. }))
        .expect("the junk sheet is named")
    else {
        unreachable!()
    };
    assert!(sheet.ends_with("junk.cue"));
    assert!(detail.contains("NOT_A_CUE_COMMAND"), "{detail}");
}

#[test]
fn a_cue_sheet_whose_audio_is_present_is_no_finding() {
    let tree = TempTree::new("cueok");
    tree.write("Rip/book.mp3", &fake_mp3());
    tree.write(
        "Rip/book.cue",
        b"FILE \"book.mp3\" MP3\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n",
    );
    let books = scan(&tree);
    let findings = audiobook_shelf::scan::check_layout(&books[0]);
    assert!(
        !findings.iter().any(|f| matches!(
            f,
            Finding::CueNamesMissingFile { .. } | Finding::UnparsableCue { .. }
        )),
        "a sound rip is not a defect: {findings:?}"
    );
}
