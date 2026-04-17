## 1. Logging foundation

- [x] 1.1 Add `tracing` and `tracing-subscriber` dependencies and initialize a default subscriber during daemon startup
- [x] 1.2 Replace ad-hoc lifecycle `eprintln!` output in `src/main.rs` with `tracing` events at appropriate levels
- [x] 1.3 Ensure startup validation and mount initialization failures are logged with enough context before exit

## 2. Mount session lifecycle

- [x] 2.1 Replace the blocking `fuser::mount2` startup path in `src/main.rs` with `fuser::spawn_mount2` and retain the returned background session handle
- [x] 2.2 Add a single shutdown routine that unmounts the active session with `umount_and_join()` and maps success or failure to clear process exit behavior
- [x] 2.3 Make shutdown idempotent so repeated stop triggers do not race or attempt duplicate unmounts

## 3. Signal-driven graceful shutdown

- [x] 3.1 Add signal handling for Ctrl-C and `SIGTERM` that notifies the main thread without doing non-signal-safe cleanup work inside the handler
- [x] 3.2 Block the main thread on shutdown notifications and invoke the centralized cleanup path when termination is requested
- [x] 3.3 Emit structured tracing events for signal receipt, graceful stop, successful unmount, and unmount failure paths

## 4. Restart validation

- [x] 4.1 Manually verify the remount flow by starting the daemon, stopping it gracefully, and starting it again on the same mountpoint
- [x] 4.2 Verify the expected tracing output appears for startup, shutdown, and failure paths
- [x] 4.3 Document the graceful shutdown behavior, tracing-based logging, and any operational recovery note if cleanup fails unexpectedly
