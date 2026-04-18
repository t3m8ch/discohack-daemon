## ADDED Requirements

### Requirement: Expose aggregate sync state over D-Bus properties
The daemon SHALL expose `MountPoint`, `SyncSummary`, and `SyncItems` over D-Bus so clients can observe mount mapping and bounded global sync state without reading raw queue internals.

#### Scenario: Read mount point and sync summary
- **WHEN** a client reads the daemon D-Bus properties after the service is running
- **THEN** it receives the local mount point path plus a `SyncSummary` dictionary containing active, queued, uploading, downloading, conflict, error, timestamp, and attention flags

#### Scenario: Read bounded sync items list
- **WHEN** a client reads `SyncItems`
- **THEN** it receives a bounded array containing active items, conflicts, errors, and any configured limited set of recent completions instead of an unbounded history

### Requirement: Notify clients about global sync state changes through standard property updates
The daemon SHALL emit `org.freedesktop.DBus.Properties.PropertiesChanged` whenever `SyncSummary` or `SyncItems` changes in a way that clients need to observe.

#### Scenario: Queue state changes emit property update
- **WHEN** a file enters or leaves queued, uploading, downloading, conflict, or error state
- **THEN** the daemon emits a standard `PropertiesChanged` update for the affected sync properties

### Requirement: Expose per-path sync status queries over D-Bus methods
The daemon SHALL provide `GetSyncStatus(path: s) -> a{sv}` and `ListDirectoryStatuses(path: s) -> aa{sv}` so frontends can inspect the sync status of a single path or the direct children of a directory.

#### Scenario: Query single file status
- **WHEN** a client calls `GetSyncStatus` for a known file path
- **THEN** the daemon returns a dictionary containing the file path, name, kind, sync state, direction, placeholder flag, conflict flag, progress fields, and update timestamp

#### Scenario: Query direct children of directory
- **WHEN** a client calls `ListDirectoryStatuses` for a known directory path
- **THEN** the daemon returns one status dictionary per direct child and does not recursively enumerate the whole subtree

#### Scenario: Query unknown path explicitly
- **WHEN** a client calls `GetSyncStatus` for a path the daemon does not know
- **THEN** the daemon returns an explicit unknown-path result rather than a silent empty success

### Requirement: Allow clients to trigger manual refresh scheduling
The daemon SHALL provide a D-Bus entrypoint for manual refresh request so frontends can ask the sync layer to refresh metadata without performing provider-specific network logic themselves.

#### Scenario: Manual refresh request schedules sync work
- **WHEN** a client invokes the manual refresh entrypoint for the mount root or a known path
- **THEN** the daemon schedules the corresponding refresh work through the shared sync queue or scheduler and the resulting global sync state becomes observable through the existing properties
