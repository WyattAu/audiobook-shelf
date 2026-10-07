# audiobook-shelf

An audiobook-first library manager. Built on the WyattAu estate's format crates —
[`audiobook-core`](https://github.com/WyattAu/audiobook-core) for the domain model,
[`mp4-core`](https://github.com/WyattAu/mp4-core) and
[`id3-core`](https://github.com/WyattAu/id3-core) for the containers.

## Why

**No audio file says which files belong to the same book.** An MP3 does not record that it
is chapter 4 of 31, and an M4B has no idea what sits in the folder beside it. That fact
lives in the folder structure, because that is where the person who assembled the library
put it.

So a library manager that trusts only embedded tags produces a shelf of fragments. This
one reads the folder, which is the only place the information is.

```
$ audiobook-shelf ~/Audiobooks

~/Audiobooks/1994 - Animal Farm
    1994 — Animal Farm
    1       01
    2       02
    3       03
~/Audiobooks/Vol 2 - 1999 - A Longer Book  [, 2 disc(s)]
    1999 — #2 — A Longer Book
    d1t1    1
    d1t2    2
    d1t9    9
    d1t10   10
    d2t1    1
    d2t2    2

2 book(s), 8 file(s), 0 finding(s)
```

## The rules it applies

**Files play by disc first, then track second.** This is the rule the largest audiobook
server documents and the one that matches how a multi-disc book is actually recorded. It
is also the rule a filename sort gets wrong: a lexicographic sort puts track 10 between
track 1 and track 2, and the book breaks in the wrong place.

**A directory is a book.** `Disc`, `CD` and `Disk` subfolders belong to the folder above
them; other subfolders are nested books.

**Folder names carry the metadata the tags cannot.** Series sequence, publication year,
narrator, subtitle and Audible ASIN, following the conventions that library is laid out
for:

```text
1994 - Volume 1. Wizards First Rule {Sam Tsoutsouvas}
Vol 1 - 1994 - Wizards First Rule
1994 - Book 1 - Wizards First Rule
1 - Wizards First Rule [B002V0QK4C]
(1994) - Wizards First Rule - A Really Good Subtitle
```

Subtitle parsing is off by default, because that convention makes it an explicit setting
and defaulting it on is wrong: a dash inside a title is common, and splitting on it turns
`Death - Endless` into a book called "Death".

## What it reports

| Finding | Means |
|---|---|
| `a book folder with no audio in it` | a book whose files are missing or misfiled |
| `no title could be read from the folder name` | the name held a narrator or nothing at all |
| `no file for disc 2 track 3` | a gap, counted **within** a disc |
| `more than one file claims disc 1 track 4` | a duplicate, also within a disc |
| `no audio container signature` | an HTML error page from a failed download, named `.mp3` |
| whatever `audiobook-core`'s validator says | the assembled book does not hang together |

Gaps and duplicates carry their disc, because "track 1" of a three-disc book is three
different files. Reporting it without the disc is what makes a healthy library look
broken — which it did, until the gaps were counted per disc.

## Exit status

`0` on success, **including when the library has findings**. A library with a missing
track is still a library, and a tool that refuses to list it is less useful than one that
says so. `2` when the library cannot be read at all, which is never reported as an empty
library.

## Install

```sh
cargo install --git https://github.com/WyattAu/audiobook-shelf
```

## License

MIT OR Apache-2.0.