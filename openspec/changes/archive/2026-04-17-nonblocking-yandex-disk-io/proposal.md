## Why

Yandex Disk metadata and file-download requests currently run inline inside FUSE callbacks while holding shared filesystem state, so one slow remote operation can stall unrelated requests. We need the mount to remain responsive during remote I/O so directory traversal, attribute checks, and independent reads are not blocked behind a single Yandex Disk call.

## What Changes

- Allow FUSE request handling to continue while Yandex Disk metadata or file-content requests are in flight instead of serializing all remote I/O through one blocking critical section.
- Refactor filesystem state access so network operations do not hold the global filesystem mutex for the full duration of the request.
- Increase FUSE request-processing concurrency so the daemon can serve multiple independent operations at once on Linux.
- Preserve existing read-only behavior, inode stability, metadata caching, and error translation while introducing concurrent request handling.
- Add validation focused on responsiveness under concurrent operations, including a slow remote read not preventing other lookups, getattr calls, or directory reads from completing.

## Capabilities

### New Capabilities

### Modified Capabilities
- `yandex-disk-readonly-fuse`: remote reads and metadata fetches should not block unrelated FUSE requests from being served while the filesystem is mounted.

## Impact

- Affected code: `src/main.rs`, `src/fs.rs`, and possibly `src/yadisk.rs` depending on how remote I/O is separated from shared state.
- Runtime behavior: the daemon will process multiple FUSE requests concurrently instead of effectively serializing them behind long Yandex Disk calls.
- Dependencies: may use existing standard-threading facilities and current blocking HTTP client; no runtime switch is required unless implementation dictates otherwise.
- Validation: needs concurrency-focused manual or automated checks to confirm one slow Yandex Disk operation no longer freezes unrelated filesystem activity.
