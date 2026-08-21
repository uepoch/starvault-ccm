//! Archive import: extraction safety, preview, cancellable ingest.

use std::io::Write;
use std::path::Path;

use svccm_core::layout::SlotId;
use svccm_core::package::import::{extract_archive, preview_plan};
use svccm_core::package::normalize::plan_from_extracted;
use svccm_core::store::Store;

/// Build a zip with the given member names and contents.
fn make_zip(path: &Path, members: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, content) in members {
        zip.start_file(*name, options).unwrap();
        zip.write_all(content).unwrap();
    }
    zip.finish().unwrap();
}

fn map_container(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("payload.txt"), b"map").unwrap();
}

#[test]
fn extract_archive_unwraps_and_previews() {
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = tmp.path().join("pkg.zip");
    make_zip(
        &zip_path,
        &[
            (
                "metadata.txt",
                b"title=My Cool Campaign\ncampaign=Legacy of the Void\n" as &[u8],
            ),
            ("MyCoolCampaign.SC2Map/payload.txt", b"map" as &[u8]),
        ],
    );

    let dest = tmp.path().join("extracted");
    let mut seen = Vec::new();
    let completed = extract_archive(&zip_path, &dest, |p| {
        seen.push(p.current_file.clone());
        true
    })
    .unwrap();
    assert!(completed);
    assert_eq!(seen.len(), 2);

    // Wrapper folder is stripped; metadata drives title and slot guess.
    let plan = plan_from_extracted(&dest).unwrap();
    let preview = preview_plan(&plan);
    assert_eq!(preview.title.as_deref(), Some("My Cool Campaign"));
    assert_eq!(preview.suggested_id, "my-cool-campaign");
    assert_eq!(preview.slot_guess, "lotv");
    assert_eq!(preview.matched_pattern, Some("legacy"));
    assert_eq!(preview.file_count, 1);
}

#[test]
fn extract_can_be_cancelled_at_file_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = tmp.path().join("pkg.zip");
    make_zip(
        &zip_path,
        &[
            ("a.txt", b"a" as &[u8]),
            ("b.txt", b"b" as &[u8]),
            ("c.txt", b"c" as &[u8]),
        ],
    );

    let dest = tmp.path().join("extracted");
    let completed = extract_archive(&zip_path, &dest, |p| p.files_done < 2).unwrap();
    assert!(!completed);
}

#[test]
fn archive_traversal_paths_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = tmp.path().join("evil.zip");
    make_zip(&zip_path, &[("../evil.txt", b"x" as &[u8])]);

    let dest = tmp.path().join("extracted");
    let err = extract_archive(&zip_path, &dest, |_| true).unwrap_err();
    assert!(err.to_string().contains("unsafe path"), "{err}");
    assert!(!tmp.path().join("evil.txt").exists());
}

#[test]
fn ingest_progress_reports_and_cancels() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    map_container(&src.path().join("Maps/campaign/tarcade.SC2Map"));
    let plan = plan_from_extracted(src.path()).unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();

    // Full run reports before every file: one file → one report at (0, 1).
    let mut reports = Vec::new();
    let rev = store
        .ingest_with_progress("tarcade", SlotId::LotV, &plan, |p| {
            reports.push((p.files_done, p.files_total));
            true
        })
        .unwrap()
        .expect("not cancelled");
    assert!(!rev.is_empty());
    assert_eq!(reports, vec![(0, 1)]);

    // Cancel before the first file yields Ok(None).
    let cancelled = store
        .ingest_with_progress("tarcade", SlotId::LotV, &plan, |_| false)
        .unwrap();
    assert_eq!(cancelled, None);
}
