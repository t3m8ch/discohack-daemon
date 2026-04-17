## 1. Enable concurrent FUSE request processing

- [x] 1.1 Update mount/session configuration in `src/main.rs` to run the FUSE event loop with multiple worker threads on Linux
- [x] 1.2 Set any supporting FUSE options needed for efficient multi-threaded request handling and keep current mount behavior unchanged otherwise
- [x] 1.3 Add or update logging/config comments so the chosen concurrency settings are visible and understandable during startup

## 2. Refactor filesystem state access around remote I/O

- [x] 2.1 Identify every path in `src/fs.rs` where Yandex Disk HTTP calls happen while holding the global filesystem mutex
- [x] 2.2 Refactor metadata refresh and directory loading flows to use a lock-snapshot / remote-fetch / lock-merge pattern without changing inode or cache semantics
- [x] 2.3 Refactor download URL resolution and file read flows so no network request is performed while the global filesystem mutex is held
- [x] 2.4 Preserve existing read-only behavior, cache updates, and errno mapping under concurrent request execution

## 3. Validate responsiveness and concurrency behavior

- [x] 3.1 Add focused tests or a controlled validation path for slow remote operations and concurrent unrelated FUSE requests
- [x] 3.2 Verify that a slow read no longer blocks unrelated `lookup`, `getattr`, or `readdir` operations for other paths
- [x] 3.3 Verify that concurrent execution still preserves stable inode mapping, read-only enforcement, and existing error handling
