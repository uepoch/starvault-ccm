//! Spike: can `wow-mpq` (warcraft-rs) read a real SC2 packed container?
//!
//! Run with: cargo test --test mpq_spike -- --nocapture
//!
//! Context: msierks/mpq was rejected — v1-only headers, no HET/BET, listing
//! only via optional `(listfile)`. wow-mpq claims StormLib compatibility and
//! WoW 1.12–5.4.8 coverage (MPQ v1–v4). This test is the acceptance gate for
//! using it as our MPQ backend (decision M4).

use std::path::PathBuf;

const FIXTURE: &str = "tests/fixtures/RandomBuff.SC2Mod";

#[test]
fn wow_mpq_reads_sc2_packed_container() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    let mut archive = match wow_mpq::Archive::open(&path) {
        Ok(a) => a,
        Err(e) => panic!("OPEN FAILED: {e:?}"),
    };

    let entries = match archive.list() {
        Ok(e) => e,
        Err(e) => panic!("LIST FAILED: {e:?}"),
    };
    let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    println!("entries: {names:#?}");
    assert!(!names.is_empty(), "archive listed zero entries");

    // The whole point of M4: read DocumentHeader out of the packed container.
    let header_entry = names
        .iter()
        .find(|n| n.eq_ignore_ascii_case("DocumentHeader"))
        .expect("DocumentHeader not found among entries");

    let data = archive
        .read_file(header_entry.as_str())
        .expect("read DocumentHeader");
    assert!(data.len() >= 48, "DocumentHeader suspiciously small");
    assert_eq!(&data[0..4], b"H2CS", "DocumentHeader magic mismatch");
    println!("DocumentHeader: {} bytes, magic OK", data.len());

    // Dump RandomBuff's declared dependencies for the fixture record.
    let count = u32::from_le_bytes([data[44], data[45], data[46], data[47]]);
    let mut off = 48usize;
    let mut deps = Vec::new();
    for _ in 0..count {
        let end = data[off..].iter().position(|&b| b == 0).unwrap() + off;
        deps.push(String::from_utf8_lossy(&data[off..end]).into_owned());
        off = end + 1;
    }
    println!("RandomBuff dependencies: {deps:#?}");
}
