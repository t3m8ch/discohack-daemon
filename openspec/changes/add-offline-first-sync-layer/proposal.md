## Why

The daemon is still online-first. `src/fs.rs` resolves metadata, downloads file bytes, and commits writes directly against Yandex Disk from FUSE callbacks, so local work depends on network availability and in-flight remote mutations. The repo also has no persistent metadata store, no crash-safe sync queue, and the D-Bus surface in `src/dbus_service.rs` only exposes OAuth state.

That makes the current mount usable for simple connected workflows, but it does not satisfy the expected behavior of a desktop sync client: local writes should complete immediately, offline work must survive daemon restarts, remote refresh must continue in the background, and conflicts must preserve both versions instead of silently overwriting data.

## What Changes

- Introduce a local-first sync layer backed by on-disk cache files plus SQLite metadata and persistent operations queue.
- Refactor the FUSE filesystem to read and write the local projection first, enqueue high-level sync jobs, and keep network work out of latency-sensitive callbacks.
- Add a background sync scheduler/worker that handles local-to-remote uploads, deletes, renames, remote discovery, lazy downloads, retries, and lease recovery after restart.
- Add conflict detection based on remembered remote version and preserve both versions by creating a suffixed local conflict copy such as `file (2).ext`.
- Extend D-Bus beyond auth-only control with sync summary, bounded sync items, mount-point discovery, and per-path/per-directory status queries.
- Add database migrations, enum mapping, tests, and documentation for the offline-first architecture and sync state machine.

## Capabilities

### New Capabilities
- `sync-state-dbus`: expose aggregate and per-path sync state for frontends without leaking raw queue internals.

### Modified Capabilities
- `yandex-disk-readonly-fuse`: change the current online-first writable mount into a local-first filesystem backed by persistent metadata, local cache, and asynchronous synchronization.

## Impact

- Affected code: `src/fs.rs`, `src/main.rs`, `src/mount.rs`, `src/yadisk.rs`, `src/dbus_service.rs`, plus new storage/sync modules for SQLite state, queue processing, and local cache management.
- New dependencies: SQLite access and migration support, plus small helpers for stable enum decoding and background worker orchestration.
- Data model: introduce persistent `files`, `operations_queue`, and conflict-tracking tables with integer-backed enums and restart-safe leases.
- User-visible behavior: filesystem reads and writes become local-first, offline work survives restart, remote changes are discovered asynchronously, and clients can inspect sync health over D-Bus.
