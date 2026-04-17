## Context

The current daemon is a minimal `fuser` example that exposes only `/hello.txt` from hardcoded in-memory data. The target behavior is a real read-only filesystem backed by Yandex Disk using an OAuth token already stored in `.env`. The implementation must fit Rust + `fuser` callback semantics, where operations such as `lookup`, `getattr`, `readdir`, `open`, and `read` are synchronous and need predictable error translation back into POSIX-style errno values.

This change touches multiple concerns at once: HTTP API access, mapping remote paths to FUSE inodes, translating remote metadata into `FileAttr`, and fetching remote file bytes. The available project documentation is intentionally brief, so the design should prefer a small, debuggable architecture over aggressive optimization.

## Goals / Non-Goals

**Goals:**
- Replace the demo filesystem with a read-only Yandex Disk mount rooted at `disk:/`.
- Support directory traversal for root and nested directories via `lookup`, `getattr`, and `readdir`.
- Support opening and reading remote files through FUSE.
- Load the OAuth token from environment and fail fast with a clear startup error when configuration is missing.
- Keep implementation understandable enough to iterate on later for caching, pagination, and write support.

**Non-Goals:**
- Any mutating filesystem operations such as create, write, truncate, rename, remove, chmod, or chown.
- Offline sync, background prefetch, or local mirroring of Yandex Disk.
- Full support for every FUSE operation; only the operations needed for a stable read-only mount are in scope.
- Advanced cache invalidation or aggressive performance tuning beyond simple short-lived caches.

## Decisions

### 1. Split the implementation into a small API client layer and a filesystem state layer
The code should move away from a monolithic `main.rs` demo shape into modules such as `yadisk_client`, `fs_state`, and `fuse_fs` (names may vary). The API client will own HTTP requests and response deserialization. The filesystem layer will own inode assignment, path lookup, metadata translation, and FUSE callbacks.

**Why:** this keeps Yandex-specific HTTP concerns separate from `fuser` logic and makes later extension easier.

**Alternatives considered:**
- Keep everything in `main.rs`: faster initially, but difficult to maintain once lookup/readdir/read each need remote state.
- Build a generic VFS abstraction first: unnecessary abstraction for a single backend at this stage.

### 2. Use a blocking HTTP client with serde-based response models
FUSE callbacks in this project are synchronous, so the simplest implementation is a blocking client (`reqwest::blocking` or equivalent) plus `serde` for JSON decoding. The client should add `Authorization: OAuth <token>` to Yandex API calls and follow the documented two-step flow for file reads: resolve a download URL, then fetch file bytes.

**Why:** this matches the callback model and minimizes async runtime complexity.

**Alternatives considered:**
- Async HTTP with an executor: more moving parts and thread/runtime concerns for little benefit in the first version.
- Shelling out to `curl`: poor error handling, brittle quoting, and difficult integration with byte-range reads.

### 3. Maintain an in-memory inode/path registry with lazy population
The mounted filesystem should keep a process-local map from inode to remote entry metadata and from `(parent inode, child name)` or remote path to inode. Root is fixed to `INO 1`; child inodes are allocated monotonically as remote entries are discovered. `lookup` and `readdir` populate the registry lazily from Yandex API responses.

**Why:** FUSE needs stable inode numbers during the mount lifetime, while Yandex Disk identifies objects by path. A local registry provides stable inode handling without requiring a full tree preload.

**Alternatives considered:**
- Derive inode directly from path hashing: simple, but collision handling and rename semantics become awkward.
- Preload the entire remote tree on mount: too expensive and unnecessary for large disks.

### 4. Cache directory metadata briefly and cache file download URLs opportunistically
Directory listings and file metadata should be cached in memory for a short TTL similar to the existing FUSE TTL, reducing repeated API calls during `ls`, `find`, and repeated attribute lookups. For file reads, the implementation should cache the resolved direct download URL and basic metadata; file content caching should be optional and conservative.

**Why:** repeated FUSE callbacks often occur back-to-back, and a tiny metadata cache gives a large usability win without complex invalidation.

**Alternatives considered:**
- No cache at all: simpler, but causes excessive remote round-trips for common shell operations.
- Full file-content cache: risky for large files and unnecessary for the first version.

### 5. Read file content through HTTP GET with byte-range support when available, with a safe fallback
For `read`, the filesystem should resolve a download URL and perform an HTTP GET for the requested byte range. If the direct download endpoint does not honor `Range`, the implementation may fall back to downloading the response body and slicing the requested window, ideally only for the active request and without permanent retention.

**Why:** FUSE read offsets are random-access oriented, and full-file buffering does not scale for multi-hundred-megabyte files.

**Alternatives considered:**
- Always download the entire file first: too memory- and latency-heavy.
- Reject large files: contradicts the goal of exposing the real remote disk.

### 6. Map remote and HTTP failures to stable POSIX-style errors
The client and filesystem should normalize failures into categories such as `ENOENT` (missing path), `EACCES`/`EPERM` (auth or forbidden access where appropriate), `EIO` (unexpected API/network failures), and `EISDIR` (opening a directory as a file). Startup configuration failures should abort mount with a readable message instead of surfacing as runtime FUSE errors.

**Why:** shell tools behave much better when errno values are coherent.

**Alternatives considered:**
- Return `EIO` for everything: easiest, but makes debugging and user experience worse.

## Risks / Trade-offs

- [Large files may be slow to read over HTTP] → Prefer byte-range requests and avoid persistent full-file caching.
- [Repeated directory traversals can generate many API calls] → Add short-lived metadata caches and populate child entries during `readdir`.
- [Yandex Disk API details are incomplete in local docs] → Encapsulate all API assumptions inside the client and log raw failure context for troubleshooting.
- [Stable inode mapping is only guaranteed for one mount session] → Accept this for now; FUSE clients generally tolerate inode changes across remounts.
- [Network/auth failures can surface during normal shell usage] → Translate errors consistently and document that the mount depends on live API access.

## Migration Plan

1. Add environment/config loading and Yandex Disk API client dependencies.
2. Refactor the demo filesystem into modular stateful components.
3. Implement directory metadata loading, inode registration, and read-only directory traversal.
4. Implement file open/read via resolved download URLs.
5. Validate with local mount tests against real Yandex Disk content from the provided token.
6. Keep rollback simple: revert to the previous single-file demo implementation if remote integration proves unstable.

## Open Questions

- Does the resolved Yandex direct-download URL reliably support HTTP `Range` requests for all file types?
- Should the first version expose all timestamps returned by Yandex Disk directly, or normalize some missing values to mount-time `now()`?
- How much logging should be enabled by default for API failures during normal filesystem operations?
