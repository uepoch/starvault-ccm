//! Package parsing and normalization.
//!
//! See `docs/design/package-model.md`. Submodules:
//! - `metadata`: legacy `metadata.txt` parser (compatibility-critical)
//! - `header`: `DocumentHeader` binary dependency reader
//! - `docinfo`: `DocumentInfo` XML dependency reader
//! - `container`: unified dependency access for both container forms

pub mod container;
pub mod docinfo;
pub mod header;
pub mod metadata;
pub mod normalize;
