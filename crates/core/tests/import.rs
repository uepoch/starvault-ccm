//! Bounded archive extraction and stable import DTOs.

use std::io::Write;
use std::path::Path;

use svccm_core::package::import::{
    extract_archive_with, preview_plan, ArchiveLimits, ImportOperationSnapshot,
    ImportOperationState,
};
use svccm_core::package::normalize::plan_from_extracted;

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

fn limits() -> ArchiveLimits {
    ArchiveLimits {
        max_entries: 10,
        max_file_bytes: 32,
        max_total_bytes: 64,
        max_path_bytes: 64,
        reserve_bytes: 16,
    }
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link: &Path) {
    junction::create(target, link).unwrap();
}

#[cfg(unix)]
fn remove_directory_link(link: &Path) {
    std::fs::remove_file(link).unwrap();
}

#[cfg(windows)]
fn remove_directory_link(link: &Path) {
    std::fs::remove_dir(link).unwrap();
}

#[cfg(any(unix, windows))]
fn assert_sentinel_only(directory: &Path) {
    assert_eq!(std::fs::read(directory.join("sentinel")).unwrap(), b"keep");
    assert_eq!(std::fs::read_dir(directory).unwrap().count(), 1);
}

#[test]
fn extracts_and_previews_a_valid_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = tmp.path().join("pkg.zip");
    make_zip(
        &zip_path,
        &[
            (
                "metadata.txt",
                b"title=My Cool Campaign\ncampaign=Legacy of the Void\n",
            ),
            ("MyCoolCampaign.SC2Map/payload.txt", b"map"),
        ],
    );

    let destination = tmp.path().join("extracted");
    assert!(extract_archive_with(
        &zip_path,
        &destination,
        ArchiveLimits::default(),
        |_| Ok(u64::MAX),
        |_| true,
    )
    .unwrap());
    let plan = plan_from_extracted(&destination).unwrap();
    let preview = preview_plan(&plan, Some("pkg"));
    assert_eq!(preview.suggested_id, "my-cool-campaign");
    assert_eq!(preview.slot_guess, "lotv");
    assert_eq!(preview.file_count, 1);
}

#[test]
fn cancellation_is_checked_mid_file_and_removes_scratch() {
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = tmp.path().join("large.zip");
    let content = vec![7_u8; 9 * 1024 * 1024];
    make_zip(&zip_path, &[("large.SC2Map/payload.bin", &content)]);
    let destination = tmp.path().join("extracted");
    let mut checks = 0;
    let completed = extract_archive_with(
        &zip_path,
        &destination,
        ArchiveLimits::default(),
        |_| Ok(u64::MAX),
        |_| {
            checks += 1;
            checks < 3
        },
    )
    .unwrap();
    assert!(!completed);
    assert_eq!(checks, 3, "9 MiB must cross more than two 4 MiB checks");
    assert!(!destination.exists());
}

#[test]
fn rejects_traversal_and_windows_separator_traversal() {
    for name in ["../escape.txt", "..\\escape.txt", "C:\\escape.txt"] {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("unsafe.zip");
        make_zip(&zip_path, &[(name, b"x")]);
        let destination = tmp.path().join("extracted");
        let error = extract_archive_with(
            &zip_path,
            &destination,
            limits(),
            |_| Ok(u64::MAX),
            |_| true,
        )
        .unwrap_err();
        assert_eq!(error.code(), "unsafe_archive_path");
        assert!(!destination.exists());
    }
}

#[test]
fn rejects_dos_device_path_segments_with_or_without_extensions() {
    for name in [
        "CON",
        "con.txt",
        "dir/PrN.bin",
        "AUX",
        "nul.dat",
        "COM1",
        "com9.SC2Map",
        "LPT1",
        "lpt9.txt",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("device-name.zip");
        make_zip(&zip_path, &[(name, b"x")]);
        let destination = tmp.path().join("extracted");
        let error = extract_archive_with(
            &zip_path,
            &destination,
            limits(),
            |_| Ok(u64::MAX),
            |_| true,
        )
        .unwrap_err();
        assert_eq!(error.code(), "unsafe_archive_path", "path `{name}`");
        assert!(!destination.exists());
    }
}

#[test]
fn rejects_windows_invalid_characters_and_ascii_controls() {
    for name in [
        "bad<name",
        "bad>name",
        "bad\"name",
        "bad|name",
        "bad?name",
        "bad*name",
        "bad\u{1f}name",
        "bad\u{7f}name",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("invalid-name.zip");
        make_zip(&zip_path, &[(name, b"x")]);
        let destination = tmp.path().join("extracted");
        let error = extract_archive_with(
            &zip_path,
            &destination,
            limits(),
            |_| Ok(u64::MAX),
            |_| true,
        )
        .unwrap_err();
        assert_eq!(error.code(), "unsafe_archive_path", "path `{name:?}`");
        assert!(!destination.exists());
    }
}

#[cfg(any(unix, windows))]
#[test]
fn linked_import_scratch_and_ancestors_are_rejected_without_external_writes() {
    for link_is_destination in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("pkg.zip");
        make_zip(&zip_path, &[("map.SC2Map/payload", b"map")]);
        let external = tmp.path().join("external");
        let linked = tmp.path().join("linked");
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("sentinel"), b"keep").unwrap();
        create_directory_link(&external, &linked);
        let destination = if link_is_destination {
            linked.clone()
        } else {
            linked.join("extracted")
        };

        let error = extract_archive_with(
            &zip_path,
            &destination,
            limits(),
            |_| Ok(u64::MAX),
            |_| true,
        )
        .unwrap_err();

        assert_eq!(error.code(), "invalid_import_scratch");
        assert_sentinel_only(&external);
        remove_directory_link(&linked);
    }
}

#[cfg(any(unix, windows))]
#[test]
fn cancellation_removes_an_injected_scratch_link_without_traversing_it() {
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = tmp.path().join("pkg.zip");
    make_zip(
        &zip_path,
        &[("first/member", b"one"), ("second/member", b"two")],
    );
    let destination = tmp.path().join("extracted");
    let external = tmp.path().join("external");
    std::fs::create_dir_all(&external).unwrap();
    std::fs::write(external.join("sentinel"), b"keep").unwrap();
    let mut injected = false;

    let completed = extract_archive_with(
        &zip_path,
        &destination,
        limits(),
        |_| Ok(u64::MAX),
        |progress| {
            if progress.files_done == 1 && !injected {
                std::fs::remove_file(destination.join("first/member")).unwrap();
                std::fs::remove_dir(destination.join("first")).unwrap();
                create_directory_link(&external, &destination.join("first"));
                injected = true;
                false
            } else {
                true
            }
        },
    )
    .unwrap();

    assert!(!completed);
    assert!(injected);
    assert!(!destination.exists());
    assert_sentinel_only(&external);
}

#[test]
fn accepts_names_that_only_resemble_dos_devices() {
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = tmp.path().join("safe-names.zip");
    make_zip(
        &zip_path,
        &[
            ("COM0.txt", b"zero"),
            ("COM10.txt", b"ten"),
            ("CONSOLE.txt", b"console"),
            ("LPT0.txt", b"printer"),
        ],
    );
    let destination = tmp.path().join("extracted");

    assert!(extract_archive_with(
        &zip_path,
        &destination,
        ArchiveLimits::default(),
        |_| Ok(u64::MAX),
        |_| true,
    )
    .unwrap());
    assert_eq!(
        std::fs::read(destination.join("CONSOLE.txt")).unwrap(),
        b"console"
    );
}

#[test]
fn enforces_entry_file_total_and_path_limits_before_writing() {
    let cases = [
        (
            vec![("one", b"x" as &[u8]), ("two", b"y" as &[u8])],
            ArchiveLimits {
                max_entries: 1,
                ..limits()
            },
        ),
        (
            vec![("one", b"xy" as &[u8])],
            ArchiveLimits {
                max_file_bytes: 1,
                ..limits()
            },
        ),
        (
            vec![("one", b"xy" as &[u8]), ("two", b"zw" as &[u8])],
            ArchiveLimits {
                max_total_bytes: 3,
                ..limits()
            },
        ),
        (
            vec![("too-long", b"x" as &[u8])],
            ArchiveLimits {
                max_path_bytes: 4,
                ..limits()
            },
        ),
    ];

    for (members, case_limits) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("limited.zip");
        make_zip(&zip_path, &members);
        let destination = tmp.path().join("extracted");
        assert!(extract_archive_with(
            &zip_path,
            &destination,
            case_limits,
            |_| Ok(u64::MAX),
            |_| true,
        )
        .is_err());
        assert!(!destination.exists());
    }
}

#[test]
fn rejects_insufficient_space_and_can_retry_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = tmp.path().join("pkg.zip");
    make_zip(&zip_path, &[("map.SC2Map/payload", b"1234")]);
    let destination = tmp.path().join("extracted");

    let error =
        extract_archive_with(&zip_path, &destination, limits(), |_| Ok(19), |_| true).unwrap_err();
    assert_eq!(error.code(), "insufficient_space");
    assert!(!destination.exists());

    assert!(
        extract_archive_with(&zip_path, &destination, limits(), |_| Ok(20), |_| true,).unwrap()
    );
    assert!(destination.join("map.SC2Map/payload").is_file());
}

#[test]
fn rejects_duplicate_and_file_directory_collision_paths() {
    for members in [
        vec![("A.txt", b"a" as &[u8]), ("a.TXT", b"b" as &[u8])],
        vec![("blocked", b"a" as &[u8]), ("blocked/child", b"b" as &[u8])],
        vec![
            ("blocked", b"a" as &[u8]),
            ("blocked-other", b"x" as &[u8]),
            ("blocked/child", b"b" as &[u8]),
        ],
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("collision.zip");
        make_zip(&zip_path, &members);
        assert!(extract_archive_with(
            &zip_path,
            &tmp.path().join("extracted"),
            limits(),
            |_| Ok(u64::MAX),
            |_| true,
        )
        .is_err());
    }
}

#[test]
fn import_state_serializes_with_the_frozen_vocabulary() {
    let states = [
        (ImportOperationState::Analyzing, "Analyzing"),
        (ImportOperationState::Ready, "Ready"),
        (ImportOperationState::Ingesting, "Ingesting"),
        (ImportOperationState::Cancelled, "Cancelled"),
        (ImportOperationState::Failed, "Failed"),
        (ImportOperationState::Completed, "Completed"),
    ];
    for (state, expected) in states {
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            format!("\"{expected}\"")
        );
    }
    let snapshot = ImportOperationSnapshot {
        op_id: "operation-1".into(),
        state: ImportOperationState::Failed,
        preview: None,
        revision: None,
        error_code: Some("archive_entry_limit".into()),
    };
    let value = serde_json::to_value(snapshot).unwrap();
    assert_eq!(value["state"], "Failed");
    assert_eq!(value["error_code"], "archive_entry_limit");
    assert!(value.get("preview").is_none());
}

#[test]
fn preview_never_suggests_an_invalid_package_id() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("a.SC2Map")).unwrap();
    std::fs::write(tmp.path().join("a.SC2Map/payload"), b"map").unwrap();
    let plan = plan_from_extracted(tmp.path()).unwrap();
    assert_eq!(
        preview_plan(&plan, Some("plain")).suggested_id,
        "imported-package"
    );
    assert_eq!(
        preview_plan(&plan, Some(&"a".repeat(100)))
            .suggested_id
            .len(),
        64
    );
}
