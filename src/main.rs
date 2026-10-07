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

    // `--write` is opt-in and never the default: a tool that rewrites a library on the
    // strength of being pointed at it is a tool that eventually destroys one.
    if let Some(book) = args.iter().position(|a| a == "--write") {
        let name = args.get(book + 1).map_or("", String::as_str);
        if name.is_empty() {
            eprintln!("audiobook-shelf: --write needs the chapter list to write");
            return ExitCode::from(2);
        }
        return match apply_writes(&root, name, args.iter().any(|a| a == "--dry-run")) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("audiobook-shelf: {e}");
                ExitCode::from(1)
            }
        };
    }
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

/// Write a chapter list into every MP3 in a library, as chapters of one book.
///
/// The chapter times are *book-relative* and are rebased onto each file, since a file's
/// own `CHAP` frames are relative to that file. A chapter that belongs to a later file is
/// not written into an earlier one.
#[allow(clippy::expect_used, clippy::panic)]
fn apply_writes(root: &std::path::Path, spec: &str, dry_run: bool) -> Result<ExitCode, String> {
    use audiobook_shelf::naming::ParseOptions;
    use audiobook_shelf::write::FileChapter;

    // `00:10 Chapter` per line. Simple on purpose: this is a maintenance command, not a
    // parser with a documented grammar, and every convenience added here is a way to write
    // the wrong time into a file.
    let starts: Vec<(u64, String)> = spec
        .split(';')
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (stamp, title) = line
                .trim()
                .split_once(' ')
                .ok_or_else(|| format!("`{line}` is not `MM:SS Chapter`"))?;
            let parts: Vec<u64> = stamp
                .split(':')
                .map(|p| {
                    p.parse::<u64>()
                        .map_err(|_| format!("`{stamp}` is not a time"))
                })
                .collect::<Result<_, _>>()?;
            // The leading field is **seconds**, then minutes, then hours — the order
            // audiobooks and ffmpeg's own `CHAPTERX` form both use, so `5:00` is five
            // minutes and `1:02:03` is one hour two minutes three seconds.
            //
            // Reading the first field as minutes instead, which is the obvious mistake and
            // the one this originally made, turns every time into sixty times itself:
            // `5:00` becomes 300 seconds and a chapter list lands nowhere near where it
            // was asked to go, with no error to say so.
            let seconds = match parts.as_slice() {
                [s] => *s,
                [m, s] => m * 60 + s,
                [h, m, s] => h * 3600 + m * 60 + s,
                _ => return Err(format!("`{stamp}` has too many parts")),
            };
            Ok((seconds * 1000, String::from(title)))
        })
        .collect::<Result<Vec<_>, String>>()?;
    if starts.is_empty() {
        return Err("no chapters given".to_string());
    }

    // The spec carries only start times, so each chapter ends where the next one begins.
    // Leaving the end unset would write an end of zero, which is a chapter that ends
    // before it starts and which a player renders as a zero-length entry.
    let chapters: Vec<FileChapter> = starts
        .iter()
        .enumerate()
        .map(|(i, (start_ms, title))| {
            let end_ms = starts.get(i + 1).map_or(*start_ms, |(next, _)| *next);
            if end_ms == *start_ms {
                // The final chapter has nothing to end against. An end equal to the start is
                // a zero-length chapter, which a player shows as an entry that cannot be
                // skipped; an open-ended one runs to the end of the file, which is what
                // "the rest of the book" means.
                FileChapter::open_ended(title, *start_ms)
            } else {
                FileChapter::new(title, *start_ms, end_ms)
            }
        })
        .collect();

    let books = audiobook_shelf::scan::scan(root, ParseOptions::default())
        .map_err(|_| format!("cannot read {}", root.display()))?;
    let mut written = 0usize;
    for book in &books {
        if book.files.is_empty() {
            continue;
        }
        // The book's total length, from each file's own duration, so a chapter can be told
        // which file it falls in.
        let mut offsets = Vec::with_capacity(book.files.len());
        let mut running = 0u64;
        for file in &book.files {
            offsets.push(running);
            running = running.saturating_add(duration_of(&file.path));
        }
        for (i, file) in book.files.iter().enumerate() {
            let start_of_file = offsets.get(i).copied().unwrap_or(0);
            let end_of_file = offsets
                .get(i + 1)
                .copied()
                .unwrap_or(running)
                .max(start_of_file);
            // Only the chapters that fall inside this file, rebased onto it.
            let mine: Vec<FileChapter> = chapters
                .iter()
                .filter(|c| {
                    c.start_ms >= start_of_file && c.start_ms < end_of_file.max(start_of_file + 1)
                })
                .map(|c| {
                    // Rebasing has to preserve "runs to the end of the file" rather than
                    // collapse it to an end of zero, which is what `unwrap_or(0)` did: a
                    // zero-length chapter is one a player cannot skip past, and ffmpeg
                    // rejects outright.
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
            if mine.is_empty() {
                continue;
            }
            if dry_run {
                println!(
                    "would write {} chapter(s) to {}",
                    mine.len(),
                    file.path.display()
                );
            } else {
                let outcome = audiobook_shelf::write::write_mp3_chapters(&file.path, &mine)
                    .map_err(|e| format!("{}: {e}", file.path.display()))?;
                match outcome {
                    audiobook_shelf::write::WriteOutcome::Written { delta, .. } => {
                        println!("{} ({delta:+} bytes)", file.path.display())
                    }
                    audiobook_shelf::write::WriteOutcome::Unchanged { reason } => {
                        println!("{} unchanged: {reason}", file.path.display())
                    }
                    // `WriteOutcome` is `#[non_exhaustive]`. An unrecognised outcome is
                    // named rather than ignored, because a write path that silently skips
                    // an outcome is a write path that claims to have done something it
                    // did not.
                    other => println!("{} unrecognised outcome: {other:?}", file.path.display()),
                }
            }
            written += 1;
        }
    }
    if dry_run {
        println!("dry run: {written} file(s) would change");
    } else {
        println!("{written} file(s) written");
    }
    Ok(ExitCode::SUCCESS)
}

/// A file's duration in milliseconds, or zero when it cannot be read.
fn duration_of(path: &std::path::Path) -> u64 {
    std::fs::read(path).map_or(0, |bytes| {
        audiobook_core::MediaProbe::probe(&bytes)
            .duration_ms
            .unwrap_or(0)
    })
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
    audiobook-shelf <LIBRARY_DIR> --write <SPEC> [--dry-run]

WHAT IT DOES
    Treats each directory as one book, orders its files by disc and then track,
    reads each with mp4-core and id3-core, and reports what is inconsistent.

    Files play in the order the convention states: disc first, track second. A
    lexicographic sort would put track 10 between 1 and 2.

    Folder names are parsed for a series number, year, narrator, subtitle and
    Audible ASIN, following the conventions the largest audiobook server
    documents.

OPTIONS
    --write SPEC  Write chapters into every MP3 in the library. SPEC is a
                  `;`-separated list of `MM:SS Chapter`. Chapter times are
                  book-relative and rebased onto each file, since a file's own
                  CHAP frames are relative to that file.
    --dry-run     With --write, report what would change and touch nothing.
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
