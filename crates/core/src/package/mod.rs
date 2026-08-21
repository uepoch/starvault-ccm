//! Package parsing and normalization.
//!
//! See `docs/design/package-model.md`. Submodules:
//! - `metadata`: legacy `metadata.txt` parser (compatibility-critical)
//! - `header`: `DocumentHeader` binary dependency reader

pub mod header;
pub mod metadata;
