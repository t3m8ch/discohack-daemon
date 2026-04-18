## 1. Add persistent local state primitives

- [x] 1.1 Add SQLite support, daemon state-root bootstrap, and non-destructive migrations for metadata and queue tables.
- [x] 1.2 Introduce centralized integer-backed Rust enums for file sync state, content state, queue op type, and queue op status, including `TryFrom<i32>` validation.
- [x] 1.3 Add a small storage layer that localizes SQL, transactions, and cache-path bookkeeping instead of spreading DB details through FUSE and D-Bus code.

## 2. Move the filesystem to local-first behavior

- [x] 2.1 Refactor `src/fs.rs` so lookup/getattr/readdir use SQLite-backed metadata and local cache state as the primary source of truth.
- [x] 2.2 Replace direct remote write/delete/rename commit paths with local mutation flows that atomically update cache files, metadata, and queued sync intent.
- [x] 2.3 Add lazy hydration for file content reads and explicit placeholder/content-state handling for files whose metadata exists before bytes are downloaded.

## 3. Implement the persistent queue and worker runtime

- [x] 3.1 Add high-level queue upsert/coalescing rules for upload, delete, mkdir, move, rename, refresh, download, and reconcile jobs.
- [x] 3.2 Implement lease-based worker execution, retry/backoff, and restart recovery for expired leases and unsynced metadata.
- [x] 3.3 Wire worker startup and shutdown into `src/main.rs` so network work runs outside FUSE callbacks and resumes automatically after daemon restart.

## 4. Add remote discovery and reconciliation flows

- [x] 4.1 Implement bootstrap refresh on startup plus periodic, network-restored, manual, and optional stale-on-access refresh scheduling.
- [x] 4.2 Update metadata when remote objects change, invalidate or rehydrate local content cache appropriately, and enqueue downloads only when bytes are actually needed.
- [x] 4.3 Handle remote deletions safely, including explicit remote-delete conflicts when local unsynced content exists.

## 5. Add conflict detection and preservation logic

- [x] 5.1 Choose and centralize the safest available `remote_version` strategy in the Yandex client and worker flow.
- [x] 5.2 Implement conflict detection for upload and remote-delete reconciliation, store conflict records, and surface clear sync/error states.
- [x] 5.3 Add the numeric-suffix conflict filename helper and ensure it handles plain files, existing suffixed names, files without extensions, and compound extensions.

## 6. Extend D-Bus sync visibility without breaking auth clients

- [x] 6.1 Add `MountPoint`, `SyncSummary`, and bounded `SyncItems` properties derived from SQLite state.
- [x] 6.2 Add `GetSyncStatus(path)` and `ListDirectoryStatuses(path)` methods with explicit unknown-path results for files and directories.
- [x] 6.3 Emit `PropertiesChanged` updates whenever aggregate sync state changes and document how future GTK/extension clients should consume the new API.

## 7. Verify behavior with focused tests

- [x] 7.1 Add tests for offline local write enqueue, successful upload completion, queue coalescing, and restart recovery of pending/expired leased jobs.
- [x] 7.2 Add tests for remote-version conflict detection, remote-delete conflict handling, conflict record creation, and numeric-suffix filename generation.
- [x] 7.3 Add tests for D-Bus summary/items projections and per-path directory-status methods, plus any needed store-level migration tests.

## 8. Update documentation

- [x] 8.1 Update `README.md` with the offline-first architecture, cache/queue behavior, conflict policy, and current limitations.
- [x] 8.2 Add or update docs describing the sync state machine, SQLite schema, enum mapping, and D-Bus API surface.
