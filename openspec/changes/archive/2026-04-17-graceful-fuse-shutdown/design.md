## Context

The daemon currently uses `fuser::mount2`, which blocks the main thread until the filesystem is unmounted externally or the process exits. That makes shutdown behavior mostly implicit: if the process is interrupted, the kernel-side FUSE mount can be left in a disconnected state, and the next attempt to mount the same path fails with `Transport endpoint is not connected` until the user cleans it up manually.

The daemon also reports lifecycle events through ad-hoc stderr output. That is enough for startup failures, but not ideal once mount lifecycle, signal handling, and graceful teardown become explicit states that operators need to observe.

This change is small in surface area but cross-cutting in lifecycle behavior. It touches process startup, signal handling, FUSE session ownership, exit codes, and operational recovery. The current code keeps mount orchestration entirely inside `src/main.rs`, so the design should preserve that simplicity while giving the process an explicit shutdown path and consistent structured logging.

## Goals / Non-Goals

**Goals:**
- Make normal daemon termination explicitly unmount the FUSE filesystem before the process exits.
- Handle interactive interrupts and service-manager termination signals in a controlled way.
- Keep the shutdown path observable through clear structured logs and stable exit behavior.
- Introduce `tracing`-based logging for startup, mount lifecycle, signal receipt, graceful stop, and unmount failures.
- Allow a mountpoint that was shut down normally to be reused immediately on the next start.
- Minimize changes to the filesystem implementation itself; most of the work should stay in mount/session orchestration.

**Non-Goals:**
- Recover mounts after `SIGKILL`, kernel crashes, or machine power loss.
- Add write support or change read-only filesystem semantics.
- Introduce a long-running supervisor, daemonization framework, or large async runtime.
- Build a full logging configuration system with multiple sinks, remote exporters, or dynamic runtime reconfiguration.
- Automatically force-unmount unrelated stale mountpoints at startup unless they belong to the current session flow.

## Decisions

### 1. Replace blocking `mount2` with `spawn_mount2` and explicit session ownership
The daemon will switch from `fuser::mount2` to `fuser::spawn_mount2`, keep the returned `BackgroundSession` in `main`, and call `umount_and_join()` during shutdown.

**Why:** `mount2` gives no opportunity to coordinate shutdown from application code because it blocks until unmount completes. `spawn_mount2` makes the session a first-class value the daemon can own, shut down, and wait on deterministically.

**Alternatives considered:**
- Keep `mount2` and rely on process exit: this is the behavior that is currently leaving stale disconnected mountpoints.
- Use lower-level `Session` APIs directly: more control, but unnecessary complexity for the current need.

### 2. Add signal-driven shutdown coordination on the main thread
The process will register handlers for at least `SIGINT` and `SIGTERM`. Signal handlers will only notify the main thread through a safe primitive such as a channel or atomic flag; the main thread will perform the actual unmount and thread join.

**Why:** unmounting and joining threads are not signal-safe operations. The handler must stay minimal, while the main thread owns the shutdown sequence.

**Alternatives considered:**
- Perform cleanup directly inside the signal handler: unsafe and error-prone.
- Handle only Ctrl-C: insufficient for systemd/service-manager shutdowns.

### 3. Treat shutdown as a single idempotent state transition
Shutdown logic will be centralized in one path that can be triggered by a signal, by mount thread failure, or by future explicit stop requests. Repeated shutdown triggers should not attempt to unmount multiple times.

**Why:** signal storms or concurrent failure paths are common around process termination. An idempotent shutdown path prevents double-unmount races and confusing logs.

**Alternatives considered:**
- Scatter cleanup across multiple error branches: simpler initially, but fragile once more exit paths are added.

### 4. Report unmount failures clearly and preserve actionable exit status
If `umount_and_join()` fails, the daemon will print a clear error describing whether unmount or background thread completion failed, and it will exit non-zero. Successful graceful shutdown will exit cleanly.

**Why:** the user’s problem is operational. Good visibility is required so they can tell whether the mount was actually cleaned up.

**Alternatives considered:**
- Ignore cleanup errors: hides the exact failure mode and makes remount problems harder to diagnose.
- Always exit zero once a signal is received: misleading when cleanup failed.

### 5. Standardize daemon lifecycle logs on `tracing`
The daemon will initialize a `tracing` subscriber at startup and replace direct `eprintln!` lifecycle reporting with structured `tracing` events. At minimum, startup validation failures, mount start, signal receipt, shutdown start, successful unmount, and cleanup failures should be logged with appropriate levels.

**Why:** once graceful shutdown is explicit, operators need consistent lifecycle visibility. `tracing` gives a standard Rust logging model and makes future extensions such as env-based filtering straightforward.

**Alternatives considered:**
- Keep `eprintln!`: adequate for single-shot errors, but weak for multi-step lifecycle visibility.
- Introduce a heavier observability stack now: overkill for the current daemon.

### 6. Validate behavior with an actual remount flow on the same mountpoint
Manual validation should cover: mount, access the filesystem, stop the daemon with Ctrl-C or SIGTERM, then immediately start it again on the same path.

**Why:** the reported bug is specifically about remountability after shutdown. A real restart cycle is the acceptance test for this change.

**Alternatives considered:**
- Unit-test only internal shutdown state: useful but insufficient to prove kernel-side mount cleanup.

## Risks / Trade-offs

- [Background session shutdown may still fail if the kernel or userspace FUSE stack is already broken] → Surface the exact error and keep the failure non-silent.
- [Adding signal handling introduces another dependency or OS-specific code path] → Prefer a small well-supported crate or minimal Linux-focused implementation.
- [The daemon may receive multiple signals during teardown] → Make shutdown idempotent and avoid repeated unmount attempts.
- [Moving from blocking mount to background session changes control flow in `main`] → Keep ownership localized to `main.rs` and avoid spreading lifecycle state into filesystem modules.
- [Introducing `tracing` can add noisy output if levels are chosen poorly] → Keep lifecycle events concise and use conventional levels (`info`, `warn`, `error`, optionally `debug`).

## Migration Plan

1. Add `tracing` initialization and replace ad-hoc lifecycle stderr output with structured log events.
2. Replace the blocking mount call in `src/main.rs` with background session startup.
3. Add signal registration and a main-thread wait loop for shutdown events.
4. Implement a single cleanup path that calls `umount_and_join()` and maps failures to logs plus exit codes.
5. Manually verify that a clean stop leaves the mountpoint reusable immediately.
6. Roll back by restoring `mount2` if the new coordination logic proves unstable, though that would reintroduce the current remount issue.

## Open Questions

- Should the daemon also listen for `SIGHUP`, or are `SIGINT` and `SIGTERM` sufficient for the intended deployment model?
- Do we want a best-effort fallback to `fusermount -u`/lazy unmount if `umount_and_join()` fails, or should that remain an operator action?
- Should log filtering be controlled only through `RUST_LOG`, or do we want a daemon-specific environment variable as well?
