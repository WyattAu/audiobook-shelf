//! `audiobook-shelf` — read a library the way a player does, and say what is wrong with it.
//!
//! The point is to be wrong *loudly*. A library tool that quietly guesses which files form
//! a book, or silently drops a chapter, produces a shelf that looks right and plays wrong.
//! Everything this binary is unsure about, it reports.

use std::path::PathBuf;
use std::process::ExitCode;

use audiobook_shelf::naming::ParseOptions;
use audiobook_shelf::scan::{self, Finding};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", usage());
        return if args.is_empty() {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        };
    }

    let root = PathBuf::from(&args[0]);
    let subtitles = args.iter().any(|a| a == "--subtitles");
    let quiet = args.iter().any(|a| a == "-q" || a == "--quiet");

    let options = ParseOptions { subtitles };
    let books = match scan::scan(&root, options) {
        Ok(b) => b,
        Err(_) => {
            eprintln!(
                "audiobook-shelf: cannot read {} — is it a directory?",
                root.display()
            );
            return ExitCode::from(2);
        }
    };

    if books.is_empty() {
        println!("no books found under {}", root.display());
        // An empty library is a legitimate answer, not a failure.
        return ExitCode::SUCCESS;
    }

    let mut total_files = 0usize;
    let mut all_findings: Vec<Finding> = Vec::new();

    for book in &books {
        total_files += book.files.len();
        if !quiet {
            let discs = book
                .files
                .iter()
                .filter_map(|f| f.disc)
                .max()
                .map(|m| format!(", {m} disc(s)"))
                .unwrap_or_default();
            println!(
                "{}{}",
                book.folder.display(),
                if discs.is_empty() {
                    String::new()
                } else {
                    format!("  [{discs}]")
                }
            );
            // A book with no readable title shows its folder name rather than a blank
            // line, so a listing always says what it is looking at.
            let label = if book.name.title.is_empty() {
                book.folder
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            } else {
                book.name.display_line()
            };
            println!("    {label}");
            // A multi-disc book's listing repeats track numbers, so the disc has to be
            // shown: "1, 1, 2, 2" reads as a duplicate rather than two discs.
            let multi_disc = book.files.iter().any(|f| f.disc.is_some());
            for file in &book.files {
                let position = match (multi_disc, file.disc, file.track) {
                    (true, Some(disc), Some(track)) => format!("d{disc}t{track:<3}"),
                    (_, Some(disc), Some(track)) => format!("d{disc}t{track:<3}"),
                    (_, _, Some(track)) => format!("{track:<6}"),
                    _ => "  -    ".to_string(),
                };
                println!("    {position}  {}", file.stem);
            }
        }
        all_findings.extend(scan::check_layout(book));
        let (_, mut from_reading) = scan::read(book);
        all_findings.append(&mut from_reading);
    }

    println!(
        "\n{} book(s), {total_files} file(s), {} finding(s)",
        books.len(),
        all_findings.len()
    );
    if !all_findings.is_empty() {
        println!("\nfindings:");
        for finding in &all_findings {
            println!("  · {}", describe(finding));
        }
        // Findings are information, not a crash: a library with a missing track is still a
        // library, and a tool that refuses to list it is less useful than one that says so.
    }
    ExitCode::SUCCESS
}

/// A track, named the way a person would: "disc 2 track 3", or just "track 3" when the
/// book has no discs. Naming disc 0 would be noise.
fn describe_track(track: &audiobook_shelf::layout::MissingTrack) -> String {
    match track.disc {
        Some(0) | None => format!("track {}", track.track),
        Some(disc) => format!("disc {disc} track {}", track.track),
    }
}

/// One finding, as a sentence a person can act on.
fn describe(finding: &Finding) -> String {
    match finding {
        Finding::LooseFile { path } => format!(
            "{}: an audio file in the library root, belonging to no book folder",
            path.display()
        ),
        Finding::EmptyBook { folder } => {
            format!("{}: a book folder with no audio in it", folder.display())
        }
        Finding::NoTitle { folder, name } => format!(
            "{}: no title could be read from the folder name {name:?}",
            folder.display()
        ),
        Finding::MissingTracks { folder, tracks } => {
            let list: Vec<String> = tracks.iter().map(describe_track).collect();
            format!("{}: no file for {}", folder.display(), list.join(", "))
        }
        Finding::DuplicateTrack { folder, tracks } => {
            let list: Vec<String> = tracks.iter().map(describe_track).collect();
            format!(
                "{}: more than one file claims {}",
                folder.display(),
                list.join(", ")
            )
        }
        Finding::Unreadable { path, detail } => {
            format!("{}: {detail}", path.display())
        }
        Finding::Inconsistent { folder, detail } => {
            format!("{}: {detail}", folder.display())
        }
        // `Finding` is `#[non_exhaustive]`, so a new variant cannot silently vanish. It
        // prints as unknown rather than being dropped, because an unrecognised finding
        // shown verbatim is recoverable and one swallowed is not.
        other => format!("unclassified finding: {other:?}"),
    }
}

fn usage() -> String {
    String::from(
        "audiobook-shelf — an audiobook-first library manager

USAGE:
    audiobook-shelf <LIBRARY_DIR> [--subtitles] [--quiet]

WHAT IT DOES
    Treats each directory as one book, orders its files by disc and then track,
    reads each with mp4-core and id3-core, and reports what is inconsistent.

    Files play in the order the convention states: disc first, track second. A
    lexicographic sort would put track 10 between 1 and 2.

    Folder names are parsed for a series number, year, narrator, subtitle and
    Audible ASIN, following the conventions the largest audiobook server
    documents.

OPTIONS
    --subtitles   Read a trailing ` - ` segment as a subtitle. Off by default,
                  because a dash inside a title is common and splitting on it
                  silently shortens titles like `Death - Endless`.
    -q, --quiet   Print only the summary and any findings.
    -h, --help    This message

EXIT STATUS
    0 on success, including when the library has findings — a library with a
    missing track is still a library. 2 when the library cannot be read, which is
    never reported as an empty library.
",
    )
}
