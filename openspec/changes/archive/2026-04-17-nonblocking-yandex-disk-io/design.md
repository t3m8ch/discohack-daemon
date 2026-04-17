## Context

The current read-only Yandex Disk mount uses `fuser::spawn_mount2` but leaves the FUSE session at its default single worker thread and keeps nearly all filesystem state behind one `Mutex<FsState>`. The filesystem methods call blocking Yandex Disk HTTP operations while holding that mutex, so a slow `read`, `lookup`, `getattr`, or `readdir` can serialize unrelated requests behind one long critical section.

The existing implementation already uses a blocking `reqwest` client and synchronous `fuser::Filesystem` callbacks. The goal is to make the mounted filesystem observably responsive under concurrent access without rewriting the stack around a new async runtime.

## Goals / Non-Goals

**Goals:**
- Allow independent FUSE requests to make progress while another request is waiting on Yandex Disk.
- Preserve the current blocking HTTP client, read-only semantics, inode mapping, metadata caching, and error translation.
- Keep the implementation understandable and debuggable in the current Rust + `fuser` architecture.
- Bound the change so it can be validated with concurrency-focused tests or manual checks.

**Non-Goals:**
- A full migration to Tokio or a fully async filesystem architecture.
- Content prefetching, offline mirroring, or a persistent local file cache.
- Eliminating all shared state; some synchronization will remain necessary for inode and cache bookkeeping.
- Guaranteeing fairness across every possible workload beyond ensuring unrelated requests are not blocked by one global lock plus single worker thread.

## Decisions

### 1. Keep blocking HTTP and use FUSE worker-thread concurrency
The daemon will continue using the current blocking `reqwest` client and synchronous filesystem trait, and it will enable multiple FUSE worker threads via `fuser::Config` on Linux.

**Why:** this matches the current architecture, minimizes churn, and directly addresses the main bottleneck. The problem is not that the code lacks async syntax; it is that one FUSE worker plus long-held locks effectively serializes all remote I/O.

**Alternatives considered:**
- Switch to Tokio plus async `reqwest`: possible, but it would require broader refactoring for limited additional benefit in the current design.
- Spawn an ad-hoc OS thread per request: simpler conceptually, but it risks unbounded thread growth under heavy read activity.

### 2. Never hold the global filesystem mutex during network I/O
Filesystem operations will be refactored into phases: read or reserve the minimum necessary state under lock, release the lock before any Yandex Disk call, then reacquire the lock only to merge results back into caches and inode maps.

**Why:** this is the core fix. With the current design, even multiple FUSE workers would still block behind one mutex if remote calls happen inside the critical section.

**Alternatives considered:**
- Keep the current lock scope and only increase `n_threads`: improves little because the mutex remains the choke point.
- Replace the single mutex with many finer-grained locks immediately: possible, but unnecessarily invasive for the first concurrency fix.

### 3. Preserve one authoritative shared state object for inode and cache bookkeeping
The filesystem will keep a single shared state for inode assignment, path-to-inode mapping, cached metadata, and cached directory children, but it will introduce helper flows that separate snapshotting from remote fetch/merge operations.

**Why:** stable inode assignment and cache coherence still benefit from one authoritative registry. The responsiveness issue comes from lock duration, not from the existence of shared state itself.

**Alternatives considered:**
- Fully sharded state by inode or path prefix: more complex and hard to justify before measuring real contention after lock shortening.
- Stateless path hashing for inodes: would weaken current stability guarantees and complicate cache updates.

### 4. Serialize only per-entry or per-directory refresh publication, not the remote fetch itself
When metadata or directory cache entries are stale, the implementation may allow more than one request to race on the same remote fetch, but only the final cache update path will modify shared state under lock.

**Why:** this keeps the refactor small and safe. Avoiding duplicate remote fetches is desirable, but preventing them with wait groups or promise registries is a secondary optimization.

**Alternatives considered:**
- Add an in-flight request registry and request coalescing immediately: useful later, but not required to satisfy the observable responsiveness requirement.
- Keep stale data indefinitely while a background refresher updates it: adds stale-read semantics that are not needed for this change.

### 5. Validate concurrency as observable filesystem behavior
The change will be considered successful only if validation demonstrates that one slow remote read or metadata request no longer prevents an unrelated lookup, getattr, or readdir from completing.

**Why:** the user-visible requirement is responsiveness under concurrent FUSE activity, so validation must measure that externally rather than relying only on code inspection.

**Alternatives considered:**
- Only unit-test helper methods: useful but insufficient because the regression appears at the mounted filesystem level.

## Risks / Trade-offs

- [Multiple workers can trigger duplicate remote fetches for the same path] → Accept initially; preserve correctness first, optimize coalescing later if needed.
- [Shorter lock scopes increase state-transition complexity] → Keep snapshot/merge helpers small and explicit, and document which fields may be read or updated in each phase.
- [More FUSE concurrency can expose latent races in inode/cache updates] → Keep all registry mutation under one mutex and add focused tests around repeated lookup/readdir/read sequences.
- [Blocking HTTP still ties up a worker thread per remote request] → Use multiple FUSE workers so unrelated requests still progress; revisit a worker pool or async transport only if measured workloads demand it.
- [Linux-specific `clone_fd`/multi-thread behavior may differ on other platforms] → Scope the concurrency tuning to Linux where the daemon currently runs and where `fuser` documents multi-thread support.

## Migration Plan

1. Update mount/session configuration to use multiple FUSE worker threads on Linux.
2. Refactor filesystem methods so state is locked only for snapshotting and merge/update steps, never across Yandex Disk HTTP calls.
3. Adjust metadata refresh, directory loading, download URL resolution, and read flows to follow the new snapshot-fetch-merge pattern.
4. Run concurrency-focused validation against a real or controlled Yandex Disk backend, including a deliberately slow request alongside unrelated filesystem operations.
5. If regressions appear, rollback is straightforward: restore the previous lock scope and single-thread configuration while preserving existing read-only semantics.

## Open Questions

- Do we want an explicit regression test harness with injected artificial delays in the Yandex client, or is manual concurrency validation sufficient for the first pass?
- After the lock-scope refactor, is duplicate remote fetching rare enough to ignore, or do we need in-flight request coalescing in a follow-up?
- Should the daemon log slow remote operations or lock-wait timing to help diagnose future responsiveness issues?
