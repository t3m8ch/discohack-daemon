## Why

After the daemon exits, the same mountpoint can remain in a broken FUSE state and the next startup fails with `Transport endpoint is not connected`. We need predictable shutdown behavior so the process detaches cleanly, releases the mount, and allows immediate remount without manual recovery.

## What Changes

- Add graceful shutdown handling for normal process termination paths so the daemon explicitly unmounts the FUSE filesystem before exiting.
- Handle termination signals and user interrupts in a controlled way instead of relying on abrupt process exit.
- Ensure the mount lifecycle is tracked so shutdown waits for the FUSE session to stop accepting requests and releases kernel-side state.
- Replace ad-hoc stderr output with structured logging based on `tracing` for startup, mount lifecycle, signal handling, graceful stop, and shutdown failures.
- Validate that restarting the daemon on the same mountpoint works after a normal shutdown.

## Capabilities

### New Capabilities
- `graceful-fuse-shutdown`: Cleanly stop the FUSE mount on daemon exit and emit structured lifecycle logs so the same mountpoint can be reused without manual unmount recovery.

### Modified Capabilities

## Impact

- Affected code: `src/main.rs` and likely FUSE mount/session lifecycle code around process startup and exit.
- Runtime behavior: daemon now responds to shutdown signals by unmounting before process termination and emits structured lifecycle logs.
- Dependencies: may require signal handling or shutdown-coordination utilities plus `tracing` / `tracing-subscriber` for logging.
- Operations: restart/redeploy flows become safer because mountpoints are less likely to be left in a stale disconnected state and failures are easier to diagnose from logs.
