## 1. Add persistent local state and schema

- [x] 1.1 Add a SQLite dependency and initialize a daemon-managed database plus cache directory during startup.
- [x] 1.2 Implement forward-only migrations for `files`, `operations_queue`, and `conflicts`, including indices for pending job leasing and path lookups.
- [x] 1.3 Introduce centralized Rust enum mappings for sync state, content state, operation type, and operation status using integer-backed enums with explicit decode validation.

## 2. Refactor the filesystem to local-first behavior

- [x] 2.1 Replace the current in-memory-only source of truth with local metadata/cache reads for `lookup`, `getattr`, `readdir`, and normal file reads.
- [x] 2.2 Move writable staging from temporary files into the managed local cache so local edits survive restart and can be read back immediately.
- [x] 2.3 Change `write`, `truncate`, `create`, `mkdir`, `unlink`, and `rename` flows to update local state first and enqueue high-level sync intent instead of performing remote mutations inline.

## 3. Extend Yandex Disk integration for worker-driven sync

- [x] 3.1 Extend `src/yadisk.rs` metadata models to expose the remote version token needed for safe conflict detection.
- [x] 3.2 Add or tighten worker-facing helpers for upload, delete, mkdir, move, lazy download, and async-operation completion handling where Yandex requires it.
- [x] 3.3 Keep auth refresh and HTTP error mapping centralized in the client so worker and filesystem code do not duplicate transport logic.

## 4. Implement the persistent queue and sync worker

- [x] 4.1 Implement queue enqueue/coalescing rules so repeated writes collapse into one upload intent and delete supersedes stale uploads.
- [x] 4.2 Add worker leasing, lease expiry recovery, retry/backoff, and startup repair for unsynced metadata that lacks a runnable queue row.
- [x] 4.3 Update file sync state, queue state, and derived D-Bus sync projections after every worker transition.

## 5. Implement conflict detection and conflict-copy naming

- [x] 5.1 Add a helper that generates `basename (N).ext` conflict names, including existing suffix incrementation, extension handling, and collision probing.
- [x] 5.2 Detect upload conflicts from `remote_version` mismatch before overwrite and refuse silent remote replacement.
- [x] 5.3 Create durable conflict records and rename the offline copy to the suffixed path while preserving the original path for the authoritative remote version.

## 6. Expose sync state over D-Bus

- [x] 6.1 Extend the existing `ru.literallycats.daemon` interface with `SyncSummary: a{sv}` and `SyncItems: aa{sv}` properties.
- [x] 6.2 Emit standard `org.freedesktop.DBus.Properties.PropertiesChanged` updates whenever derived sync state changes.
- [x] 6.3 Document the new property contract so future GTK app or GNOME extension clients can consume it without needing raw queue access.

## 7. Verify behavior and update documentation

- [x] 7.1 Add tests for offline local writes, restart recovery, successful upload completion, queue coalescing, and D-Bus summary/item projections.
- [x] 7.2 Add conflict-focused tests covering remote-version mismatch, conflict record creation, and numeric suffix naming edge cases.
- [x] 7.3 Update `README.md` and relevant docs with the offline-first architecture, sync state machine, SQLite schema, D-Bus properties, and known limitations.
