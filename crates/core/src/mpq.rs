//! MPQ archive reading (read-only in v1).
//!
//! Needed day one for packed containers like `RandomBuff.SC2Mod`
//! (decision M4). Candidate implementations are evaluated in M1; see
//! `docs/design/roadmap.md` risk register.

use crate::error::Result;

/// A read-only handle to an MPQ archive (opened from a file or memory).
pub struct MpqArchive;

impl MpqArchive {
    pub fn open(_path: impl AsRef<std::path::Path>) -> Result<Self> {
        Err(crate::internal!("mpq reader lands in M1"))
    }

    /// List all member paths inside the archive.
    pub fn list(&self) -> Result<Vec<String>> {
        Err(crate::internal!("mpq reader lands in M1"))
    }

    /// Read one member's full contents.
    pub fn read(&self, _member: &str) -> Result<Vec<u8>> {
        Err(crate::internal!("mpq reader lands in M1"))
    }
}
