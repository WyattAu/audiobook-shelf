# Security Policy: audiobook-conformance
- **No network, filesystem, or process state**; the crate is pure
- `#![forbid(unsafe_code)]`; no unsafe blocks exist in this crate.