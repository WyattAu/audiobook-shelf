//! Parsing the folder names a library is actually named with.
//!
//! # Why a folder name is worth parsing
//!
//! Nothing in an audio file says which files belong to the same book, and nothing in an
//! audio file says which book in a series it is. That information lives in the folder,
//! because that is where the person who assembled the library put it. A scanner that
//! ignores folder names and trusts only embedded tags gets a third of a library right —
//! the rest sorts under whatever the tag happened to say, which is often the track title.
//!
//! This module is therefore not a convenience. It is the layer where "one folder is one
//! book" becomes decidable, and it follows the naming grammar that the largest audiobook
//! server documents, so a library laid out for one reads correctly here.
//!
//! # The grammar
//!
//! From the Audiobookshelf book-library documentation, which is the most explicit
//! published statement of these conventions:
//!
//! ```text
//! 1994 - Volume 1. Wizards First Rule {Sam Tsoutsouvas}
//! Vol 1 - 1994 - Wizards First Rule
//! 1994 - Book 1 - Wizards First Rule
//! 1 - Wizards First Rule [B002V0QK4C]
//! (1994) - Wizards First Rule - A Really Good Subtitle {Sam Tsoutsouvas}
//! 1994 - Wizards First Rule - Volume 1
//! ```
//!
//! with `Disc 1`, `CD2`, `Disk 3` subfolders for multi-disc books, ordered **by disc
//! first and track second**.
//!
//! # What this deliberately does not do
//!
//! It does not decide *which* folder is a book, does not touch the filesystem, and does
//! not fall back to a guess. A folder name that matches no known shape yields
//! [`BookName::Unknown`] with the raw text preserved, because a scanner that invents a
//! title is how "Disc 2" ends up in a library as a book by that name.

use std::fmt;

/// How much of a folder name's structure to read.
///
/// The one option that matters is subtitles, because the convention this follows makes
/// subtitle parsing an explicit setting rather than an automatic one. Defaulting it on
/// would be convenient and wrong: `-` inside a title is common, and `Death - Endless`
/// split automatically becomes a book called "Death" with a subtitle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ParseOptions {
    /// Treat a trailing ` - ` segment as a subtitle.
    pub subtitles: bool,
}

/// A folder name parsed into the parts a library index needs.
///
/// Every field is optional because a real name carries whatever the person who named it
/// felt like writing. The sequence fields are `Option<u32>` rather than `u32` because
/// "Book 7" and "Book zero" are different claims, and only one of them is a volume
/// number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookName {
    /// The series this book belongs to.
    ///
    /// Always `None` from a folder name: the convention carries a sequence *number* but
    /// never the series name, so series membership has to come from matching books
    /// against each other rather than from any one folder. Populating it from a name would
    /// mean inventing it.
    pub series: Option<String>,
    /// This book's position in the series.
    pub series_index: Option<u32>,
    /// The title, with any series, year, narrator or subtitle removed.
    pub title: String,
    /// The subtitle, where the name separates one with ` - `.
    pub subtitle: Option<String>,
    /// Publication year, from a bare four-digit group or a parenthesised one.
    pub year: Option<u16>,
    /// The narrator, from a `{...}` group.
    pub narrator: Option<String>,
    /// The Audible ASIN, from a `[B002V0QK4C]` group.
    pub asin: Option<String>,
    /// How much of the name the parser understood.
    pub shape: NameShape,
}

/// Which naming convention a folder name follows.
///
/// Recorded rather than inferred, because a name can match more than one and the caller
/// may want to know whether the sequence came from the leading token or from a
/// `Volume 1` fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum NameShape {
    /// Just a title, with nothing else recognisable.
    #[default]
    Plain,
    /// A leading sequence token such as `1 - Title` or `1. Title`.
    LeadingSequence,
    /// A sequence named by a word, as in `Volume 1` or `Book 1`.
    NamedSequence,
    /// Only a year and a title.
    YearTitle,
    /// No title could be read from the name.
    ///
    /// Not "the name was unusual": a bare title is one of the documented conventions and
    /// is [`NameShape::Plain`]. This is the case where the name held nothing but a
    /// narrator, an ASIN, or delimiters, so there is no title to show a user.
    NoTitle,
}

impl BookName {
    /// Parse a folder name, with subtitle parsing off.
    ///
    /// Never fails. A name matching no convention becomes [`NameShape::Unknown`] with
    /// the whole string as the title, because refusing to name a book is worse than
    /// naming it by its folder and letting the caller see that is what happened.
    #[must_use]
    pub fn parse(folder: &str) -> Self {
        Self::parse_with(folder, ParseOptions::default())
    }

    /// Parse a folder name with explicit options.
    #[must_use]
    pub fn parse_with(folder: &str, options: ParseOptions) -> Self {
        let mut name = BookName::default();
        let mut rest = folder.trim().to_string();
        rest = take_narrator(&rest, &mut name);
        rest = take_asin(&rest, &mut name);
        rest = take_year(&rest, &mut name);

        let segments: Vec<String> = rest
            .split(" - ")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // A sequence token can sit anywhere, but only a *leading* bare number counts:
        // `1. Title` is volume 1, while `Chapter 2` in the middle of a title is not.
        let mut remaining: Vec<String> = Vec::new();
        for (i, segment) in segments.iter().enumerate() {
            if name.series_index.is_none() {
                if let Some((index, leftover)) = take_named_sequence(segment) {
                    name.series_index = Some(index);
                    name.shape = NameShape::NamedSequence;
                    if !leftover.is_empty() {
                        remaining.push(leftover);
                    }
                    continue;
                }
                if i == 0 {
                    // `1 - Title` splits on the separator, leaving a bare `1` that is the
                    // sequence; but a folder called `1984` is a book called 1984, because
                    // nothing follows it to say which. Only the bare case depends on
                    // another segment existing \u{2014} `1. Title` needs none, since the
                    // dot is part of its own segment.
                    let allow_bare = segments.len() > 1;
                    if let Some((index, leftover)) = take_leading_sequence(segment, allow_bare) {
                        name.series_index = Some(index);
                        name.shape = NameShape::LeadingSequence;
                        if !leftover.is_empty() {
                            remaining.push(leftover);
                        }
                        continue;
                    }
                }
            }
            // A year can also sit after the sequence, as in `Vol 1 - 1994 - Title` \u{2014}
            // but only when something else is in the name. A folder called `1984` is a
            // book called 1984, and nothing follows it to say otherwise.
            if name.year.is_none() && segments.len() > 1 && is_year(segment) {
                name.year = segment.parse().ok();
                continue;
            }
            remaining.push(segment.clone());
        }

        if name.series_index.is_some() && name.shape == NameShape::Plain {
            name.shape = NameShape::NamedSequence;
        }
        if name.year.is_some() && name.series_index.is_some() && name.shape == NameShape::Plain {
            name.shape = NameShape::YearTitle;
        }

        match remaining.len() {
            0 => {}
            1 => name.title = remaining.first().cloned().unwrap_or_default(),
            _ => {
                if options.subtitles {
                    // Subtitle parsing is opt-in, and only then does a trailing segment
                    // become a subtitle. Everything before it is still the title, joined,
                    // so `A - B - C` does not invent a series name.
                    let subtitle = remaining.last().cloned().unwrap_or_default();
                    let head = &remaining[..remaining.len() - 1];
                    name.title = head.join(" - ");
                    name.subtitle = Some(subtitle);
                } else {
                    // With subtitles off, ` - ` is a character of the title and not a
                    // separator: "Death - Endless" is one title, and splitting it would
                    // silently shorten it.
                    name.title = remaining.join(" - ");
                }
            }
        }

        // A delimited group counts as *seen* even when it turned out to be empty, because
        // `{}` was a folder someone deliberately named with braces in it \u{2014} putting
        // it back as the title would present the delimiters as the name of a book.
        let had_group = name.narrator.is_some()
            || name.asin.is_some()
            || name.year.is_some()
            || saw_delimited_group(folder);

        if name.title.is_empty() && !had_group && !folder.trim().is_empty() {
            // Nothing was recognised at all, so the folder name itself is the title.
            name.title = folder.trim().to_string();
        }
        // A title made only of separators and delimiters is not a title. `-` or ` - ` is
        // a folder someone left half-renamed, and showing it as the name of a book puts
        // punctuation in the library index.
        if !name.title.is_empty() && !name.title.chars().any(is_title_character) {
            name.title = String::new();
        }
        if name.title.is_empty() {
            // The truthful answer. Borrowing the raw folder back here would put
            // `{Sam Tsoutsouvas}` in the title of a book, and an empty title is what
            // lets the caller report the folder as unreadable instead.
            name.shape = NameShape::NoTitle;
        }
        name
    }

    /// Whether a title could be read from this name.
    #[must_use]
    pub fn is_recognised(&self) -> bool {
        self.shape != NameShape::NoTitle
    }

    /// A one-line rendering, for a report or a listing.
    #[must_use]
    pub fn display_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(y) = self.year {
            parts.push(y.to_string());
        }
        if let Some(s) = &self.series {
            parts.push(s.clone());
        }
        if let Some(i) = self.series_index {
            parts.push(format!("#{i}"));
        }
        parts.push(self.title.clone());
        if let Some(s) = &self.subtitle {
            parts.push(format!("({s})"));
        }
        parts.join(" — ")
    }
}

impl fmt::Display for BookName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display_line())
    }
}

/// Whether a character could be part of a title.
///
/// Deliberately permissive: anything printable that is not a separator or a delimiter
/// counts, because a title of odd characters is still somebody's title and filtering it
/// would throw away a real book. The only thing excluded is punctuation a person uses to
/// structure a name rather than to name a book.
fn is_title_character(c: char) -> bool {
    !matches!(c, '-' | '_' | '.' | '{' | '}' | '[' | ']' | '(' | ')' | '/') && !c.is_whitespace()
}

/// Whether a name carries any delimited group at all, empty or not.
fn saw_delimited_group(folder: &str) -> bool {
    // Only the actual delimiters. Digits are deliberately not included: a sequence number
    // or a year is already handled above, and counting digits here would make every named
    // volume (`Book 1`) refuse to be its own title.
    folder.contains('{')
        || folder.contains('}')
        || folder.contains('[')
        || folder.contains(']')
        || folder.contains('(')
        || folder.contains(')')
}

/// Pull a `{...}` narrator group out of the name, returning what is left.
fn take_narrator(input: &str, name: &mut BookName) -> String {
    match (input.find('{'), input.rfind('}')) {
        (Some(open), Some(close)) if close > open => {
            let inner = input[open + 1..close].trim();
            if !inner.is_empty() {
                name.narrator = Some(inner.to_string());
            }
            let mut left = String::with_capacity(input.len());
            left.push_str(&input[..open]);
            left.push_str(&input[close + 1..]);
            // Re-join any split around the removed group so `A {X} - B` does not become
            // `A  - B` with a doubled separator.
            left.trim().replace("  - ", " - ")
        }
        _ => input.to_string(),
    }
}

/// Pull an ASIN out of `[...]`, returning what is left.
///
/// Only a bracketed group that looks like an ASIN is taken, since `[text]` is a plausible
/// thing for a person to write in a folder name and a title is more likely than a serial
/// number in brackets.
fn take_asin(input: &str, name: &mut BookName) -> String {
    match (input.find('['), input.rfind(']')) {
        (Some(open), Some(close)) if close > open => {
            let inner = input[open + 1..close].trim();
            if is_asin(inner) {
                name.asin = Some(inner.to_ascii_uppercase());
            } else if !inner.is_empty() {
                // A non-empty group that is not a serial number is left in the name: a
                // person who wrote `[Draft]` meant something by it. An *empty* group is
                // different \u{2014} there is nothing to keep, so it is dropped rather
                // than becoming a book called `[]`.
                return input.to_string();
            }
            let mut left = String::with_capacity(input.len());
            left.push_str(&input[..open]);
            left.push_str(&input[close + 1..]);
            left.trim().replace("  - ", " - ").trim().to_string()
        }
        _ => input.to_string(),
    }
}

/// Whether a bracketed group is an Audible ASIN: `B` plus nine alphanumerics.
fn is_asin(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    bytes.len() == 10 && bytes[0] == b'B' && bytes[1..].iter().all(u8::is_ascii_alphanumeric)
}

/// Pull a four-digit year out of the front of the name, returning what is left.
///
/// A bare `1994 - ` prefix and a parenthesised `(1994) - ` prefix are both accepted,
/// because the documentation shows both.
fn take_year(input: &str, name: &mut BookName) -> String {
    let trimmed = input.trim_start();

    if let Some(close) = trimmed.strip_prefix('(') {
        if let Some(end) = close.find(')') {
            let inner = close[..end].trim();
            if is_year(inner) {
                name.year = inner.parse().ok();
                return close[end + 1..]
                    .trim_start_matches(" - ")
                    .trim()
                    .to_string();
            }
            // An empty or non-year parenthetical is dropped like any other empty group:
            // `()` is not the name of a book.
            if inner.is_empty() {
                return close[end + 1..]
                    .trim_start_matches(" - ")
                    .trim()
                    .to_string();
            }
        }
    }

    // Only a leading run of digits counts, and only if the whole token is a year.
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if is_year(&digits) {
        let rest = &trimmed[digits.len()..];
        // A year must be a whole token *with something after it*: a folder called
        // `1984` is a book called 1984, not a book of unknown title published in 1984.
        let next = rest.chars().next();
        if matches!(next, Some(' ') | Some('-') | Some('.')) {
            name.year = digits.parse().ok();
            let rest = rest.trim_start_matches([' ', '-', '.']);
            return rest.trim_start_matches(" - ").trim().to_string();
        }
    }
    input.to_string()
}

/// Whether a token is a plausible publication year.
fn is_year(token: &str) -> bool {
    token.len() == 4 && token.bytes().all(|b| b.is_ascii_digit())
}

/// Take a leading sequence token: `1 - Title`, `1. Title`, `Book 1`, `Vol 1`.
fn take_leading_sequence(segment: &str, allow_bare_number: bool) -> Option<(u32, String)> {
    let trimmed = segment.trim();

    // `Book 1 - Title` and `Vol. 1 - Title` put the number second.
    for keyword in ["Book", "Vol", "Volume", "Part", "#"] {
        for suffix in [" ", ".", ""] {
            let candidate = format!("{keyword}{suffix}");
            if let Some(head) = trimmed.get(..candidate.len()) {
                if !head.eq_ignore_ascii_case(&candidate) {
                    continue;
                }
            } else {
                continue;
            }
            {
                let after = trimmed.get(candidate.len()..).unwrap_or("").trim_start();
                let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
                if digits.is_empty() {
                    continue;
                }
                let index = digits.parse().ok()?;
                let rest = after
                    .get(digits.len()..)
                    .unwrap_or("")
                    .trim_start_matches([' ', '-', '.']);
                return Some((index, rest.trim().to_string()));
            }
        }
    }

    // A bare leading number: `1 - Title` or `1. Title`.
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = trimmed.get(digits.len()..).unwrap_or("");
    // `1 - Title` splits on the separator before this is reached, leaving a bare `1`.
    // That is still the sequence, so a number with nothing after it counts; a *year*
    // would have been taken already.
    let separator = rest.chars().next();
    if rest.is_empty() {
        // A number with nothing after it at all: only a sequence if the name carries
        // another segment, otherwise it is the title.
        return if allow_bare_number {
            digits.parse().ok().map(|i| (i, String::new()))
        } else {
            None
        };
    }
    if !matches!(separator, Some(' ') | Some('-') | Some('.') | Some('_')) {
        return None;
    }
    let leftover = rest
        .trim_start_matches([' ', '-', '.', '_'])
        .trim()
        .to_string();
    if leftover.is_empty() {
        return None;
    }
    digits.parse().ok().map(|i| (i, leftover))
}

/// Take a named sequence anywhere in a segment: `Volume 1`, `Book 1`, `Vol 1`.
fn take_named_sequence(segment: &str) -> Option<(u32, String)> {
    let trimmed = segment.trim();
    for keyword in ["Volume", "Book", "Vol", "Part"] {
        for suffix in [" ", "."] {
            let candidate = format!("{keyword}{suffix}");
            if trimmed.len() <= candidate.len() {
                continue;
            }
            if !trimmed[..candidate.len()].eq_ignore_ascii_case(&candidate) {
                continue;
            }
            let after = trimmed[candidate.len()..].trim_start();
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                continue;
            }
            let index = digits.parse().ok()?;
            let rest = after[digits.len()..].trim_start_matches([' ', '-', '.']);
            return Some((index, rest.trim().to_string()));
        }
    }
    None
}

/// Take a `Disc 1` / `CD2` / `Disk 3` subfolder number.
#[must_use]
pub fn disc_number(folder: &str) -> Option<u32> {
    let trimmed = folder.trim();
    for keyword in ["Disc", "CD", "Disk"] {
        if trimmed.len() > keyword.len() && trimmed[..keyword.len()].eq_ignore_ascii_case(keyword) {
            let digits: String = trimmed[keyword.len()..]
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
    }
    None
}

/// Whether a folder name is a disc subfolder rather than a book folder.
///
/// The check has to be exact in both directions: `Disc 1` is a disc, and `Discworld` is a
/// book called Discworld. Comparing only the leading word files Discworld inside Disc 1,
/// so the digits have to follow the word directly, with at most whitespace between.
#[must_use]
pub fn is_disc_folder(folder: &str) -> bool {
    disc_number(folder).is_some()
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

    /// Every example from the Audiobookshelf documentation, with what it must yield.
    ///
    /// Taken verbatim from the published directory-structure page rather than invented,
    /// because a naming convention is only worth implementing if it is the one people
    /// actually use, and an invented one would agree with nothing.
    const DOCUMENTED: &[(&str, &str, Option<u32>, Option<u16>)] = &[
        ("Wizards First Rule", "Wizards First Rule", None, None),
        ("{Sam Tsoutsouvas}", "", None, None),
        (
            "1994 - Wizards First Rule",
            "Wizards First Rule",
            None,
            Some(1994),
        ),
        (
            "Wizards First Rule - A Really Good Subtitle",
            "Wizards First Rule",
            None,
            None,
        ),
        (
            "1994 - Book 1 - Wizards First Rule",
            "Wizards First Rule",
            Some(1),
            Some(1994),
        ),
        (
            "1994 - Volume 1. Wizards First Rule",
            "Wizards First Rule",
            Some(1),
            Some(1994),
        ),
        (
            "1994 - Book 1 - Wizards First Rule",
            "Wizards First Rule",
            Some(1),
            Some(1994),
        ),
        (
            "1 - Wizards First Rule",
            "Wizards First Rule",
            Some(1),
            None,
        ),
        ("1. Wizards First Rule", "Wizards First Rule", Some(1), None),
        (
            "Vol 1 - 1994 - Wizards First Rule",
            "Wizards First Rule",
            Some(1),
            Some(1994),
        ),
        (
            "1994 - Wizards First Rule - Volume 1",
            "Wizards First Rule",
            Some(1),
            Some(1994),
        ),
        (
            "(1994) - Wizards First Rule - A Really Good Subtitle",
            "Wizards First Rule",
            None,
            Some(1994),
        ),
    ];

    #[test]
    fn every_documented_folder_name_yields_its_title_and_sequence() {
        // Subtitle parsing on, because one documented example has a subtitle and the
        // convention makes it an explicit setting rather than an automatic one.
        let options = ParseOptions { subtitles: true };
        for (folder, title, index, year) in DOCUMENTED {
            let name = BookName::parse_with(folder, options);
            assert_eq!(&name.title, title, "title for {folder:?}");
            assert_eq!(name.series_index, *index, "index for {folder:?}");
            assert_eq!(name.year, *year, "year for {folder:?}");
        }
    }

    #[test]
    fn the_narrator_is_taken_from_braces() {
        let name = BookName::parse("1994 - Volume 1. Wizards First Rule {Sam Tsoutsouvas}");
        assert_eq!(
            name.narrator.as_deref(),
            Some("Sam Tsoutsouvas"),
            "and the title has no braces left in it: {:?}",
            name.title
        );
        assert_eq!(name.title, "Wizards First Rule");
    }

    #[test]
    fn a_bracket_group_is_only_an_asin_when_it_looks_like_one() {
        let with = BookName::parse("1. Wizards First Rule [B002V0QK4C]");
        assert_eq!(with.asin.as_deref(), Some("B002V0QK4C"));
        assert_eq!(with.title, "Wizards First Rule");

        // A bracketed group that is not a serial number is left in the name rather than
        // silently discarded — a person who wrote `[Draft]` meant something by it.
        let draft = BookName::parse("Wizards First Rule [Draft]");
        assert_eq!(draft.asin, None);
        assert!(
            draft.title.contains("Draft"),
            "not silently dropped: {:?}",
            draft.title
        );
    }

    #[test]
    fn a_bare_year_is_a_year_and_not_a_volume() {
        // The trap: `1984` is a title and also looks like a number. It is only a year
        // when something follows it, and only a volume when a *sequence marker* follows.
        let title = BookName::parse("1984");
        assert_eq!(title.title, "1984");
        assert_eq!(title.year, None, "a bare number is not a year");
        assert_eq!(title.series_index, None, "nor a volume");

        let as_year = BookName::parse("1984 - Animal Farm");
        assert_eq!(as_year.year, Some(1984));
        assert_eq!(as_year.title, "Animal Farm");
    }

    #[test]
    fn a_leading_number_before_a_title_is_a_sequence() {
        let name = BookName::parse("3 - The Gunslinger");
        assert_eq!(name.series_index, Some(3));
        assert_eq!(name.title, "The Gunslinger");
        assert_eq!(name.shape, NameShape::LeadingSequence);
    }

    #[test]
    fn a_folder_named_only_for_its_position_keeps_that_as_its_title() {
        // `Book 1` in a series of sibling folders is a crude but real layout, and refusing
        // it a title would report every one as broken. The sequence is still read.
        let name = BookName::parse("Book 1");
        assert_eq!(name.title, "Book 1");
        assert_eq!(name.series_index, Some(1));
    }

    #[test]
    fn an_ordinary_title_is_a_recognised_convention() {
        // A bare title is one of the documented shapes, so "unrecognised" must not mean
        // "plain" \u{2014} that would report most of a library.
        let name = BookName::parse("Wizards First Rule");
        assert_eq!(name.title, "Wizards First Rule");
        assert!(name.is_recognised());
        assert_eq!(name.shape, NameShape::Plain);
    }

    #[test]
    fn a_name_with_a_narrator_but_no_title_says_so_rather_than_inventing_one() {
        // A folder called only `{Sam Tsoutsouvas}` has a narrator and no title. Borrowing
        // the raw folder back would put a name with braces in it into a book's title.
        let name = BookName::parse("{Sam Tsoutsouvas}");
        assert_eq!(name.title, "");
        assert_eq!(name.narrator.as_deref(), Some("Sam Tsoutsouvas"));
        assert!(!name.is_recognised());
    }

    #[test]
    fn a_name_of_only_delimiters_has_no_title() {
        for nothing in ["{}", "[]", "()", "-", "  ", "() {}", "() []"] {
            let name = BookName::parse(nothing);
            assert!(
                !name.is_recognised(),
                "{nothing:?} should yield no title, got {:?}",
                name.title
            );
        }
    }

    #[test]
    fn a_title_containing_a_dash_is_not_torn_apart() {
        // `-` inside a title is a real character in plenty of titles, and a single
        // ` - ` split is only a separator when there is something on both sides.
        let name = BookName::parse("Death - Endless");
        assert_eq!(name.title, "Death - Endless");
        assert_eq!(name.subtitle, None, "not mistaken for a subtitle");
    }

    #[test]
    fn an_empty_name_does_not_panic() {
        for empty in ["", "   ", "-", " - ", "{}", "[]", "()"] {
            let name = BookName::parse(empty);
            assert!(
                name.title.is_empty() || !name.title.is_empty(),
                "no panic for {empty:?}"
            );
        }
    }

    #[test]
    fn disc_folders_are_identified_and_ordinary_names_are_not() {
        for (folder, want) in [
            ("Disc 1", Some(1u32)),
            ("disc 2", Some(2)),
            ("Disc1", Some(1)),
            ("Disc 004", Some(4)),
            ("CD 2", Some(2)),
            ("CD2", Some(2)),
            ("Disk 3", Some(3)),
            ("disc", None),
            ("Discworld", None),
            ("Wizards First Rule", None),
            ("", None),
        ] {
            assert_eq!(disc_number(folder), want, "for {folder:?}");
        }
    }

    #[test]
    fn discworld_is_not_a_disc_folder() {
        // The check that matters: a book called Discworld must not be filed inside Disc 1.
        assert!(!is_disc_folder("Discworld"));
        assert!(is_disc_folder("Disc 1"));
        assert!(is_disc_folder("CD2"));
    }
}
