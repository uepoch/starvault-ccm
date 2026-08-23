//! Legacy `metadata.txt` parser.
//!
//! Compatibility contract from the original CCM: flat `key=value` lines,
//! split on the first `=`, keys case-insensitive. Keys: title, author, desc,
//! version, campaign.
//!
//! The campaign value is a *guess* produced by fuzzy substring matching
//! (decision K1/K2): the result carries the matched pattern so the UI can
//! show its basis and let the user confirm. No-match is an explicit Unknown,
//! never a silent bucket.

/// Fuzzy slot matching, evaluated in the original tool's order.
const SLOT_PATTERNS: &[(&str, SlotGuessKind)] = &[
    ("wings", SlotGuessKind::Wol),
    ("liberty", SlotGuessKind::Wol),
    ("wol", SlotGuessKind::Wol),
    ("heart", SlotGuessKind::HotS),
    ("swarm", SlotGuessKind::HotS),
    ("hots", SlotGuessKind::HotS),
    ("legacy", SlotGuessKind::LotV),
    ("void", SlotGuessKind::LotV),
    ("lotv", SlotGuessKind::LotV),
    ("nova", SlotGuessKind::Nco),
    ("covert", SlotGuessKind::Nco),
    ("ops", SlotGuessKind::Nco),
    ("nco", SlotGuessKind::Nco),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotGuessKind {
    Wol,
    HotS,
    LotV,
    Nco,
    Unknown,
}

impl SlotGuessKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SlotGuessKind::Wol => "wol",
            SlotGuessKind::HotS => "hots",
            SlotGuessKind::LotV => "lotv",
            SlotGuessKind::Nco => "nco",
            SlotGuessKind::Unknown => "unknown",
        }
    }
}

/// A slot assignment guess with its evidence attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotGuess {
    pub kind: SlotGuessKind,
    /// The substring that triggered the match, e.g. `"lotv"`.
    /// `None` when kind is Unknown.
    pub matched_pattern: Option<&'static str>,
}

/// Parsed legacy metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub desc: Option<String>,
    pub version: Option<String>,
    pub campaign_raw: Option<String>,
}

impl LegacyMetadata {
    pub fn parse(text: &str) -> Self {
        let mut meta = LegacyMetadata::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            // Empty values are absent values: `title=` must not become an
            // empty title (or, via slug, an empty package id).
            let value = (!value.is_empty()).then(|| value.to_string());
            let Some(value) = value else { continue };
            match key.as_str() {
                "title" => meta.title = Some(value),
                "author" => meta.author = Some(value),
                "desc" => meta.desc = Some(value),
                "version" => meta.version = Some(value),
                "campaign" => meta.campaign_raw = Some(value),
                _ => {} // unknown keys are ignored, matching the original tool
            }
        }
        meta
    }

    /// Fuzzy slot guess per the original CCM's matching order.
    pub fn slot_guess(&self) -> SlotGuess {
        let Some(raw) = &self.campaign_raw else {
            return SlotGuess {
                kind: SlotGuessKind::Unknown,
                matched_pattern: None,
            };
        };
        let hay = raw.to_lowercase();
        for (pattern, kind) in SLOT_PATTERNS {
            if hay.contains(pattern) {
                return SlotGuess {
                    kind: *kind,
                    matched_pattern: Some(pattern),
                };
            }
        }
        SlotGuess {
            kind: SlotGuessKind::Unknown,
            matched_pattern: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
title=Nexus Covert Ops Legacy
desc=A thing
author=SomeMapper
version=1.2.3
campaign=Legacy of the Void
";

    #[test]
    fn parses_all_known_keys() {
        let m = LegacyMetadata::parse(SAMPLE);
        assert_eq!(m.title.as_deref(), Some("Nexus Covert Ops Legacy"));
        assert_eq!(m.author.as_deref(), Some("SomeMapper"));
        assert_eq!(m.version.as_deref(), Some("1.2.3"));
        assert_eq!(m.desc.as_deref(), Some("A thing"));
        assert_eq!(m.campaign_raw.as_deref(), Some("Legacy of the Void"));
    }

    #[test]
    fn keys_are_case_insensitive_and_split_on_first_equals() {
        let m = LegacyMetadata::parse("TITLE=X=Y\nAuthor = Z\n");
        assert_eq!(m.title.as_deref(), Some("X=Y"));
        assert_eq!(m.author.as_deref(), Some("Z"));
    }

    #[test]
    fn fuzzy_match_in_original_order() {
        // "ops" appears before any lotv pattern would match? No: patterns are
        // evaluated in fixed order, lotv's "legacy"/"void"/"lotv" come first.
        let m = LegacyMetadata::parse("campaign=Legacy of the Void");
        assert_eq!(m.slot_guess().kind, SlotGuessKind::LotV);

        let m = LegacyMetadata::parse("campaign=wings");
        assert_eq!(m.slot_guess().kind, SlotGuessKind::Wol);
    }

    #[test]
    fn unknown_campaign_is_explicit_not_a_bucket() {
        let m = LegacyMetadata::parse("campaign=Something Original");
        let g = m.slot_guess();
        assert_eq!(g.kind, SlotGuessKind::Unknown);
        assert_eq!(g.matched_pattern, None);
    }

    #[test]
    fn missing_campaign_is_unknown() {
        let m = LegacyMetadata::parse("title=Only A Title\n");
        assert_eq!(m.slot_guess().kind, SlotGuessKind::Unknown);
    }
}
