# Offline-First Sync

## Overview

The daemon now uses an offline-first architecture:

- FUSE works against local metadata and local cached files.
- SQLite stores file metadata, sync state, queued operations, and conflicts.
- Background workers perform remote refresh, upload, delete, rename, mkdir, and lazy download work.
- Local writes complete before remote synchronization.
- After restart the daemon recovers queued work from SQLite and continues syncing.

State root:

- default: `${XDG_STATE_HOME:-$HOME/.local/state}/discohack-daemon/`
- metadata DB: `metadata.db`
- local content cache: `cache/`

## SQLite Schema

### `files`

- `file_id TEXT PRIMARY KEY`
- `path TEXT NOT NULL UNIQUE`
- `parent_path TEXT`
- `name TEXT NOT NULL`
- `kind INTEGER NOT NULL`
- `sync_state INTEGER NOT NULL`
- `content_state INTEGER NOT NULL`
- `remote_version TEXT`
- `local_version INTEGER NOT NULL DEFAULT 0`
- `mtime INTEGER`
- `size INTEGER NOT NULL DEFAULT 0`
- `hash BLOB`
- `cache_path TEXT`
- `last_remote_check_at INTEGER`
- `remote_deleted INTEGER NOT NULL DEFAULT 0`
- `last_error TEXT`

### `operations_queue`

- `id INTEGER PRIMARY KEY AUTOINCREMENT`
- `file_id TEXT NOT NULL`
- `op_type INTEGER NOT NULL`
- `op_status INTEGER NOT NULL`
- `payload_json TEXT`
- `retry_count INTEGER NOT NULL DEFAULT 0`
- `next_retry_at INTEGER`
- `worker_id TEXT`
- `lease_expires_at INTEGER`
- `created_at INTEGER NOT NULL`
- `updated_at INTEGER NOT NULL`

### `conflicts`

- `conflict_id TEXT PRIMARY KEY`
- `file_id TEXT NOT NULL`
- `original_path TEXT NOT NULL`
- `conflict_path TEXT NOT NULL`
- `created_at INTEGER NOT NULL`
- `base_remote_version TEXT`
- `current_remote_version TEXT`
- `origin_device TEXT`

## Integer Enum Mapping

All DB enums are centralized in `src/sync.rs` with `#[repr(i32)]` and `TryFrom<i32>` validation.

### `NodeKind`

- `0 = file`
- `1 = directory`

### `SyncState`

- `0 = synced`
- `1 = queued_upload`
- `2 = uploading`
- `3 = downloading`
- `4 = conflict`
- `5 = queued_delete`
- `6 = error`
- `7 = placeholder`

### `ContentState`

- `0 = placeholder`
- `1 = hydrated`

### `QueueOpType`

- `0 = upload`
- `1 = delete`
- `2 = mkdir`
- `3 = move`
- `4 = rename`
- `5 = refresh_tree`
- `6 = refresh_dir`
- `7 = download`
- `8 = reconcile_remote_delete`

### `QueueOpStatus`

- `0 = pending`
- `1 = leased`
- `2 = done`
- `3 = retryable_error`
- `4 = permanent_error`
- `5 = conflict`

## Sync State Machine

Typical local write flow:

1. FUSE writes to a local staging file.
2. On commit, the daemon copies the staged bytes into the local cache.
3. Metadata is updated in SQLite.
4. A high-level upload job is upserted into `operations_queue`.
5. Worker leases the job and moves the file through `queued -> uploading -> synced` or `error/conflict`.

Typical lazy read flow:

1. File metadata may exist in SQLite as a placeholder.
2. First read hydrates local content into `cache/`.
3. `content_state` becomes `hydrated`.

Typical refresh flow:

1. Startup queues `refresh_tree`.
2. Background periodic refresh also queues `refresh_tree`.
3. Manual D-Bus refresh queues `refresh_tree` or `refresh_dir`.
4. Remote metadata is reconciled into SQLite.
5. Changed remote files invalidate local cache unless local unsynced work exists.

## Conflict Policy

The daemon does not use merge or last-write-wins.

- Remote canonical version keeps the original path.
- Local unsynced version is renamed with a numeric suffix.
- Example: `file.txt -> file (2).txt`
- Existing suffixes are incremented: `file (2).txt -> file (3).txt`
- Compound extensions are preserved for common archive formats such as `.tar.gz`

Conflicts are recorded in the `conflicts` table and exposed through D-Bus summary/items/status methods.

## Remote Version Strategy

`src/yadisk.rs` derives `remote_version` from the best provider field available in this order:

1. `revision`
2. `sha256`
3. `md5`
4. `modified`
5. `resource_id`

If Yandex Disk does not provide a strong conditional-write primitive for a given object, the daemon uses this client-side compare-before-write baseline and preserves both copies on mismatch.

## D-Bus API

Service:

- service: `ru.literallycats.daemon`
- path: `/ru/literallycats/daemon`
- interface: `ru.literallycats.daemon`

### Auth Surface

- property `IsAuth: b`
- method `BeginLogin()`
- signal `LoginCompleted`

### Sync Surface

- property `MountPoint: s`
- property `SyncSummary: a{sv}`
- property `SyncItems: aa{sv}`
- method `GetSyncStatus(path: s) -> a{sv}`
- method `ListDirectoryStatuses(path: s) -> aa{sv}`
- method `RequestRefresh(path: s)`

`SyncSummary` fields:

- `active_count`
- `uploading_count`
- `downloading_count`
- `queued_count`
- `conflict_count`
- `error_count`
- `last_update_unix`
- `is_syncing`
- `attention_required`

Per-path status fields:

- `path`
- `name`
- `kind`
- `state`
- `direction`
- `is_conflicted`
- `is_placeholder`
- `progress`
- `bytes_done`
- `bytes_total`
- `updated_at`
- `known`

Global sync property changes are emitted through standard `org.freedesktop.DBus.Properties.PropertiesChanged`.

## Current Compromises

- Refresh uses a safe periodic full-tree baseline, not a provider-native delta feed.
- Lazy hydration still blocks the requesting filesystem operation until the file is downloaded.
- Queue state currently uses one shared SQLite connection behind a mutex for simplicity.
- The repo still has no GTK or GNOME extension frontend; D-Bus hooks are prepared for future consumers.
