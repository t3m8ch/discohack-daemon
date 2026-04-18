# yandex-disk-readonly-fuse Specification

## Purpose
TBD - created by archiving change add-yandex-disk-readonly-fuse. Update Purpose after archive.
## Requirements
### Requirement: Mount Yandex Disk directory hierarchy
The filesystem SHALL expose the contents of `disk:/` as the FUSE mount root and SHALL allow clients to traverse nested directories discovered through the Yandex Disk resources API.

#### Scenario: List root directory
- **WHEN** a client reads the mount root directory
- **THEN** the filesystem returns `.` and `..` plus the files and directories reported for `disk:/` by Yandex Disk

#### Scenario: Traverse nested directory
- **WHEN** a client performs lookup for a child directory and then reads that directory
- **THEN** the filesystem resolves the corresponding Yandex Disk path and returns the child entries for that remote directory

### Requirement: Read file content from mounted local state with lazy remote hydration
The filesystem SHALL allow clients to open mounted regular files and SHALL return bytes from the local cache when available, hydrating placeholder content from Yandex Disk on demand when the file has metadata but no local bytes yet.

#### Scenario: Read already cached file content
- **WHEN** a client opens and reads a file whose local cached bytes are already present
- **THEN** the filesystem returns those local bytes without downloading the file again first

#### Scenario: Read placeholder file while online
- **WHEN** a client opens a file whose metadata is known locally but whose bytes have not yet been hydrated and network access is available
- **THEN** the filesystem fetches the content, stores it in the local cache, and returns the requested bytes

### Requirement: Serve unrelated requests while remote I/O is in flight
The filesystem SHALL continue serving independent FUSE requests while another request is waiting on Yandex Disk metadata, file-content download, or mutation/upload I/O, provided the later requests do not require the exact same reply payload to be produced first.

#### Scenario: Slow file upload does not block directory traversal
- **WHEN** one client issues a file flush or release that is delayed by Yandex Disk upload latency
- **THEN** another client can still complete `lookup` or `readdir` for a different path without waiting for the slow upload to finish

#### Scenario: Slow metadata refresh does not block unrelated attributes
- **WHEN** one path requires a stale metadata refresh from Yandex Disk before replying
- **THEN** another client can still complete `getattr` for a different already-known path while that refresh is in flight

#### Scenario: Concurrent requests preserve inode and mutation semantics
- **WHEN** the filesystem processes multiple overlapping requests while remote I/O is in flight
- **THEN** it still returns consistent inode mapping, read and write access rules, and error categories for the affected paths

### Requirement: Use service-managed OAuth credentials for Yandex Disk access
The daemon SHALL obtain Yandex Disk credentials from the service-managed auth state and SHALL not require `YANDEX_DISK_TOKEN`, `TOKEN`, or `YANDEX_TOKEN` in `.env` for normal operation.

#### Scenario: Stored credentials are reused after restart
- **WHEN** the daemon restarts after a previous successful login
- **THEN** it uses the persisted service-managed credentials for Yandex Disk API access without requiring the token to be re-entered in `.env`

#### Scenario: Refreshed credentials are used by filesystem operations
- **WHEN** the auth subsystem refreshes an expired access token while the filesystem is active
- **THEN** subsequent metadata requests, file reads, and remote mutations use the refreshed credentials without changing mounted filesystem behavior

### Requirement: Start the filesystem mount automatically after successful login
The daemon SHALL automatically start the writable Yandex Disk mount after a successful D-Bus login once managed credentials are available.

#### Scenario: First successful login starts the mount
- **WHEN** the daemon completes the Yandex OAuth flow successfully for an unauthenticated session
- **THEN** it starts the writable filesystem mount without requiring a separate mount command

#### Scenario: Automatic mount uses managed credentials
- **WHEN** the automatic mount starts after login
- **THEN** it authenticates Yandex Disk access with the newly managed service credentials rather than any token from `.env`

### Requirement: Unauthenticated filesystem startup or access fails clearly
When the daemon has no valid service-managed Yandex credentials, it SHALL report a clear unauthenticated state instead of silently falling back to environment-token configuration.

#### Scenario: Service starts before login
- **WHEN** the daemon starts and no valid service-managed credentials are available yet
- **THEN** it remains available for D-Bus login and reports that authentication is required before normal Yandex Disk access can proceed

#### Scenario: Mount or API access is attempted without valid credentials
- **WHEN** the daemon needs Yandex Disk access but no valid managed credentials are available
- **THEN** it returns a clear authentication error rather than attempting to read a token from `.env`

### Requirement: Expose remote metadata as writable file attributes
The filesystem SHALL translate Yandex Disk file and directory metadata into FUSE attributes for `lookup` and `getattr`, including entry type and file size where available, and SHALL expose regular files and directories with writable permissions suitable for a user-owned mount.

#### Scenario: Get attributes for a remote file
- **WHEN** a client requests attributes for a file returned by Yandex Disk
- **THEN** the filesystem reports a regular file with the remote file size and writable file permissions

#### Scenario: Get attributes for a remote directory
- **WHEN** a client requests attributes for a directory returned by Yandex Disk
- **THEN** the filesystem reports a directory with writable traversal permissions

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
