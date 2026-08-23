# Code quality review — 2026-08-23

Sources: three parallel reviewer agents (core, app, UI), full project
diagnostics (LSP, ast-grep, jscpd, zizmor, opengrep), manual triage.
Every finding below is triaged: real issue → action point; false positive →
accepted convention with rationale. No repetitions: one line per point.

## Conventions accepted (non-issues, do not re-report)

- `.lock().expect("… poisoned")` for every mutex: the documented Rust
  convention for poisoned-lock guards (a panic on poison is correct — state
  is untrustworthy after). Lens flags these; they stay.
- `unwrap()` inside `#[cfg(test)]` modules (container, mpq, docinfo, header,
  config, normalize tests): tests fail loudly on purpose.
- `tsconfig.*.json` "comments not permitted": tsconfig is JSONC; the JSON
  language server is wrong, the files are valid.
- jscpd duplicates across `container.rs` / `mpq.rs` / `docinfo.rs`: test
  fixture helpers, `#[cfg(test)]` only.

## Action points

Each point gets its own commit. Status: ☐ todo / ☑ done (commit).

### A. CI / release hardening (from zizmor + opengrep)

- ☑ A1 (fa35a28→amended, this commit) — `release.yml` L22: `inputs.version` interpolated directly in a
  `run:` step (template injection). Pass through an `env:` variable.
- ☐ A2 — Actions pinned by tag, not SHA (checkout, rust-cache, toolchain,
  setup-vp, gh-release, configure-pages…). Pin the ones handling secrets to
  full SHAs.
- ☐ A3 — `persist-credentials: false` on checkouts (artipacked) in both
  workflows.
- ☐ A4 — Least-privilege `permissions:` blocks per job (contents: read for
  CI; only the release job needs write).

### B. Rust core

- ☐ B1 — (from reviewer) …to be merged
- ☐ B2 — …

### C. Tauri app layer

- ☐ C1 — (from reviewer) …to be merged

### D. UI

- ☐ D1 — (from reviewer) …to be merged

### E. Docs

- ☐ E1 — `docs/design/research-save-followups.md` typos ("Fo" ×2).
