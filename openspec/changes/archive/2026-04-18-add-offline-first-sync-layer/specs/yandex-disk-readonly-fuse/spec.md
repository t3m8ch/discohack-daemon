## ADDED Requirements

### Requirement: Serve the mounted filesystem from durable local state
The filesystem SHALL treat the daemon-managed local cache and metadata store as the client-visible source of truth and SHALL continue serving cached files and directories when Yandex Disk is temporarily unavailable.

#### Scenario: Read cached file while offline
- **WHEN** a file has already been hydrated into the local cache and the network becomes unavailable
- **THEN** a client read for that file returns the cached bytes without requiring a live Yandex Disk request

#### Scenario: Browse cached directory while offline
- **WHEN** directory metadata has already been persisted locally and the daemon restarts without connectivity
- **THEN** `lookup`, `getattr`, and `readdir` for those cached paths succeed from local metadata

### Requirement: Persist sync intent across daemon restart
The filesystem SHALL store unsynced mutations in persistent metadata and queue state so pending synchronization work survives crash and restart.

#### Scenario: Pending upload survives restart
- **WHEN** a client modifies a file locally and the daemon restarts before the background upload completes
- **THEN** the file remains available locally and the pending upload intent is restored from persistent state

### Requirement: Preserve both versions when remote state conflicts with local edits
The filesystem SHALL detect when the current remote version differs from the last synchronized remote version for a locally edited file and SHALL preserve both the remote version and the local version.

#### Scenario: Upload detects a changed remote version
- **WHEN** the sync worker prepares to upload a locally edited file and the current remote version does not match the file's stored base remote version
- **THEN** the daemon keeps the remote file at the original path, renames the local copy to a numeric-suffix conflict path, and records the conflict for later client visibility

### Requirement: Expose background sync failures without discarding local data
The filesystem SHALL preserve local mutations even when background synchronization fails and SHALL mark the affected items as needing attention instead of silently dropping them.

#### Scenario: Upload fails after a local write succeeded
- **WHEN** a client updates a file locally and the later background upload is rejected or times out
- **THEN** the local file contents remain available and the item transitions into a durable retryable or error sync state

## MODIFIED Requirements

### Requirement: Read file content from mounted local state with lazy remote hydration
The filesystem SHALL allow clients to open mounted regular files and SHALL return bytes from the local cache when available, hydrating placeholder content from Yandex Disk on demand when the file has metadata but no local bytes yet.

#### Scenario: Read already cached file content
- **WHEN** a client opens and reads a file whose local cached bytes are already present
- **THEN** the filesystem returns those local bytes without downloading the file again first

#### Scenario: Read placeholder file while online
- **WHEN** a client opens a file whose metadata is known locally but whose bytes have not yet been hydrated and network access is available
- **THEN** the filesystem fetches the content, stores it in the local cache, and returns the requested bytes

### Requirement: Apply local file writes before asynchronous cloud sync
The filesystem SHALL allow clients to create or open regular files with write access, apply writes and truncation against local state immediately, and synchronize the resulting file contents to Yandex Disk asynchronously through a persistent queue.

#### Scenario: Create and write a new file while offline
- **WHEN** a client creates a new file and writes bytes while Yandex Disk is unavailable
- **THEN** the filesystem commits the local file contents, records a pending upload intent, and reports success for the local mutation

#### Scenario: Overwrite an existing file without blocking on upload
- **WHEN** a client updates an existing file and closes or flushes the handle successfully
- **THEN** the local file contents become the mounted truth immediately and the remote upload proceeds later in the background

### Requirement: Apply directory and path mutations through durable queued sync
The filesystem SHALL apply supported directory creation, deletion, and rename operations to local state first and SHALL reconcile those mutations to Yandex Disk through the persistent sync queue.

#### Scenario: Rename while offline
- **WHEN** a client renames a file while offline
- **THEN** the mounted hierarchy reflects the new local path immediately and a queued move or rename intent is persisted for later synchronization

#### Scenario: Delete supersedes pending upload
- **WHEN** a file has a pending upload intent and the client deletes it locally before the worker syncs it
- **THEN** the queue collapses the stale upload intent into the final delete intent instead of attempting both remote operations

## REMOVED Requirements

### Requirement: Report failed mutations without silently changing remote state
**Reason**: In the offline-first model, local mutation success is decoupled from remote sync success. Remote failures are still surfaced durably, but they are no longer required to fail the original FUSE mutation once the local state change has committed.

**Migration**: Update callers and tests to treat local mutation success and later remote sync success as separate observable states.
