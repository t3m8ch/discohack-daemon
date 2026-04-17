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

### Requirement: Require valid API authentication at startup
The daemon SHALL require a Yandex Disk OAuth token from environment configuration before mounting and SHALL fail clearly if the token is missing or unusable.

#### Scenario: Missing token
- **WHEN** the daemon starts without the required OAuth token in the environment
- **THEN** it aborts before mounting and prints a clear configuration error

#### Scenario: Invalid token during initial API access
- **WHEN** the daemon starts with a token that Yandex Disk rejects during initial remote access
- **THEN** it fails the mount initialization with a clear authentication error

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

