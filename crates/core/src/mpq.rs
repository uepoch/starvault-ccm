//! MPQ archive reading (read-only, decision M4).
//!
//! Backend: [`wow_mpq`](https://crates.io/crates/wow-mpq) (warcraft-rs),
//! chosen after an empirical bake-off recorded in `docs/design/roadmap.md`:
//!
//! - msierks/mpq: rejected. v1-only headers ("ToDo: Header v3 and v4"), no
//!   HET/BET, enumeration only via an optional `(listfile)` member.
//! - StormLib C bindings: fully capable but drags a C++ toolchain into every
//!   build host; kept as escalation path if wow-mpq ever fails a real archive.
//! - wow-mpq: pure Rust, actively maintained, StormLib-compatible, covers
//!   MPQ v1–v4 (WoW 1.12–5.4.8 era includes HET/BET). Spike test reads a real
//!   SC2 packed container (`RandomBuff.SC2Mod`) end-to-end.

use std::path::Path;

use crate::error::{pkg_err, Result};

/// A read-only handle to an MPQ archive.
pub struct MpqArchive {
    inner: wow_mpq::Archive,
    context: String,
}

impl MpqArchive {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let context = path.display().to_string();
        let inner = wow_mpq::Archive::open(path)
            .map_err(|e| pkg_err(context.clone(), format!("failed to open MPQ: {e}")))?;
        Ok(Self { inner, context })
    }

    /// Enumerate member paths. Enumeration walks the archive tables; member
    /// names keep their original (Windows-style) separators.
    pub fn list(&mut self) -> Result<Vec<String>> {
        let entries = self
            .inner
            .list()
            .map_err(|e| pkg_err(self.context.clone(), format!("failed to list MPQ: {e}")))?;
        Ok(entries.into_iter().map(|e| e.name.clone()).collect())
    }

    /// Read one member's full contents. Name matching is case-insensitive,
    /// matching Windows filesystem semantics.
    pub fn read(&mut self, member: &str) -> Result<Vec<u8>> {
        self.inner.read_file(member).map_err(|e| {
            pkg_err(
                self.context.clone(),
                format!("failed to read MPQ member `{member}`: {e}"),
            )
        })
    }

    /// Find a member by case-insensitive name; returns the archive's own
    /// spelling of the name so it can be passed to [`Self::read`].
    pub fn find_case_insensitive(&mut self, member: &str) -> Result<Option<String>> {
        let target = member.to_ascii_lowercase();
        Ok(self
            .list()?
            .into_iter()
            .find(|n| n.to_ascii_lowercase() == target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn lists_and_reads_real_sc2_container() {
        let mut a = MpqArchive::open(fixture("RandomBuff.SC2Mod")).unwrap();
        let entries = a.list().unwrap();
        assert!(entries
            .iter()
            .any(|n| n.eq_ignore_ascii_case("DocumentHeader")));
        assert!(entries
            .iter()
            .any(|n| n.eq_ignore_ascii_case("DocumentInfo")));

        let header = a.read("DocumentHeader").unwrap();
        assert_eq!(&header[0..4], b"H2CS");
    }

    #[test]
    fn case_insensitive_lookup_works() {
        let mut a = MpqArchive::open(fixture("RandomBuff.SC2Mod")).unwrap();
        let found = a.find_case_insensitive("documentheader").unwrap();
        assert!(found.is_some(), "case-insensitive member lookup failed");
    }

    #[test]
    fn missing_member_is_a_package_error() {
        let mut a = MpqArchive::open(fixture("RandomBuff.SC2Mod")).unwrap();
        assert!(a.find_case_insensitive("(nope)").unwrap().is_none());
        assert!(a.read("(nope)").is_err());
    }
}
