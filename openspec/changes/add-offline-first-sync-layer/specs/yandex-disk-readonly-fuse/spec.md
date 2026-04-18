## MODIFIED Requirements

### Requirement: Mount Yandex Disk directory hierarchy
The filesystem SHALL expose the contents of `disk:/` as the FUSE mount root through a local metadata projection backed by persistent state, and SHALL allow clients to traverse nested directories discovered from Yandex Disk refresh data plus local unsynchronized changes.

#### Scenario: List root directory from local metadata
- **WHEN** a client reads the mount root directory after the daemon has bootstrapped or refreshed metadata
- **THEN** the filesystem returns `.` and `..` plus the files and directories currently recorded in the local metadata projection for `disk:/`

#### Scenario: Traverse nested directory while offline
- **WHEN** a client performs lookup for a known child directory and then reads that directory while the network is unavailable
- **THEN** the filesystem resolves the directory from persistent local metadata without requiring direct network access from the FUSE callback

### Requirement: Read file content from Yandex Disk
The filesystem SHALL serve regular-file bytes from local cached content when available and SHALL support lazy hydration of placeholder files through the daemon sync layer when metadata exists but content is not yet cached locally.

#### Scenario: Read hydrated local file
- **WHEN** a client opens a file whose content is already present in the local cache
- **THEN** the filesystem returns the cached bytes without calling the Yandex Disk content API directly from the FUSE callback

#### Scenario: First read of placeholder file hydrates local cache
- **WHEN** a client reads a file whose metadata is known locally but whose content cache is absent
- **THEN** the daemon hydrates the file into the local cache through the sync layer and returns the requested bytes after hydration succeeds

#### Scenario: Read past end of cached file
- **WHEN** a client requests bytes starting at or beyond the end of the locally cached file
- **THEN** the filesystem returns an empty payload

### Requirement: Persist file writes to Yandex Disk
The filesystem SHALL allow clients to create or open regular files with write access, apply writes and truncation to the local cached file first, atomically update metadata and queued sync intent, and synchronize the resulting contents to Yandex Disk asynchronously.

#### Scenario: Create and write a new file while offline
- **WHEN** a client creates a new file, writes bytes to it, and closes the handle while the network is unavailable
- **THEN** the written contents are preserved in local cache, the file is visible through subsequent lookups and reads, and a persistent upload job is queued for later synchronization

#### Scenario: Overwrite an existing file without waiting for upload
- **WHEN** a client opens an existing file for write access, replaces its contents, and commits the save
- **THEN** a later read of that path returns the updated local bytes immediately and the upload is performed asynchronously by the sync worker

#### Scenario: Truncate an existing file locally first
- **WHEN** a client truncates a file to a smaller size and commits the change
- **THEN** the local cached file size and readable contents reflect the truncation immediately and the change remains queued until synchronized

### Requirement: Apply directory and path mutations to Yandex Disk
The filesystem SHALL apply directory creation, file deletion, directory deletion, and rename operations to the local metadata projection first, persist the corresponding high-level sync jobs, and reconcile those mutations with Yandex Disk asynchronously.

#### Scenario: Create a directory locally first
- **WHEN** a client invokes `mkdir` for a path that does not yet exist
- **THEN** the filesystem creates the directory in local metadata immediately and queues the corresponding remote mutation for background synchronization

#### Scenario: Delete a file locally first
- **WHEN** a client unlinks a regular file that exists in the mounted hierarchy
- **THEN** the filesystem removes the file from the local projection immediately and records a persistent delete job instead of waiting for a remote API round trip

#### Scenario: Rename a file or directory locally first
- **WHEN** a client renames an existing file or directory within the mounted hierarchy
- **THEN** the filesystem updates future lookups to the new path immediately and queues the corresponding remote rename or move reconciliation

### Requirement: Report failed mutations without silently changing remote state
The daemon MUST preserve locally committed mutations even when later synchronization fails, and it MUST surface retryable errors or conflicts without silently discarding local changes or blindly overwriting the remote state.

#### Scenario: Upload fails after local save
- **WHEN** a client saves changes locally and the background upload later fails with a retryable remote error
- **THEN** the local contents remain authoritative for mounted clients, the sync item is marked as errored or retryable, and the change stays available for later retry

#### Scenario: Remote overwrite conflict is detected
- **WHEN** the daemon prepares to upload a locally changed file and the current remote version no longer matches the remembered synchronized remote version
- **THEN** the daemon does not overwrite the remote file, preserves both versions, and records the file as conflicted

## ADDED Requirements

### Requirement: Persist sync metadata and queued operations across daemon restart
The daemon SHALL persist file metadata, queue state, leases, and conflict bookkeeping in SQLite so local state and pending synchronization survive process restart or crash.

#### Scenario: Pending job survives daemon restart
- **WHEN** the daemon restarts after recording a local file change that has not yet been synchronized
- **THEN** the file metadata and pending queue item remain available after startup and synchronization can resume automatically

#### Scenario: Expired lease is recovered after crash
- **WHEN** the daemon starts and finds a previously leased queue item whose lease expiration has passed
- **THEN** the item becomes runnable again instead of remaining stuck in a leased state forever

### Requirement: Coalesce low-level local mutations into high-level sync jobs
The daemon SHALL normalize repeated low-level file activity into high-level persistent jobs so the queue represents synchronization intent rather than raw FUSE event volume.

#### Scenario: Repeated writes collapse into one upload job
- **WHEN** a client issues multiple local write operations against the same file before synchronization runs
- **THEN** the queue retains one effective pending upload job for that file instead of accumulating redundant uploads

#### Scenario: Delete supersedes queued upload
- **WHEN** a file already has a pending upload intent and the client deletes that file before synchronization runs
- **THEN** the queue records the effective delete intent instead of keeping stale upload work for the removed file

### Requirement: Detect and reconcile remote metadata changes
The daemon SHALL discover remote metadata changes at startup, after network restoration, on a periodic schedule, and on manual refresh request, and SHALL reconcile those changes into the local metadata projection without requiring clients to perform direct network sync work.

#### Scenario: Startup refresh updates local metadata
- **WHEN** the daemon starts while network access is available
- **THEN** it schedules or performs a bootstrap refresh that updates local metadata from the current remote tree before ongoing steady-state synchronization continues

#### Scenario: Remote change invalidates stale local content cache
- **WHEN** remote metadata indicates that a file changed and the local file has no unsynchronized local edits
- **THEN** the daemon updates the file metadata and invalidates or refreshes the local content cache before future reads rely on stale bytes

#### Scenario: Manual refresh request schedules discovery work
- **WHEN** a client requests a refresh through the daemon API
- **THEN** the daemon schedules remote discovery work through the shared sync infrastructure instead of performing a separate ad hoc network path

### Requirement: Preserve both versions when local and remote changes conflict
The daemon SHALL preserve both versions of a file whenever remote changes conflict with unsynchronized local changes, and SHALL keep the remote canonical version at the original path while renaming the local unsynchronized copy with the next available numeric suffix.

#### Scenario: Local upload conflict creates suffixed copy
- **WHEN** a local file has unsynchronized changes and the remote version changed since the last synchronized remote version
- **THEN** the daemon keeps the remote file at the original path and renames the local unsynchronized copy to a path such as `file (2).ext`

#### Scenario: Remote deletion conflicts with local unsynchronized edits
- **WHEN** remote discovery reports that a file was deleted remotely while the local file still has unsynchronized edits
- **THEN** the daemon records a delete conflict, preserves the local unsynchronized copy under a suffixed conflict path, and does not silently discard the local data

### Requirement: Generate numeric-suffix conflict filenames deterministically
The daemon SHALL create conflict copy names by inserting a numeric suffix before the extension, incrementing any existing numeric suffix instead of nesting a new one, and trying the next value until a free path is found.

#### Scenario: Add suffix to plain filename
- **WHEN** a conflict copy is created for `file.txt`
- **THEN** the first generated conflict filename is `file (2).txt`

#### Scenario: Increment existing suffix
- **WHEN** a conflict copy is created for `file (2).txt`
- **THEN** the next generated conflict filename is `file (3).txt`

#### Scenario: Preserve files without normal extension
- **WHEN** a conflict copy is created for a filename without an extension or with a compound extension
- **THEN** the generated name preserves the base naming structure while inserting the numeric suffix in the configured conflict position
