## Why

The daemon is currently online-first. `src/fs.rs` serves reads from Yandex Disk metadata and download URLs, and writable handles still require remote upload on `flush`, `fsync`, or `release`. The project also has no persistent metadata or queue layer yet, so local mutation intent, sync status, and conflict context are lost across restart.

That model makes normal offline use impossible: network latency leaks into FUSE callbacks, local edits cannot be committed safely without the cloud, and a remote version change can only be handled as an immediate write failure. The next step is to turn the mount into a local-first filesystem with durable background sync and explicit sync telemetry.

## What Changes

- Add a persistent local state layer backed by SQLite for file metadata, sync state, and queued operations, plus a managed on-disk content cache used as the client-visible source of truth.
- Refactor FUSE mutation paths so local file changes complete against the local cache first, then atomically update metadata and enqueue high-level sync jobs for background processing.
- Add a crash-safe sync worker with SQLite-backed leases, retry/backoff, queue recovery on restart, and job coalescing so repeated writes do not explode into low-level queue spam.
- Extend Yandex Disk integration to track remote version metadata, detect upload conflicts safely, and preserve both versions by renaming the offline copy to `basename (N).ext` before syncing it as a separate file.
- Extend the D-Bus interface with `SyncSummary` and `SyncItems` properties and standard `PropertiesChanged` updates so clients can observe active sync, conflicts, and errors.
- Add tests and documentation for offline write behavior, restart recovery, conflict handling, queue coalescing, and D-Bus sync state reporting.

## Capabilities

### New Capabilities
- `sync-state-dbus`: expose bounded background-sync summary and item state over the existing daemon D-Bus interface.

### Modified Capabilities
- `yandex-disk-readonly-fuse`: evolve the mount from immediate remote write-through semantics to an offline-first local filesystem with durable eventual sync.

## Impact

- Affected code: `src/main.rs`, `src/mount.rs`, `src/fs.rs`, `src/yadisk.rs`, `src/dbus_service.rs`, plus new local-state, queue, and conflict-management modules.
- New storage: SQLite schema and migrations for `files`, `operations_queue`, and conflict metadata, plus a managed local cache directory for file contents and placeholders.
- External API impact: the daemon will depend on Yandex Disk revision/etag metadata when available and otherwise use the safest client-side preflight conflict check it can implement.
- User-visible behavior: reads and writes continue against local state while offline, sync proceeds in the background when connectivity returns, and conflicts preserve both versions instead of silently overwriting remote data.
