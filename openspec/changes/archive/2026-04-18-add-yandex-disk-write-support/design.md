## Context

The current filesystem implementation is intentionally read-only: `src/fs.rs` exposes `0444`/`0555` permissions, rejects every mutating FUSE operation with `EROFS`, and `src/mount.rs` mounts with `MountOption::RO`. On the Yandex side, the daemon already knows how to authenticate and read metadata and file contents, and the project now includes example write-side API flows for directory creation, deletion, and two-step upload.

The main architectural constraint is that Yandex Disk exposes whole-resource upload semantics rather than a random-write block API. FUSE write requests, however, arrive as arbitrary offsets, sizes, truncations, and close/flush sequences. The design therefore needs a write-back layer that translates POSIX-style mutations into Yandex Disk resource operations without breaking the existing read, lookup, and concurrency behavior.

## Goals / Non-Goals

**Goals:**
- Support writable file and directory operations required for normal save workflows: create, open-for-write, write, truncate, flush/release upload, mkdir, unlink, rmdir, and rename.
- Preserve existing read and traversal behavior for unchanged paths.
- Keep service-managed OAuth as the only normal authentication path for both reads and writes.
- Maintain coherent inode, directory, and metadata state after successful remote mutations.
- Return clear filesystem errors when remote write operations fail.

**Non-Goals:**
- Implement block-level remote patching or multipart resumable uploads.
- Guarantee crash-safe recovery of uncommitted local temp data across daemon restarts.
- Add advanced Yandex Disk features such as copy, publish, trash restore, sharing, or conflict-resolution UIs.
- Optimize very large file writes beyond a pragmatic first write-back implementation.

## Decisions

### 1. Use local write-back staging for mutable file handles
Writable handles will map to local staging files rather than directly to Yandex Disk. For an existing file, opening with write intent will materialize the current remote bytes into a temp file once; for a newly created file, the temp file starts empty. `write` and `setattr(size)` update that temp file, while `flush`, `fsync`, or the final `release` uploads the full staged contents back to Yandex Disk if the handle is dirty.

This matches the Yandex Disk API shape, which supports full upload via an obtained `href` but not arbitrary remote offset writes.

**Alternatives considered:**
- Upload on every `write` call: rejected because FUSE writes are partial and frequent, causing incorrect overwrite semantics and extreme network overhead.
- Keep all staged bytes only in memory: rejected because editors may write large files and seek/truncate repeatedly.

### 2. Extend `YandexDiskClient` with explicit mutation primitives
`src/yadisk.rs` will grow dedicated methods for writable operations:
- create directory via `PUT /resources`
- delete file or directory via `DELETE /resources`
- resolve upload URL and upload file contents to that URL
- move/rename resources via the appropriate Yandex Disk move endpoint
- refresh metadata after successful mutation when the filesystem needs authoritative size or timestamps

Keeping these as client-level primitives isolates HTTP details, token refresh behavior, and Yandex-specific error mapping from the FUSE layer.

**Alternatives considered:**
- Construct HTTP requests directly inside `src/fs.rs`: rejected because it would mix filesystem semantics with transport details and duplicate auth/error logic.

### 3. Track writable handles separately from path metadata cache
`FsState` will be extended with open-handle state that records the target inode/path, staging file location, dirty status, and whether the handle represents a newly created resource. Path/inode metadata remains the shared directory cache, while write sessions are keyed by `FileHandle`.

On successful commit, the filesystem will either update the affected entry in place or invalidate/reload the parent directory cache. On delete or rename, stale path mappings and cached download URLs must be removed or rewritten so later lookups see the remote result.

**Alternatives considered:**
- Invalidate the entire filesystem cache after every mutation: simpler, but it would cause unnecessary remote calls and make save workflows feel much slower.

### 4. Make the mount writable at the FUSE layer
The mount manager will stop advertising the mount as read-only, and attribute permissions will change to writable defaults (`0644` for files, `0755` for directories, subject to the existing single-user mount model). `access`, `open`, `create`, `mkdir`, `unlink`, `rmdir`, `rename`, `write`, and `setattr(size)` will move from unconditional `EROFS` to operation-specific logic.

This is required so normal tools and editors attempt save flows instead of failing immediately.

**Alternatives considered:**
- Keep the mount marked read-only and selectively fake writes: rejected because the kernel and user-space tools would never exercise the mutation paths consistently.

### 5. Commit on both `fsync`/`flush` and final `release`, with duplicate-safe handling
Different clients persist changes at different points. Some rely on `fsync`, some on `flush`, and some only on `release`. The filesystem will treat any commit trigger as “upload if dirty,” then mark the handle clean so repeated callbacks do not re-upload unchanged data.

**Alternatives considered:**
- Commit only on `release`: rejected because applications expecting `fsync` durability would observe weaker behavior.

## Risks / Trade-offs

- **Large-file writes require full staging and full upload** → Mitigation: use on-disk temp files, stream upload from disk, and document the trade-off.
- **Remote mutation latency can make save operations feel slow** → Mitigation: keep lock scopes short, do HTTP work outside the main state mutex, and preserve the existing concurrency pattern for unrelated paths.
- **Rename semantics may depend on exact Yandex Disk API details** → Mitigation: verify move endpoint contract with curl during implementation and keep overwrite behavior explicit.
- **Failed upload after local writes can leave the handle dirty** → Mitigation: surface `EIO`/`EACCES`, keep staged data until handle close completes, and avoid mutating cached metadata before remote success.
- **Delete/rename can invalidate cached inodes and children mappings** → Mitigation: centralize cache invalidation/update helpers and add mutation-focused tests.

## Migration Plan

This change does not require a persistent data migration. Deployment consists of shipping the writable filesystem code, removing the read-only mount option, and updating documentation to describe save behavior and current limitations. Rollback is straightforward: restore the previous read-only mount configuration and reject mutations again.

## Open Questions

- Which exact Yandex Disk move endpoint parameters and response codes should be treated as the canonical rename contract in the client?
- Should deletes always use `permanently=true`, or should initial writable support prefer trash semantics if available?
- Do we need an explicit `fsyncdir`/directory sync implementation, or is cache invalidation after successful directory mutations sufficient for the first version?
