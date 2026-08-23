# Fresh alpha reset

The single-campaign store uses a new SQLite schema and package layout. The app
rejects an older alpha store instead of guessing how to convert active game
files.

Complete this procedure before installing the first build with the new schema:

1. Close StarCraft II and StarVault CCM.
2. Open the current StarVault build and restore every active faction to its
   plain state.
3. Verify that no StarVault junction and no StarVault-owned `Mods\` deployment
   remains in the StarCraft II directory.
4. Create a timestamped recovery directory outside StarVault's app-data
   directory.
5. Copy the complete current StarVault app-data directory into that recovery
   directory.
6. Copy the selected StarCraft II profile's `Saves` and `Banks` directories
   into the same recovery directory.
7. Compare the copied directories with their sources. Check that the expected
   files exist and that the copy completed without errors.
8. Delete the old StarVault app-data directory.
9. Start the new build and reimport each package.

Stop before step 8 if restoration, backup, or verification fails. Keep the old
app data and recovery copy until the new build has activated a package,
returned to vanilla, and preserved the expected saves successfully.
