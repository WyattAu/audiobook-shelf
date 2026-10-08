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

### Verification

- **`tests/library.rs` checks the chapter arithmetic against ffprobe.** Every other test in
  this crate pins a rule against constructed inputs, which cannot catch the part that
  actually breaks in practice: a book's chapters live in the individual files, so chapter 7
  of a four-file book is not at 7 seconds but at the sum of the first three files'
  durations. ffprobe reads each file independently, so the expected position of every
  chapter is arithmetic over ground truth rather than a number this crate chose.

  Confirmed non-vacuous: removing the offset accumulation fails three of the four tests,
  and restoring it passes them.

- Two defects in the harness itself, both found by this and both worth recording because
  each looked like a scanner bug from where it was noticed: ffprobe writes `start_time` and
  `duration` as **quoted strings**, so a scan for the next numeric run reads the `0` out of
  the opening quote and reports every chapter at zero; and `tags` is a **nested** object,
  so splitting the document on `{` puts a chapter's title in the following chunk. The
  assertions that caught them lived downstream of the parser, which is exactly how a harness
  defect gets mistaken for a product defect.

- Without ffmpeg on `PATH` the tests **skip and say so** on stderr. A conformance test that
  quietly passes because its oracle was missing reports coverage it does not have.

### Added

- **`write` — chapters written back into MP3s, checked by ffmpeg.** Reading chapters was
  only half of what a library manager does; the other half is fixing them. The write is
  constrained by one fact that makes it dangerous: an MP3's audio starts where its ID3v2
  tag stops, so replacing the tag with a longer one shifts every audio byte and corrupts
  the file silently — it still *plays*, from the wrong offset.
  `audiobook_shelf::write::write_mp3_chapters` bounds the tag by its declared size,
  preserves the bytes after it verbatim, keeps an ID3v1 trailer at EOF, and reuses the
  tag's padding so a second edit does not move the audio.

  A CLI verb, `--write "0:00 Prologue;5:00 One"`, rebases book-relative times onto each
  file, since a file's own `CHAP` frames are relative to that file. `--dry-run` touches
  nothing.

- **`tests/write.rs` checks the write against ffmpeg**, which is the check that matters: a
  tag this crate writes is only correct if something that did not write it can use it.
  ffprobe must read back the chapters written, the file must still decode to the same
  length, and five successive edits must leave it playable. Confirmed non-vacuous by
  reinstating the size bug below and watching three of the five fail.

### Fixed

All found by running the tool against a real library, which is how these show up:

- **The tag's declared size included its own header**, so every reader placed the audio ten
  bytes late and each rewrite compounded the error. The ID3v2 size field is the length
  *excluding* the 10-byte header. Found because ffmpeg refused to decode a file this crate
  had written; nothing in the crate's own reader objected, because it made the same mistake
  in the same direction.
- **Padding was written outside the declared size**, so it was invisible to the next write,
  which appended another 512 bytes and moved the audio again. Reusable padding is the entire
  point of padding: without it a manager rewrites a book's chapters and shifts every file in
  the book.
- **A chapter with no end was written with an end of zero.** `CHAP` has no way to say
  "open-ended": the field is a plain `u32` where 0 is an end before the start. ffmpeg
  rejects it outright — `Chapter end time 0 before start 4954` — and mutagen faithfully
  reports `end=0`. An open-ended chapter now runs to its file's measured duration, which is
  what it means.
- **The CLI's rebase collapsed open-ended chapters to an end of zero** while rebasing them
  onto their file, so the fix above was being undone on the way in. Found by the same run,
  downstream of the other.
- **`--write` treated the first field of a timestamp as minutes**, turning `5:00` into 300
  seconds and putting every chapter sixty times further into the book than asked, with no
  error to say so.

### Verified

- **The library view is checked against ffprobe, file relationships included.** The
  conformance harness checks the estate's readers file by file; nothing checked the claim
  a *library* makes, which is about relationships between files — that a book assembled
  from several files has its chapters at the offsets a player would use. `tests/
  conformance.rs` now composes ffprobe's per-file readings the way a player does and
  compares the scanner against it, on a corpus ffmpeg generates.

  Covers five shapes: three files laid out as one book, a chapterless file in the middle
  (which must not become one invented chapter), an M4B carrying both a Nero `chpl` box and
  a chapter track (two mechanisms describing the same chapters, which must not be counted
  twice), a single-file M4B, and mixed MP3/M4B in one folder.

- One defect found and fixed in the process, and it is the same defect a third time:
  ffprobe writes a **space after the colon** in its JSON, so a parser that strips for the
  opening quote without trimming first reads a duration of zero. A composed expectation
  built from it offsets every chapter by nothing, and the test then fails in a way that
  looks like a scanner bug — "scanner says 20035 ms, a player would show 0 ms" — because
  the assertion that fails is downstream of the parser. Diagnosed by asserting on the
  oracle's own output first, which is the order that finds the real fault.

### Added

- **M4B chapters, written or refused by name.** `m4b::write_m4b_chapters` writes a `chpl`
  box into an M4B whose `moov` is the last box in the file, the shape ffmpeg produces.
  Growing `moov` then changes only where the file ends; `mdat` and every chunk offset in
  it stay put, which is why the write is safe rather than reckless.

  The condition is checked, not assumed. A file whose `moov` is not last is declined as
  `UnsupportedContainer`, because growing it would invalidate every chunk offset in the
  file at once, and the failure it avoids is not a bad chapter list but an audiobook that
  plays as silence.

- **An M4B with a QuickTime chapter track is declined as
  `ChapterTrackNotWritable`, deliberately.** ffmpeg puts chapters into a QuickTime text
  track, and ffprobe and most players read the track in preference to the `chpl` box.
  Writing the box alone updates it, and leaves those players showing the chapters that
  were there before. That is worse than declining, because nobody re-checks a file they
  were told was fixed.

  An M4B **without** a track — which ffmpeg produces when the chapter list is empty — is
  written and verified visible to ffprobe, so the chpl path is proven where it is allowed
  rather than only proving refusals elsewhere.

- An M4B with an existing `chpl` under `moov > udta` is replaced, not added to: ffmpeg
  nests it there, and a writer that appended to `moov` would leave two boxes and a reader
  would see whichever it found first.

### Fixed

- **`--write` treated the first field of a timestamp as minutes**, turning `5:00` into 300
  seconds and putting every chapter sixty times further into the book than asked, with no
  error to say so. Found by running the verb against a real library: the chapters landed
  at 300 seconds, which is 60x the requested offset and looks like nothing at all in a
  report that only shows the files it touched.
- **The last chapter of a book was written with an end equal to its start.** A spec line
  with no successor has nothing to end against, and a zero-length chapter is one a player
  cannot skip past. It is written open-ended, which the M4B writer resolves to the file's
  measured duration.
- **The CLI's rebase collapsed open-ended chapters to an end of zero** while rebasing them
  onto their file, so the fix above was being undone on the way in. Same run, one step
  downstream.

### Changed

- **The QuickTime chapter track is rebuilt, so `--write` now works on real M4Bs.** The
  previous release declined every M4B ffmpeg produces, because ffmpeg puts chapters into a
  QuickTime text track, players read that track in preference to the `chpl` box, and
  rewriting the box alone left them showing the chapters that were there before. Declining
  was the honest call at the time; it was also useless for the dominant audiobook format.

  The track is now rebuilt to match the new chapter list: `stts` gains one duration per
  chapter, in the track's own timescale, read from its `mdhd` rather than guessed;
  `stsz` gains one size per sample; `stsc` becomes a single chunk; and the samples
  themselves — a length prefix, the title, and the 12-byte `encd` box ffmpeg appends,
  byte for byte — are appended after the `moov` with `stco` pointing at them.

  Appending is why this is safe. Chunk offsets are absolute file positions, so pointing
  past the moov is valid; `mdat` never moves; and the audio is asserted byte-for-byte
  unchanged in the tests.

  The `stsd` is deliberately reused rather than rebuilt: it describes the text sample
  format, which has not changed, and re-deriving it would mean guessing at fields a player
  needs. The `tkhd` duration is left alone too — it is in the *movie* timescale, and
  scaling it wrongly would be worse than leaving a field players barely consult.

  The track is identified by structure (`gmhd`, which only a text track carries) rather
  than by track id, so a file whose ids are not what ffmpeg writes still works.

### Fixed

- **`--write` ignored `.m4b` files entirely.** The CLI dispatched on extension and the M4B
  writer was never reachable from it: a book in the dominant format was silently skipped by
  the one verb that exists to fix it. Found by running the verb against an M4B and checking
  with ffprobe, which still showed the old chapters.
- **The last chapter of an M4B book now runs to the end of its file** rather than ending
  where it begins, matching the MP3 path.

### Reported, not fixed here

- `Discworld` is a book, not a disc: the disc check requires digits to follow the word, so
  a book whose name begins with those four letters is never filed inside disc 1. This is
  pinned by a test rather than left to inspection, because it is the case that decides
  whether multi-disc support works at all.
