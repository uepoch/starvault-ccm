//! `DocumentInfo` XML dependency parser.
//!
//! `DocumentInfo` is an XML document declaring the same dependency list as
//! the binary `DocumentHeader`; ingest cross-checks both (package-model.md).
//! Real-world files arrive in several encodings, so decoding mirrors
//! flat-waterfall's proven logic:
//!
//! - UTF-8 BOM (`EF BB BF`)
//! - UTF-16 LE/BE with BOM (`FF FE` / `FE FF`)
//! - raw `<` as `3C 00` / `00 3C` (UTF-16 without BOM)
//! - fallback: UTF-8
//!
//! Extraction is a scan, not a strict XML parse: DocumentInfo files in the
//! wild are not always namespace-clean, and declaration-only documents are
//! valid (they simply declare no dependencies).

/// Decode bytes into text per the encoding rules above.
pub fn decode_document_bytes(bytes: &[u8]) -> Result<String, std::string::FromUtf16Error> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok(String::from_utf8_lossy(&bytes[3..]).into_owned());
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return utf16(&bytes[2..], true);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return utf16(&bytes[2..], false);
    }
    // BOM-less UTF-16 is detected by the alignment of a raw '<'.
    if bytes.starts_with(&[0x3C, 0x00]) {
        return utf16(bytes, true);
    }
    if bytes.starts_with(&[0x00, 0x3C]) {
        return utf16(bytes, false);
    }
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn utf16(bytes: &[u8], little_endian: bool) -> Result<String, std::string::FromUtf16Error> {
    if !bytes.len().is_multiple_of(2) {
        // Truncated code unit; drop the trailing byte rather than failing the
        // whole document over one stray byte.
        return utf16(&bytes[..bytes.len() - 1], little_endian);
    }
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|c| {
            let pair = [c[0], *c.get(1).unwrap_or(&0)];
            if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        })
        .collect();
    String::from_utf16(&units)
}

/// Decode the XML entities that appear in dependency values.
fn decode_entities(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let Some(semi) = tail.find(';') else {
            out.push('&');
            rest = &rest[pos + 1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            hex if hex.starts_with("#x") || hex.starts_with("#X") => {
                u32::from_str_radix(&hex[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            dec if dec.starts_with('#') => dec[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            // Unknown entity: keep it verbatim.
            None => {
                out.push('&');
                rest = &rest[pos + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Case-insensitive scan for `<Dependencies><Value>…</Value></Dependencies>`
/// sections. Mirrors flat-waterfall's semantics: every Value inside any
/// Dependencies section, in document order.
pub fn read_dependencies(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut deps = Vec::new();
    let mut search_from = 0;

    while let Some(rel) = lower[search_from..].find("<dependencies") {
        let open_start = search_from + rel;
        let Some(open_end) = lower[open_start..].find('>') else {
            break;
        };
        let body_start = open_start + open_end + 1;
        let Some(close_rel) = lower[body_start..].find("</dependencies") else {
            break;
        };
        let body = &text[body_start..body_start + close_rel];
        let body_lower = &lower[body_start..body_start + close_rel];

        let mut value_from = 0;
        while let Some(vrel) = body_lower[value_from..].find("<value") {
            let vstart = value_from + vrel;
            let Some(vend) = body_lower[vstart..].find('>') else {
                break;
            };
            let text_start = vstart + vend + 1;
            let Some(close_vrel) = body_lower[text_start..].find("</value") else {
                break;
            };
            deps.push(decode_entities(
                body[text_start..text_start + close_vrel].trim(),
            ));
            value_from = text_start + close_vrel + 1;
        }

        search_from = body_start + close_rel + 1;
    }

    deps
}

/// Convenience: decode + parse from raw bytes.
pub fn read_dependencies_from_bytes(
    bytes: &[u8],
) -> Result<Vec<String>, std::string::FromUtf16Error> {
    Ok(read_dependencies(&decode_document_bytes(bytes)?))
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
    fn parses_real_tarcade_document_info() {
        let bytes = std::fs::read(fixture("tarcade.DocumentInfo")).unwrap();
        let deps = read_dependencies_from_bytes(&bytes).unwrap();
        assert_eq!(
            deps,
            vec![
                r"file:Mods\kit_liberty_story.SC2Mod",
                r"file:Mods\RaynorRogue.SC2Mod",
            ]
        );
    }

    #[test]
    fn parses_real_raynorrogue_document_info_with_nested_dep() {
        let bytes = std::fs::read(fixture("raynorrogue.DocumentInfo")).unwrap();
        let deps = read_dependencies_from_bytes(&bytes).unwrap();
        assert!(deps.iter().any(|d| d.contains(r"SCORE\SCORE-Other.SC2Mod")));
    }

    #[test]
    fn handles_utf16_without_bom() {
        // "file:Mods\X.SC2Mod" wrapped in Dependencies/Value, encoded UTF-16LE.
        let xml = "<Dependencies><Value>file:Mods\\X.SC2Mod</Value></Dependencies>";
        let mut bytes: Vec<u8> = Vec::new();
        for unit in xml.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(bytes[0], 0x3C);
        assert_eq!(bytes[1], 0x00);
        let deps = read_dependencies_from_bytes(&bytes).unwrap();
        assert_eq!(deps, vec![r"file:Mods\X.SC2Mod"]);
    }

    #[test]
    fn declaration_only_document_has_no_dependencies() {
        let doc = "<?xml version=\"1.0\" encoding=\"utf-8\"?>";
        assert!(read_dependencies(doc).is_empty());
    }

    #[test]
    fn entities_are_decoded_in_values() {
        let doc = "<Dependencies><Value>file:Mods&#92;A&amp;B.SC2Mod</Value></Dependencies>";
        assert_eq!(read_dependencies(doc), vec![r"file:Mods\A&B.SC2Mod"]);
    }
}
