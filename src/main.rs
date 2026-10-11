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
    if let Some(at) = args.iter().position(|a| a == "--export") {
        let format = args.get(at + 1).map(String::as_str).unwrap_or("");
        return match export(&root, format, args.iter().any(|a| a == "--dry-run")) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("audiobook-shelf: {e}");
                ExitCode::from(1)
            }
        };
    }
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
            if let Some(series) = &book.series {
                println!("    [{series}]");
            }
            // A rip's cue sheet is its chapter list, and naming it — with how many
            // tracks — is how a user knows the book's chapters are one edit away.
            if let Ok(entries) = std::fs::read_dir(&book.folder) {
                let mut sheets: Vec<std::path::PathBuf> = entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.eq_ignore_ascii_case("cue"))
                            .unwrap_or(false)
                    })
                    .collect();
                sheets.sort();
                for sheet in sheets {
                    match std::fs::read_to_string(&sheet)
                        .map_err(|e| e.to_string())
                        .and_then(|t| cuesheet_core::Sheet::parse(&t).map_err(|e| e.to_string()))
                    {
                        Ok(parsed) => {
                            let tracks: usize = parsed.files.iter().map(|f| f.tracks.len()).sum();
                            println!(
                                "    [{tracks} track(s) from chapters: {}]",
                                sheet
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            );
                        }
                        Err(e) => println!("    [cue sheet unreadable: {e}]"),
                    }
                }
            }
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

/// Export every book's chapters as a sidecar next to its audio.
fn export(root: &std::path::Path, format: &str, dry_run: bool) -> Result<ExitCode, String> {
    use audiobook_shelf::export::{render_sidecar, SidecarFormat};

    let Some(format) = SidecarFormat::named(format) else {
        let names: Vec<&str> = SidecarFormat::all().iter().map(|f| f.name()).collect();
        return Err(format!(
            "unknown sidecar format `{format}`; expected {}",
            names.join(", ")
        ));
    };

    let books = audiobook_shelf::scan::scan(root, ParseOptions::default())
        .map_err(|_| format!("cannot read {}", root.display()))?;
    let mut written = 0usize;
    for book in &books {
        if book.files.is_empty() {
            continue;
        }
        let rendered = render_sidecar(book, format)?;
        if dry_run {
            println!("would write {}", rendered.path.display());
        } else {
            std::fs::write(&rendered.path, &rendered.text)
                .map_err(|e| format!("{}: {e}", rendered.path.display()))?;
            println!("{}", rendered.path.display());
        }
        written += 1;
    }
    if dry_run {
        println!("dry run: {written} sidecar(s) would be written");
    } else {
        println!("{written} sidecar(s) written");
    }
    Ok(ExitCode::SUCCESS)
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
    use audiobook_shelf::spec::{plan_writes, resolve_spec};

    let starts = resolve_spec(spec, root)?;
    if starts.is_empty() {
        return Err("no chapters given".to_string());
    }

    let books = audiobook_shelf::scan::scan(root, ParseOptions::default())
        .map_err(|_| format!("cannot read {}", root.display()))?;
    let mut written = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for book in &books {
        if book.files.is_empty() {
            continue;
        }
        let durations = audiobook_shelf::spec::book_durations(book);
        for (file, mine) in plan_writes(book, &durations, &starts) {
            if dry_run {
                println!(
                    "would write {} chapter(s) to {}",
                    mine.len(),
                    file.path.display()
                );
                written += 1;
                continue;
            }

            // The container decides which writer runs: an MP3 takes an ID3v2 tag and an MP4
            // takes boxes, and writing the wrong one produces a file that plays as silence.
            // Extension is the test because that is what a library is organised by, and the
            // writers themselves re-check the bytes.
            let is_mp4 = file
                .path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| {
                    let e = e.to_ascii_lowercase();
                    e == "m4b" || e == "m4a" || e == "mp4"
                })
                .unwrap_or(false);
            let outcome = if is_mp4 {
                audiobook_shelf::m4b::write_m4b_chapters(&file.path, &mine)
            } else {
                audiobook_shelf::write::write_mp3_chapters(&file.path, &mine)
            };
            // A failure is recorded and the pass continues, because a book with twenty
            // files where the third refuses must not leave the other seventeen unwritten:
            // one refusal per run is how a fix takes twenty runs to converge.
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(e) => {
                    failures.push(format!("{}: {e}", file.path.display()));
                    continue;
                }
            };
            match outcome {
                audiobook_shelf::write::WriteOutcome::Written { delta, .. } => {
                    println!("{} ({delta:+} bytes)", file.path.display())
                }
                audiobook_shelf::write::WriteOutcome::Unchanged { reason } => {
                    println!("{} unchanged: {reason}", file.path.display())
                }
                // `WriteOutcome` is `#[non_exhaustive]`. An unrecognised outcome is named
                // rather than ignored, because a write path that silently skips an outcome
                // is a write path that claims to have done something it did not.
                other => println!("{} unrecognised outcome: {other:?}", file.path.display()),
            }
            written += 1;
        }
    }
    if dry_run {
        println!("dry run: {written} file(s) would change");
    } else {
        println!("{written} file(s) written");
    }
    if !failures.is_empty() {
        println!("\n{} file(s) could not be written:", failures.len());
        for failure in &failures {
            println!("  \u{b7} {failure}");
        }
        // Exit non-zero so a scripted fix knows the library is not clean, while every
        // other file has still been written.
        return Ok(ExitCode::from(1));
    }
    Ok(ExitCode::SUCCESS)
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
        Finding::CueNamesMissingFile {
            folder,
            sheet,
            missing,
        } => format!(
            "{}: {} names audio this folder does not have: {} — the rip is broken, \
             and the sheet is the book's chapter list",
            folder.display(),
            sheet.display(),
            missing.join(", ")
        ),
        Finding::UnparsableCue { sheet, detail } => {
            format!("{}: {detail}", sheet.display())
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
    audiobook-shelf <LIBRARY_DIR> --export <FORMAT>

WHAT IT DOES
    Treats each directory as one book, orders its files by disc and then track,
    reads each with mp4-core and id3-core, and reports what is inconsistent.

    Files play in the order the convention states: disc first, track second. A
    lexicographic sort would put track 10 between 1 and 2.

    Folder names are parsed for a series number, year, narrator, subtitle and
    Audible ASIN, following the conventions the largest audiobook server
    documents.

OPTIONS
    --write SPEC  Write chapters into every audio file in the library. SPEC is
                  a `;`-separated list of `MM:SS Chapter`, or `@FILE` to apply
                  a sidecar in any supported format. Chapter times are
                  book-relative and rebased onto each file.
    --export FMT  Write each book's chapters as a sidecar next to its audio:
                  `ffmetadata`, `chapterx`, `timecode` or `cue`. Round-trips
                  with `--write @`. A cue sheet names one audio file, so it
                  is refused for a book of several.
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
