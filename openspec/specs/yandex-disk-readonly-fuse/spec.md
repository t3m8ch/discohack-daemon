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

### Requirement: Expose remote metadata as read-only file attributes
The filesystem SHALL translate Yandex Disk file and directory metadata into FUSE attributes for `lookup` and `getattr`, including entry type and file size where available, and SHALL expose regular files as read-only.

#### Scenario: Get attributes for a remote file
- **WHEN** a client requests attributes for a file returned by Yandex Disk
- **THEN** the filesystem reports a regular file with the remote file size and read-only permissions

#### Scenario: Get attributes for a remote directory
- **WHEN** a client requests attributes for a directory returned by Yandex Disk
- **THEN** the filesystem reports a directory with readable traversal permissions

### Requirement: Read file content from Yandex Disk
The filesystem SHALL allow clients to open remote regular files in read-only mode and SHALL return file bytes from Yandex Disk for the requested offset and size.

#### Scenario: Read beginning of file
- **WHEN** a client opens a remote file and reads bytes starting at offset `0`
- **THEN** the filesystem returns the first bytes of the corresponding Yandex Disk file content

#### Scenario: Read file slice with offset
- **WHEN** a client reads a remote file with a non-zero offset and bounded size
- **THEN** the filesystem returns the matching byte slice for that offset without corrupting file order

#### Scenario: Read past end of file
- **WHEN** a client requests bytes starting at or beyond the end of a remote file
- **THEN** the filesystem returns an empty payload

### Requirement: Enforce read-only filesystem behavior
The filesystem MUST reject mutating operations and MUST not advertise write capability for mounted Yandex Disk entries.

#### Scenario: Open file with write intent
- **WHEN** a client attempts to open a remote file with write access
- **THEN** the filesystem returns an error indicating the file is not writable

#### Scenario: Invoke unsupported mutation operation
- **WHEN** a client attempts a mutation such as create, rename, unlink, mkdir, rmdir, or write
- **THEN** the filesystem returns an operation-not-permitted style error and leaves Yandex Disk unchanged

### Requirement: Serve unrelated requests while remote I/O is in flight
The filesystem SHALL continue serving independent FUSE requests while another request is waiting on Yandex Disk metadata or file-content I/O, provided the later requests do not require the exact same reply payload to be produced first.

#### Scenario: Slow file read does not block directory traversal
- **WHEN** one client issues a file read that is delayed by Yandex Disk network or download latency
- **THEN** another client can still complete `lookup` or `readdir` for a different path without waiting for the slow read to finish

#### Scenario: Slow metadata refresh does not block unrelated attributes
- **WHEN** one path requires a stale metadata refresh from Yandex Disk before replying
- **THEN** another client can still complete `getattr` for a different already-known path while that refresh is in flight

#### Scenario: Concurrent requests preserve read-only semantics
- **WHEN** the filesystem processes multiple overlapping requests while remote I/O is in flight
- **THEN** it still returns the same read-only access rules, inode mapping behavior, and error categories defined for the mount

### Requirement: Use service-managed OAuth credentials for Yandex Disk access
The daemon SHALL obtain Yandex Disk credentials from the service-managed auth state and SHALL not require `YANDEX_DISK_TOKEN`, `TOKEN`, or `YANDEX_TOKEN` in `.env` for normal operation.

#### Scenario: Stored credentials are reused after restart
- **WHEN** the daemon restarts after a previous successful login
- **THEN** it uses the persisted service-managed credentials for Yandex Disk API access without requiring the token to be re-entered in `.env`

#### Scenario: Refreshed credentials are used by filesystem operations
- **WHEN** the auth subsystem refreshes an expired access token while the filesystem is active
- **THEN** subsequent metadata and file-read requests use the refreshed credentials without changing the read-only filesystem behavior

### Requirement: Start the filesystem mount automatically after successful login
The daemon SHALL automatically start the read-only Yandex Disk mount after a successful D-Bus login once managed credentials are available.

#### Scenario: First successful login starts the mount
- **WHEN** the daemon completes the Yandex OAuth flow successfully for an unauthenticated session
- **THEN** it starts the read-only filesystem mount without requiring a separate mount command

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

