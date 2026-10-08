//! `audiobook-shelf` — an audiobook-first library manager.
//!
//! Built on the WyattAu estate's format crates: `audiobook-core` for the domain model,
//! `mp4-core` and `id3-core` for the containers. This crate is the layer that decides
//! **which files belong to the same book**, which is information no audio file contains —
//! it lives in the folder, because that is where the person who assembled the library put
//! it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod layout;
pub mod m4b;
pub mod naming;
pub mod scan;
pub mod write;

pub use layout::{Book, BookFile};
pub use naming::{BookName, NameShape, ParseOptions};
pub use write::{write_mp3_chapters, WriteOutcome};
