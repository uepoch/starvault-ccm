# Release process

Run releases from a clean `main` checkout:

```sh
scripts/release.sh vX.Y.Z
```

The helper checks for a changelog entry, compares the requested version with
the Cargo workspace and Tauri configuration, runs `scripts/check.sh`, pushes
the current commit to `main`, then dispatches the release workflow.

The workflow keeps signing credentials in the final Windows job. That job
cannot start until metadata, Rust, frontend, and dependency-audit jobs pass.
`latest.json` is composed after the signed installer exists and is uploaded in
the same final release step. The composer renames the installer and signature
to a fixed GitHub-safe filename, then uses that exact filename in the manifest.
Its Windows regression test rejects a mismatch before the release build. A
failed gate cannot publish an updater manifest.

## Dependency audit policy

JavaScript auditing uses `vp pm audit -- --prod --audit-level high`. Rust auditing
uses cargo-audit against `Cargo.lock`.

The project has one temporary RustSec exception in `.cargo/audit.toml`:
`RUSTSEC-2023-0071` for `rsa 0.9.10`. `wow-mpq` depends on that crate and also
contains a signature-generation API, but StarVault only calls `Archive::open`
and read operations. It performs no private-key operation, and the weak
Blizzard key included by the dependency is public. RustSec reports no fixed
upgrade. Remove the exception as soon as `wow-mpq` moves to a fixed RSA
implementation.
