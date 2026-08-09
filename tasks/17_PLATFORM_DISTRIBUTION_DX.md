# Task 17 — Platforms, browser provisioning, distribution, and diagnostics

## Objective

Make installation and operation credible for early adopters without weakening local-first defaults. Define supported OS/browser matrix, prebuilt releases, reproducible browser option, `doctor`, CI, and failure diagnostics.

## Required local inputs

- `Cargo.toml`
- `src/browser/discover.rs`
- `src/browser/launch.rs`
- `src/paths.rs`
- `src/main.rs`
- `README.md`

## Required external research

- Official Chrome for Testing repository/endpoints.
- Official GitHub Actions artifact/provenance docs.
- Homebrew formula requirements.
- Rust cross-compilation/release tooling primary docs.
- Competitor installation pages for Playwright CLI, agent-browser, and gsd-browser.

## Required questions

1. Define tiered platform support: macOS ARM/x64, Linux ARM/x64, Windows x64, WSL.
2. Compare installed-browser default with opt-in pinned Chrome for Testing.
3. Define `brow doctor` read-only checks and separate `--fix` mutations.
4. Define binary release, checksums, signatures/attestations, SBOM, update policy, rollback, and offline install.
5. Define browser-version compatibility and deprecation policy.
6. Identify Windows pipe/process/socket design work without pretending it is a packaging-only task.
7. Define actionable diagnostics and machine-readable health output.

## Task-specific deliverables

- Platform/release matrix.
- Installer and doctor command specification.
- CI/release pipeline proposal.
- Windows architecture gap list.

## Task-specific acceptance criteria

- Read-only `doctor` does not mutate `BROW_HOME`, project tree, browser profiles, or network; verify with fresh temporary roots and a filesystem diff.
- Downloads require explicit action, exact checksum verification, and zip-slip-safe extraction.
- User-installed browsers are never deleted or modified.
- Every released artifact has version, platform, checksum, provenance, and supported-browser declaration.
