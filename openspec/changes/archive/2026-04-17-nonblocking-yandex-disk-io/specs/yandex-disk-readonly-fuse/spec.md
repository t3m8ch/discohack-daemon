## ADDED Requirements

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
