# Task 12 — Video, frames, and synchronized action timeline

## Objective

Specify a crash-tolerant evidence bundle that connects every action to page snapshots, screenshots/frames, actionability, console, network, decisions, and approvals.

## Required local inputs

- `src/page/capture.rs`
- `src/page/events.rs`
- `src/page/input.rs`
- `src/cdp/conn.rs`
- `src/daemon.rs`
- `src/jobs.rs`
- `src/paths.rs`
- `docs/research/50-capture-screenshots-and-video.md`
- `docs/research/60-event-streams-console-network-performance.md`

## Required external research

- Official Playwright Trace Viewer and video documentation.
- Official CDP `Page.startScreencast` event/ack documentation.
- Current `ffmpeg-sidecar` license/features if recommended.

## Required questions

1. Define `brow-trace-v1` manifest and stable identifiers: job, session, action, frame, stream.
2. Map media timestamps and action boundaries onto Task 13's candidate clock/event model; do not create a second general event envelope.
3. Define frames-first crash recovery, screencast ACK, queue bounds, drop counters, and static-page duration.
4. Define retention modes `off`, `on`, and `retain-on-error` expressed through Task 08's candidate retention primitives.
5. Define optional muxing without making ffmpeg a hidden mandatory download.
6. Define redaction and access controls for visual artifacts.
7. Define an offline viewer only if it can remain a separate read-only deliverable.

## Task-specific deliverables

- Artifact directory layout and schemas.
- Recording state machine.
- Timeline join rules.
- Verification fixtures for concurrency, odd dimensions, crashes, and clock drift.

## Task-specific acceptance criteria

- Every completed action accepted while trace recording is enabled is joinable to before/after evidence by `action_id`. An interrupted action has before evidence plus `missing_after_reason` and an incomplete marker.
- Future implementation and verification are blocked on Task 06 scheduler isolation; state this dependency explicitly.
- Two concurrent sessions both record non-empty output.
- Dropped frames and timing uncertainty are visible.
- Crash leaves readable frames and a detectably incomplete manifest.
- No downloader runs implicitly during build or execution.
