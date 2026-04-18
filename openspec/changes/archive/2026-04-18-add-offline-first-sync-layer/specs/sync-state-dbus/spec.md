## ADDED Requirements

### Requirement: Publish background sync summary over D-Bus
The daemon SHALL expose a `SyncSummary` property of type `a{sv}` on the existing `ru.literallycats.daemon` D-Bus interface so clients can observe aggregate sync state without inspecting queue internals.

#### Scenario: Summary reflects active work and attention state
- **WHEN** the daemon has queued, uploading, downloading, conflicting, or errored items
- **THEN** `SyncSummary` reports `active_count`, `uploading_count`, `downloading_count`, `queued_count`, `conflict_count`, `error_count`, `last_update_unix`, `is_syncing`, and `attention_required` with the documented semantics

### Requirement: Publish a bounded list of relevant sync items over D-Bus
The daemon SHALL expose a `SyncItems` property of type `aa{sv}` containing active items, conflicts, errors, and only a bounded set of recent completions rather than the full historical queue.

#### Scenario: Clients receive only relevant sync items
- **WHEN** many past sync operations have already completed successfully
- **THEN** `SyncItems` omits unbounded historical entries and keeps the property payload limited to active work, attention items, and a small recent tail

#### Scenario: Conflict item is visible to clients
- **WHEN** a file enters conflict state because the remote version changed before upload
- **THEN** `SyncItems` includes an item describing the conflicted path, state, direction, progress fields, and update timestamp so clients can present the issue

### Requirement: Notify clients using standard D-Bus property change signals
The daemon SHALL emit `org.freedesktop.DBus.Properties.PropertiesChanged` whenever `SyncSummary` or `SyncItems` changes.

#### Scenario: Worker state transition updates observers
- **WHEN** the sync worker leases, completes, retries, or fails a queued operation
- **THEN** subscribed D-Bus clients receive a `PropertiesChanged` notification for the affected sync properties without relying on a custom signal

### Requirement: Preserve the existing auth control plane on the same D-Bus object
The daemon SHALL keep the current auth-related service name, object path, interface, and login members while extending that object with sync-state properties.

#### Scenario: Existing auth client keeps working
- **WHEN** a client only reads `IsAuth` or calls `BeginLogin()`
- **THEN** it continues to work without needing to understand the new sync-state properties
