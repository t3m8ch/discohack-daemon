## Context

The current daemon is centered around a synchronous FUSE implementation in `src/fs.rs`. Even after the earlier write-back change, the filesystem still treats Yandex Disk as the source of truth for most operations: metadata refresh, download URL resolution, file reads, upload commit, `mkdir`, `unlink`, and `rename` all call the remote client directly. `src/main.rs` starts auth, callback server, D-Bus, and mount lifecycle, but there is no persistent runtime state beyond Secret Service tokens. `src/dbus_service.rs` currently exports only `IsAuth`, `BeginLogin()`, and `LoginCompleted`.

This change needs to add an offline-first sync model without replacing the stack with a second architecture. The existing pieces worth preserving are:
- `YandexDiskClient` as the Yandex HTTP/auth boundary.
- `MountManager` as the mount lifecycle owner.
- `YandexDiskFs` as the FUSE adapter.
- Existing fake-client tests in `src/fs.rs` as a seed for broader integration coverage.

The main gap is that the daemon has no durable local source of truth. To support offline work and background sync, we need one state model that both FUSE and the worker layer share.

## Goals / Non-Goals

**Goals:**
- Make the mounted filesystem local-first: reads come from local cache when available, writes update local state immediately, and remote work is deferred.
- Persist metadata, queue state, and conflict records across daemon restart and crash.
- Use a single SQLite-backed job model for uploads, deletes, renames, remote refresh, lazy downloads, and reconcile steps.
- Detect remote-version conflicts safely and preserve both versions with numeric-suffix conflict copies.
- Expose bounded aggregate and per-path sync state over D-Bus without breaking existing auth clients.
- Fit the implementation into the current Rust daemon instead of introducing a parallel service or transport stack.

**Non-Goals:**
- Text merge, CRDTs, or any automatic conflict resolution beyond preserving both copies.
- Full remote tree export over D-Bus.
- Streaming file contents over D-Bus.
- A large GTK or GNOME Extension UI project; this repo currently has no such frontend code.
- Perfect provider-native delta sync if the Yandex API cannot guarantee it; we will implement a safe baseline and document compromises.

## Decisions

### 1. Introduce one daemon state root with SQLite metadata and local cached content
The daemon will gain a persistent state root, preferably under XDG state/data directories, with:
- `metadata.db` for SQLite metadata, queue, leases, conflicts, and refresh bookkeeping.
- a cache directory for hydrated file content and placeholder materialization.
- a small migrations/bootstrap step during daemon startup before mounting.

The SQLite schema will centralize integer-backed enums and the local/remote reconciliation state. Initial tables:
- `files`
  - `file_id TEXT PRIMARY KEY`
  - `path TEXT NOT NULL UNIQUE`
  - `kind INTEGER NOT NULL CHECK (...)`
  - `sync_state INTEGER NOT NULL CHECK (...)`
  - `content_state INTEGER NOT NULL CHECK (...)`
  - `remote_version TEXT`
  - `local_version INTEGER NOT NULL DEFAULT 0`
  - `mtime INTEGER`
  - `size INTEGER NOT NULL DEFAULT 0`
  - `hash BLOB`
  - `cache_path TEXT`
  - `last_remote_check_at INTEGER`
  - `remote_deleted INTEGER NOT NULL DEFAULT 0 CHECK (remote_deleted IN (0,1))`
  - `last_error TEXT`
- `operations_queue`
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
- `conflicts`
  - `conflict_id TEXT PRIMARY KEY`
  - `file_id TEXT NOT NULL`
  - `original_path TEXT NOT NULL`
  - `conflict_path TEXT NOT NULL`
  - `base_remote_version TEXT`
  - `current_remote_version TEXT`
  - `created_at INTEGER NOT NULL`
  - `origin_device TEXT`

Indexes will cover `operations_queue(op_status, next_retry_at, lease_expires_at)` and path-based file lookups. Enum values will live in one Rust module with `#[repr(i32)]` plus `TryFrom<i32>` decoding so SQL does not spread magic numbers through the code.

**Why:** this gives FUSE and background workers one durable source of truth.

**Alternatives considered:**
- Keep cache only in memory and queue only on disk: rejected because restart recovery would remain incomplete.
- Put queue in a separate subsystem from file metadata: rejected because it duplicates state transitions and makes coalescing harder.

### 2. Refactor `YandexDiskFs` to operate on the local projection, not directly on Yandex Disk
`YandexDiskFs` will stop treating remote APIs as the immediate authority for user mutations. Instead it will talk to a storage/sync facade that:
- resolves inode/path metadata from SQLite plus cache state.
- lazily hydrates file content into the local cache when a read/open requires bytes that are not present.
- applies writes, truncate, create, rename, and delete to local files first.
- updates metadata and enqueues a high-level sync job inside the same transaction.

The critical sequence for local mutation will be:
1. update the cached file or local placeholder state
2. update file metadata (`size`, `mtime`, `local_version`, `sync_state`)
3. enqueue or coalesce the corresponding job
4. commit one SQLite transaction

FUSE callbacks remain synchronous, but they only touch local disk and SQLite. Network operations move behind the worker/scheduler path.

**Why:** the user-visible contract is that save operations complete locally even when offline.

**Alternatives considered:**
- Keep current FUSE write-back staging and only persist staged files after close: rejected because the remote API would still sit on the hot path for normal save durability.
- Add a second “offline mount” separate from the current FUSE layer: rejected because it duplicates nearly the whole filesystem stack.

### 3. Use one shared scheduler/worker model for both push and refresh work
The daemon will add a sync runtime started from `src/main.rs` after auth/bootstrap. It will own:
- periodic remote refresh scheduling
- network-restored refresh scheduling
- manual refresh requests from D-Bus
- worker leasing and retry loops for pending jobs

High-level `op_type` values will cover both directions, for example:
- `upload`
- `delete`
- `mkdir`
- `move`
- `rename`
- `refresh_tree`
- `refresh_dir`
- `download`
- `reconcile_remote_delete`

Jobs are leased with `worker_id` and `lease_expires_at`. Startup recovery will:
- reset expired leased jobs back to runnable state
- schedule bootstrap refresh work
- optionally reconstruct jobs for files whose metadata implies unsynced local state

Backoff applies only to retryable failures. Permanent errors and conflicts remain visible until resolved.

**Why:** remote discovery is part of the same sync system, not a separate stack.

**Alternatives considered:**
- A dedicated second queue just for remote discovery: rejected because it duplicates scheduling, retry, and D-Bus projection logic.

### 4. Coalesce low-level file activity into stable high-level jobs
The queue must represent synchronization intent, not raw FUSE events. The storage/sync facade will normalize events before enqueueing:
- repeated local writes for one file collapse into a single pending `upload` job keyed by `file_id`
- `delete` after queued writes replaces the pending upload intent with delete
- rename/move updates the pending record payload instead of stacking multiple path-only jobs when safe
- refresh jobs for one directory may be deduplicated by path and freshness window

Ordering is preserved where it changes semantics, such as rename followed by delete across different paths. The design goal is to keep one authoritative pending action per file whenever possible.

**Why:** FUSE can emit many writes and rename-related sequences; persisting each low-level callback would bloat the queue and slow restart recovery.

### 5. Detect conflicts with remembered remote version and preserve both copies locally
Each synced file remembers the last accepted `remote_version`. Before upload or delete reconciliation, the worker checks the current remote version/state against that base. If the remote object changed or disappeared while the local file has unsynced changes, the worker records a conflict instead of overwriting remote state.

Conflict policy:
- the remote canonical object keeps the original path/name
- the local unsynced version is renamed to the next available `basename (N).ext`
- a conflict row is inserted so D-Bus and future UI can explain the event semantically, not only by filename

Filename helper rules:
- `file.txt` -> `file (2).txt`
- `file (2).txt` -> `file (3).txt`
- preserve compound extensions such as `archive.tar.gz` by inserting the suffix before the full extension group chosen by the helper rules
- files without extensions also work

Remote-delete conflict is treated explicitly as the same class of data-preservation problem.

**Why:** silent overwrite or last-write-wins would violate the core safety requirement.

**Provider note:** if Yandex Disk exposes reliable revision/etag or conditional-write semantics, the worker will use them. If not, the implementation falls back to safest available client-side compare-before-write checks and documents the residual race window.

### 6. Add bounded D-Bus sync state alongside the existing auth API
`src/dbus_service.rs` will keep the existing auth surface and grow sync-facing properties/methods on the same service/interface unless the zbus layout makes a sibling interface cleaner.

New properties:
- `MountPoint: s`
- `SyncSummary: a{sv}`
- `SyncItems: aa{sv}`

New methods:
- `GetSyncStatus(path: s) -> a{sv}`
- `ListDirectoryStatuses(path: s) -> aa{sv}`
- optional `RequestRefresh(path: s)` or equivalent manual refresh method if the zbus surface benefits from an explicit trigger

`SyncSummary` is a bounded aggregate projection derived from SQLite state. `SyncItems` contains only active items, conflicts, errors, and a small bounded tail of recently completed work. Updates are emitted through standard `org.freedesktop.DBus.Properties.PropertiesChanged`; no custom sync signal is required.

Because this repo currently has no GTK app or GNOME extension code, the change will stop at backend-facing D-Bus hooks and documentation for future consumers.

### 7. Keep implementation incremental by extracting new modules around the current core
To avoid turning `src/fs.rs` into an even larger mixed-responsibility file, the refactor will likely introduce focused modules such as:
- `src/state.rs` or `src/store/` for SQLite schema, migrations, enums, and transactions
- `src/cache.rs` for local file cache paths and hydration/invalidation helpers
- `src/sync.rs` or `src/worker/` for queue scheduling, leasing, retry, and refresh loops
- `src/sync_status.rs` for D-Bus projections

`src/fs.rs` remains the FUSE adapter, `src/yadisk.rs` remains the provider client, and `src/main.rs` wires the runtime together.

**Why:** this is the smallest restructuring that localizes infrastructure details while preserving the current daemon entrypoints.

## Risks / Trade-offs

- **The current repo has no SQLite layer yet**: this is the biggest new subsystem. Mitigation: keep schema small, centralize SQL, and prefer one store facade over ad-hoc statements across modules.
- **Offline-first semantics require cache-path decisions and hydration rules**: mitigation is to separate metadata TTL from content hydration state and make lazy download explicit.
- **Provider revision support may be incomplete**: mitigation is to choose the safest compare-before-write/delete flow available and document gaps.
- **A monolithic `fs.rs` makes refactor risk higher**: mitigation is to peel state/storage logic into new modules first, then switch callbacks over incrementally.
- **D-Bus payloads can grow unbounded**: mitigation is to keep `SyncItems` explicitly bounded and move directory inspection to pull-based methods.

## Migration Plan

1. Add daemon state-root initialization, SQLite dependency, migrations, and enum mapping.
2. Introduce local metadata/cache primitives and adapt mount/bootstrap to open them before FUSE startup.
3. Move FUSE reads and writes from direct remote operations to the local-first store facade.
4. Add queue leasing, worker execution, retry, refresh scheduling, and startup recovery.
5. Add conflict handling, D-Bus projections, and manual refresh entrypoints.
6. Expand tests and update README/docs.

Rollback remains possible because the new behavior is behind the daemon codepath only; if needed, the project can temporarily disable worker startup and restore direct remote flows, but the preferred rollout is to land the change in incremental patches with compile/test verification at each step.

## Open Questions

- Which exact Yandex Disk field should be treated as canonical `remote_version` in this codebase: revision, etag, resource id + modified timestamp, or another stable version token?
- Should the daemon expose the state-root location as configuration, or is an XDG-derived default sufficient for the first implementation?
- Do we need a small completed-items retention window in SQLite for `SyncItems`, or can the bounded “recently done” projection be derived from file metadata timestamps alone?
