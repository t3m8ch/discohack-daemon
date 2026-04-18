## Context

The current daemon is a thin FUSE-to-Yandex bridge. `src/main.rs` wires auth, the Yandex client, mount startup, and the D-Bus auth service; `src/fs.rs` keeps inode/path state entirely in memory; `src/yadisk.rs` exposes blocking metadata, list, download, upload, delete, and move helpers; `src/dbus_service.rs` only exposes `IsAuth`, `BeginLogin`, and `LoginCompleted`. There is no SQLite dependency, no persistent metadata store, and no durable queue.

That means the existing write-back staging is only locally temporary. A write can be edited in a local temp file, but the mounted state is still considered committed only after a remote upload finishes during `flush`, `fsync`, or `release`. Startup also requires live remote metadata for the root mount, so the filesystem cannot serve cached state after a disconnect or restart.

## Goals / Non-Goals

**Goals:**
- Make mounted reads and writes local-first so FUSE callbacks complete against local state rather than live network calls.
- Persist metadata, sync queue state, and conflict records in SQLite with centralized integer enum mappings.
- Run remote synchronization in a background worker with leases, retries, restart recovery, and high-level job coalescing.
- Detect remote-version conflicts before upload and preserve both versions by keeping the remote file at the original path and renaming the local conflicting copy with a numeric suffix.
- Expose bounded sync state over D-Bus through `SyncSummary`, `SyncItems`, and `PropertiesChanged`.
- Fit the new behavior into the existing daemon architecture instead of introducing a second control plane or a separate sync service.

**Non-Goals:**
- Smart file merge, CRDT/OT, or last-write-wins conflict resolution.
- Streaming file contents over D-Bus or publishing the full mounted tree there.
- Replacing FUSE, OAuth, or the existing Yandex client with a new network stack.
- Full UI work for GTK or GNOME Shell, which are not currently present in this repository.
- Perfect first-run offline bootstrap before any metadata has ever been cached locally.

## Decisions

### 1. Introduce one persistent local state layer shared by FUSE and the sync worker
The daemon will grow a small local-state subsystem, centered on a `LocalStore`/`SyncService` boundary, instead of scattering SQLite and cache-path logic through `fs.rs`. `main.rs` will initialize this state before mounting, pass it into the filesystem, and start a background sync worker alongside the existing D-Bus service.

`fs.rs` will stop calling `YandexDiskClient` for normal mutation flows. Instead, it will consult local metadata, serve cached bytes when present, and record high-level sync intent through the shared store. `yadisk.rs` remains the only place that knows Yandex HTTP details.

This keeps the architecture close to the current one: one daemon process, one mount, one D-Bus object, and one Yandex client, but with a durable local state layer between FUSE and the network.

### 2. Use SQLite plus a managed cache directory as the local source of truth
The mounted view will be backed by two persistent pieces of state:

- SQLite metadata in a daemon-managed database file.
- File bytes in a daemon-managed local cache directory, addressed by `file_id` and represented by placeholders until content is materialized.

The initial schema will include:

`files`
- `file_id TEXT PRIMARY KEY`
- `path TEXT NOT NULL UNIQUE`
- `sync_state INTEGER NOT NULL CHECK (...)`
- `remote_version TEXT`
- `local_version INTEGER NOT NULL DEFAULT 0`
- `mtime INTEGER`
- `size INTEGER NOT NULL DEFAULT 0`
- `hash BLOB`
- `content_status INTEGER NOT NULL CHECK (...)`
- `cache_rel_path TEXT`
- `is_deleted INTEGER NOT NULL DEFAULT 0 CHECK (is_deleted IN (0, 1))`

`operations_queue`
- `id INTEGER PRIMARY KEY AUTOINCREMENT`
- `file_id TEXT NOT NULL`
- `op_type INTEGER NOT NULL CHECK (...)`
- `op_status INTEGER NOT NULL CHECK (...)`
- `payload_json TEXT`
- `retry_count INTEGER NOT NULL DEFAULT 0`
- `next_retry_at INTEGER`
- `worker_id TEXT`
- `lease_expires_at INTEGER`
- `created_at INTEGER NOT NULL`
- `updated_at INTEGER NOT NULL`

`conflicts`
- `conflict_id TEXT PRIMARY KEY`
- `file_id TEXT NOT NULL`
- `original_path TEXT NOT NULL`
- `conflict_path TEXT NOT NULL`
- `created_at INTEGER NOT NULL`
- `base_remote_version TEXT`
- `current_remote_version TEXT`
- `origin_device TEXT`

SQLite enums will be represented in Rust with centralized `#[repr(i32)]` enums plus `TryFrom<i32>` decoding helpers. Unknown values will be hard failures, not silent fallbacks. The database will use WAL mode and explicit indices for pending/retryable queue selection and path lookups.

### 3. Keep mounted I/O local-first and use lazy hydration for remote content
FUSE clients should interact with local state first:

- `lookup`, `getattr`, and `readdir` use SQLite metadata and local cache state.
- `read` returns cached bytes if the file is hydrated locally.
- Placeholder files are allowed: metadata exists locally while bytes are fetched lazily on first open/read when network is available.
- `write`, `truncate`, `create`, `rename`, `mkdir`, and `unlink` update local state immediately and enqueue background sync work instead of calling the remote API inline.

For writable files, the existing staging behavior in `fs.rs` will be adapted so the staging file lives inside the managed cache rather than `env::temp_dir()`. On commit points such as `flush`, `fsync`, or `release`, the daemon will finalize local metadata and queue state, but it will not block on remote upload.

The practical atomicity rule is:
- file bytes are first durable in the managed cache
- metadata update and queue enqueue happen in one SQLite transaction
- if the daemon crashes after bytes are durable but before the transaction commits, recovery treats the staged file as uncommitted and either re-associates it or discards it explicitly

That is the smallest realistic design that preserves the local result without pretending SQLite can transactionally own arbitrary filesystem writes.

### 4. Model sync as high-level jobs, not raw FUSE events
The queue will store high-level sync intent rather than every low-level write callback. A high-level job is the latest remote action required to reconcile one logical file or directory path with Yandex Disk, for example `upload`, `delete`, `mkdir`, or `move`.

Coalescing rules:
- repeated local writes to the same file collapse into one pending `upload`
- `delete` replaces any pending `upload` for that file
- repeated renames/moves collapse to the latest destination as long as the operation has not already been leased for execution
- directory creation only queues one pending `mkdir` per path
- once a job is leased, later local mutations create a fresh follow-up job rather than mutating the in-flight one

This keeps queue growth bounded and aligned with the final desired remote state instead of FUSE call volume.

### 5. Run one background sync worker with leasing, retries, and restart recovery
The sync worker will poll SQLite for `pending` and retryable jobs whose `next_retry_at` has arrived, lease them with `worker_id` and `lease_expires_at`, execute the remote action, and then write back queue/file state.

Recovery behavior:
- startup resets expired leases so abandoned jobs become runnable again
- if metadata says a file is unsynced but no runnable queue row exists, startup reconstructs the appropriate high-level job
- retryable network/auth/server failures back off exponentially
- permanent API failures move the file and job into explicit error state visible over D-Bus

The worker is the only place allowed to perform remote mutations. FUSE callbacks may still trigger a lazy download for an unhydrated file, but network write/delete/move work leaves the request path entirely.

### 6. Detect conflicts from `remote_version` and preserve both versions
Each synced file row stores the last known remote version token. Before uploading, the worker fetches current remote metadata for the target path and compares the remote version with the stored base version.

If the versions match, upload proceeds and the daemon records the new remote version.

If the versions differ, the daemon does not overwrite the remote file. Instead it:

1. creates a conflict record
2. computes the next free conflict name using `basename (N).ext`
3. renames the local offline copy to that conflict path in local metadata/cache
4. keeps the canonical/original path mapped to the remote version
5. queues the conflict copy for upload under its suffixed path

Filename helper rules:
- `file.txt` -> `file (2).txt`
- `file (2).txt` -> `file (3).txt`
- `file` -> `file (2)`
- `archive.tar.gz` -> `archive.tar (2).gz`

The helper parses an existing trailing ` (N)` suffix before the final extension, increments it when present, and probes local metadata plus current directory state until it finds a free path.

### 7. Extend the D-Bus object with derived sync-state properties
The existing `ru.literallycats.daemon` object/interface will remain the control plane. It will gain two new properties:

- `SyncSummary: a{sv}`
- `SyncItems: aa{sv}`

`SyncSummary` is derived from queue/file/conflict state and includes:
- `active_count`
- `uploading_count`
- `downloading_count`
- `queued_count`
- `conflict_count`
- `error_count`
- `last_update_unix`
- `is_syncing`
- `attention_required`

`SyncItems` is a bounded projection of active items, conflicts, errors, and a small tail of recent completions. It intentionally does not expose raw queue internals or the entire file tree.

Every state change that affects either projection will emit the standard `org.freedesktop.DBus.Properties.PropertiesChanged` notification. No custom sync signal is needed.

### 8. Keep implementation incremental and compatible with the current repo
The repo currently has no GTK app or extension code, so this change only needs to provide the backend D-Bus contract and document how future clients can consume it. Likewise, the first offline-first version can rely on a successful initial online bootstrap to populate root metadata instead of inventing a synthetic remote tree from nothing.

## Risks / Trade-offs

- **SQLite is a new dependency and persistence boundary**: mitigated by isolating schema, migrations, enum decoding, and query helpers in one subsystem.
- **Current Yandex client does not expose revision/etag fields**: mitigated by extending metadata models first and falling back to the safest preflight comparison available if the API cannot supply a stronger conditional write primitive.
- **Lazy hydration still needs network when a cached file has no local bytes**: mitigated by making placeholders explicit and documenting that first access to never-downloaded content still needs connectivity.
- **Conflict handling mutates local paths after the user already edited the original path**: mitigated by keeping explicit conflict records and surfacing them through D-Bus so clients can explain what happened.
- **Background sync means FUSE success no longer guarantees remote durability**: mitigated by exposing sync status clearly and making failure/conflict state durable and observable.

## Migration Plan

1. Add SQLite and the local cache directory with forward-only schema bootstrap/migrations.
2. Start the daemon with local store initialization and worker startup before FUSE mount.
3. Refactor FUSE reads/writes/mutations to use local state and queue background jobs.
4. Extend the Yandex client with remote-version metadata and any missing worker-facing primitives.
5. Add D-Bus sync properties and emit `PropertiesChanged` from derived sync-state updates.
6. Backfill tests and documentation for the new behavior.

Rollback is operationally simple: disable the new local-state path and return to the previous online-first semantics, but local SQLite/cache data can be kept for investigation rather than deleted.

## Open Questions

- Which Yandex Disk metadata field is the best durable `remote_version` in practice: revision, etag, resource ID + modified timestamp, or another API-specific token?
- Do move/delete endpoints require explicit async-operation polling in the worker to avoid racing follow-up metadata refreshes?
- Should first mount without cached metadata remain auth-gated, or should the daemon mount an empty local root and let background discovery fill it later?
