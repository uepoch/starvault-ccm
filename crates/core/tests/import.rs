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
    let preview = preview_plan(&plan, Some("pkg.zip"));
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

#[test]
fn union_replaces_leftover_packed_mod_file_with_directory_form() {
    // Old installs (old CCM, manual unzips) leave packed .SC2Mod FILES where a
    // package ships an unpacked directory tree. The union must replace the
    // stale form instead of failing with os error 183.
    use svccm_core::layout::SlotId;
    use svccm_core::store::Store;

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();

    // A package with an unpacked RaynorRogueRaw.SC2Mod directory (2 files).
    let src = tmp.path().join("src");
    let raw = src.join("Mods/RaynorRogueRaw.SC2Mod");
    std::fs::create_dir_all(&raw).unwrap();
    std::fs::write(raw.join("a.xml"), b"a").unwrap();
    std::fs::write(raw.join("b.xml"), b"b").unwrap();
    let plan = plan_from_extracted(&src).unwrap();
    let rev = store.ingest("pkg", SlotId::Wol, &plan).unwrap();
    let manifest = store.load_manifest("pkg", &rev).unwrap();

    // The game Mods dir already has RaynorRogueRaw.SC2Mod as a packed FILE.
    let mods_dir = tmp.path().join("game/Mods");
    std::fs::create_dir_all(&mods_dir).unwrap();
    std::fs::write(mods_dir.join("RaynorRogueRaw.SC2Mod"), b"packed-leftover").unwrap();

    let refs = [&manifest];
    let (union, conflicts) = store.plan_mods_union(&refs);
    assert!(conflicts.is_empty());
    store.apply_mods_union(&union, &mods_dir).unwrap();

    // The leftover file is gone; the unpacked tree is live.
    let target = mods_dir.join("RaynorRogueRaw.SC2Mod");
    assert!(target.is_dir());
    assert!(target.join("a.xml").is_file());
    assert!(target.join("b.xml").is_file());
}

/// Package fixture: a map under Maps/campaign plus mod containers under Mods.
fn union_pkg(src: &Path, map: &str, mods: &[(&str, &[u8])]) {
    map_container(&src.join("Maps/campaign").join(map));
    for (name, content) in mods {
        let dir = src.join("Mods").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.txt"), content).unwrap();
    }
}

#[test]
fn restore_and_replace_prune_orphaned_mods_files() {
    // The game's Mods\ is a global namespace: restoring (or replacing) a
    // package must remove its union files that no active package owns,
    // keep files still owned by others, and prune emptied container dirs.
    use svccm_core::layout::{SlotId, WindowsLayout};
    use svccm_core::slots::SlotManager;
    use svccm_core::store::Store;

    let tmp = tempfile::tempdir().unwrap();
    let layout = WindowsLayout::new(tmp.path().join("sc2"));
    std::fs::create_dir_all(layout.mods_dir()).unwrap();
    std::fs::create_dir_all(layout.slot_dir(SlotId::LotV)).unwrap();
    std::fs::create_dir_all(layout.slot_dir(SlotId::HotS)).unwrap();
    let store = Store::open(tmp.path().join("store")).unwrap();
    let manager = SlotManager::new(&layout, &store);
    let mods = layout.mods_dir();

    let src_a = tempfile::tempdir().unwrap();
    union_pkg(
        src_a.path(),
        "aaa.SC2Map",
        &[("Shared.SC2Mod", b"s"), ("OnlyA.SC2Mod", b"a")],
    );
    let rev_a = store
        .ingest(
            "aaa",
            SlotId::LotV,
            &plan_from_extracted(src_a.path()).unwrap(),
        )
        .unwrap();

    let src_b = tempfile::tempdir().unwrap();
    union_pkg(
        src_b.path(),
        "bbb.SC2Map",
        &[("Shared.SC2Mod", b"s"), ("OnlyB.SC2Mod", b"b")],
    );
    let rev_b = store
        .ingest(
            "bbb",
            SlotId::HotS,
            &plan_from_extracted(src_b.path()).unwrap(),
        )
        .unwrap();

    // A active: its files deploy.
    manager.activate(SlotId::LotV, "aaa", &rev_a).unwrap();
    assert!(mods.join("Shared.SC2Mod/x.txt").is_file());
    assert!(mods.join("OnlyA.SC2Mod/x.txt").is_file());

    // B joins: shared content coexists, both own files present.
    manager.activate(SlotId::HotS, "bbb", &rev_b).unwrap();
    assert!(mods.join("OnlyB.SC2Mod/x.txt").is_file());

    // Restoring A removes OnlyA (and its emptied dir), keeps Shared (B owns it).
    manager.restore(SlotId::LotV).unwrap();
    assert!(!mods.join("OnlyA.SC2Mod/x.txt").exists());
    assert!(!mods.join("OnlyA.SC2Mod").exists());
    assert!(mods.join("Shared.SC2Mod/x.txt").is_file());

    // Restoring B clears the last owner; the union shrinks to nothing.
    manager.restore(SlotId::HotS).unwrap();
    assert!(!mods.join("Shared.SC2Mod/x.txt").exists());

    // Replacing a package prunes the displaced one's exclusive files too.
    let src_c = tempfile::tempdir().unwrap();
    union_pkg(src_c.path(), "ccc.SC2Map", &[("OnlyC.SC2Mod", b"c")]);
    let rev_c = store
        .ingest(
            "ccc",
            SlotId::LotV,
            &plan_from_extracted(src_c.path()).unwrap(),
        )
        .unwrap();
    manager.activate(SlotId::LotV, "ccc", &rev_c).unwrap();
    assert!(mods.join("OnlyC.SC2Mod/x.txt").is_file());

    let src_d = tempfile::tempdir().unwrap();
    union_pkg(src_d.path(), "ddd.SC2Map", &[("OnlyD.SC2Mod", b"d")]);
    let rev_d = store
        .ingest(
            "ddd",
            SlotId::LotV,
            &plan_from_extracted(src_d.path()).unwrap(),
        )
        .unwrap();
    manager.activate(SlotId::LotV, "ddd", &rev_d).unwrap();
    assert!(!mods.join("OnlyC.SC2Mod/x.txt").exists());
    assert!(mods.join("OnlyD.SC2Mod/x.txt").is_file());
}
