# Task 20 — Honest tiled full-page capture

## Objective

Design full-page capture beyond Chromium's 16,384 output-pixel axis ceiling without silent row duplication, missing content, or a falsely complete image.

## Required local inputs

- `src/page/capture.rs`
- `src/page/mod.rs`
- `src/page/input.rs`
- `tests/browser_e2e.rs`
- `docs/research/50-capture-screenshots-and-video.md`

## Required external research

Official current CDP screenshot/layout metrics documentation. Research image stitching crates only if recommending one, including license and memory behavior.

## Required questions

1. Define a tile planner in output-device-pixel coordinates for 1× and 2× DPR.
2. Define capture strategy for sticky/fixed elements, lazy content, scroll effects, and page mutation.
3. Decide stitched image versus tile manifest and explicit fallback semantics.
4. Define overlap/integrity verification and resource bounds.
5. Preserve the current fast clamped mode as an explicitly partial option if useful.
6. Define interaction with concurrent session scheduling and artifact storage.

## Task-specific deliverables

- Candidate tile/stitch algorithm.
- Artifact/manifest schema requirements.
- Failure taxonomy and CLI behavior.
- Fixture and golden-test matrix.

## Task-specific acceptance criteria

- Markers above and below 16,384 output pixels are present.
- No duplicated top rows or hidden seam corruption.
- 1× and 2× DPR cases are covered.
- An impossible honest stitch returns tiles plus explicit partial/inconsistent state, never a fake full-page success.
