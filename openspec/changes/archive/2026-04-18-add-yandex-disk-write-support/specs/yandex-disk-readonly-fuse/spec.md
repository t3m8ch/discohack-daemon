## ADDED Requirements

### Requirement: Expose remote metadata as writable file attributes
The filesystem SHALL translate Yandex Disk file and directory metadata into FUSE attributes for `lookup` and `getattr`, including entry type and file size where available, and SHALL expose regular files and directories with writable permissions suitable for a user-owned mount.

#### Scenario: Get attributes for a remote file
- **WHEN** a client requests attributes for a file returned by Yandex Disk
- **THEN** the filesystem reports a regular file with the remote file size and writable file permissions

#### Scenario: Get attributes for a remote directory
- **WHEN** a client requests attributes for a directory returned by Yandex Disk
- **THEN** the filesystem reports a directory with writable traversal permissions

### Requirement: Persist file writes to Yandex Disk
The filesystem SHALL allow clients to create or open regular files with write access, apply writes and truncation at arbitrary offsets, and persist the resulting file contents to Yandex Disk when the handle is flushed, synced, or released.

#### Scenario: Create and write a new file
- **WHEN** a client creates a new file, writes bytes to it, and closes the handle successfully
- **THEN** the filesystem uploads the written contents to the corresponding Yandex Disk path and the file becomes visible in subsequent lookups and directory reads

#### Scenario: Overwrite an existing file
- **WHEN** a client opens an existing file for write access, replaces its contents, and commits the save
- **THEN** a later read of that path returns the updated bytes from Yandex Disk

#### Scenario: Truncate an existing file
- **WHEN** a client truncates a file to a smaller size and commits the change
- **THEN** the remote file size and readable contents reflect the truncation

### Requirement: Apply directory and path mutations to Yandex Disk
The filesystem SHALL map directory creation, file deletion, directory deletion, and rename operations to the corresponding Yandex Disk resource APIs and SHALL reflect successful mutations through the mounted hierarchy.

#### Scenario: Create a directory
- **WHEN** a client invokes `mkdir` for a path that does not yet exist
- **THEN** the filesystem creates the directory on Yandex Disk and exposes it in subsequent lookups and directory reads

#### Scenario: Delete a file
- **WHEN** a client unlinks a regular file that exists on Yandex Disk
- **THEN** the filesystem removes that remote file and the path is no longer returned by lookup or readdir

#### Scenario: Rename a file or directory
- **WHEN** a client renames an existing file or directory within the mounted hierarchy
- **THEN** the filesystem applies the rename remotely and resolves future lookups at the new path instead of the old path

### Requirement: Report failed mutations without silently changing remote state
The filesystem MUST return an error when Yandex Disk rejects a create, upload, delete, or rename operation, and it MUST NOT report success for a mutation that was not committed remotely.

#### Scenario: Upload fails during save
- **WHEN** a client writes data to a file but the final remote upload is rejected by Yandex Disk
- **THEN** the filesystem returns a write-related error for the commit operation and the previous remote contents remain authoritative

#### Scenario: Delete is rejected by the remote API
- **WHEN** a client attempts to remove a path and Yandex Disk rejects the deletion
- **THEN** the filesystem returns an error and the path remains available for subsequent lookup until a successful mutation occurs

## MODIFIED Requirements

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

## REMOVED Requirements

### Requirement: Expose remote metadata as read-only file attributes
**Reason**: Writable mount support replaces the previous read-only attribute contract.
**Migration**: Update callers and tests to expect writable permissions for mounted files and directories instead of `0444`/`0555`.

### Requirement: Enforce read-only filesystem behavior
**Reason**: The filesystem is being extended to support authenticated create, write, delete, and rename operations.
**Migration**: Update callers and tests to use operation-specific success and failure expectations rather than assuming every mutation returns `EROFS`.
