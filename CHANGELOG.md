# Changelog

All notable changes to this project are documented here. Format: [Keep a
Changelog](https://keepachangelog.com/) — versions follow [semver](https://semver.org/).

## [0.1.0] - 2026-10-07

First release: the layer that decides which files are one book.

### Added

- **`naming` — folder names, parsed.** A library's books are named by their folders,
  because nothing in an audio file says which files belong together. The grammar follows
  the one the largest audiobook server documents, so a library laid out for it reads
  correctly here: series sequence (`1 - Title`, `1994 - Book 1 - Title`,
  `1994 - Volume 1. Title`), publication year (bare or parenthesised), narrator in braces,
  subtitle, and Audible ASIN in brackets.

  Subtitle parsing is **off by default**, because the convention makes it an explicit
  setting and defaulting it on is wrong: `-` inside a title is common, and splitting on it
  silently shortens `Death - Endless` into a book called "Death".

- **`layout` — the ordering rule, stated.** Files play **by disc first and track second**,
  the rule the convention documents and the one that matches how a multi-disc book is
  recorded. Sorting by filename does not: a lexicographic sort puts track 10 between 1 and
  2, and a book breaks in the wrong place. Ordering lives here rather than in
  `audiobook-core` because that crate sees bytes and cannot see a filesystem.

- **`scan` — the walk, and what is inconsistent with it.** Audio directly in a folder or
  in its `Disc`/`CD`/`Disk` subfolders makes one book; subfolders that are not disc folders
  are nested books; an empty directory is a book whose files are missing and is reported
  as such rather than skipped as inert. Files are read with `audiobook-core`, chapters are
  laid out across files, and the estate's own validator is asked whether the result hangs
  together.

### Fixed

Found by running the tool against a library built with ffmpeg, which is the only way these
show up:

- **Track gaps were counted across discs.** Track numbering restarts on every disc, so a
  healthy three-disc set reported eight missing files and two duplicate tracks — disc 2's
  tracks 1 and 2 colliding with disc 1's. Gaps and duplicates are now counted and named
  *within* a disc.
- **The library root was reported as an empty book.** A root is a container by definition,
  so every run carried one false finding.
- **An empty book folder was never visited**, so the one defect this tool most needs to
  surface — a book whose files went missing — was invisible.
- **A folder named only for its position (`Book 1`) reported no title**, because a
  sequence number was being counted as evidence that something had been read out of the
  name.

### Reported, not fixed here

- `Discworld` is a book, not a disc: the disc check requires digits to follow the word, so
  a book whose name begins with those four letters is never filed inside disc 1. This is
  pinned by a test rather than left to inspection, because it is the case that decides
  whether multi-disc support works at all.
