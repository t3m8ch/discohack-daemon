## 1. Project setup and refactor

- [x] 1.1 Add the Rust dependencies needed for environment loading, HTTP requests, JSON parsing, date/time parsing, and error handling for Yandex Disk integration
- [x] 1.2 Refactor the single-file demo in `src/main.rs` into a stateful read-only filesystem structure with separate modules for API access and FUSE logic
- [x] 1.3 Load the OAuth token from environment or `.env` and fail fast with a clear startup error when configuration is missing

## 2. Yandex Disk metadata integration

- [x] 2.1 Implement a Yandex Disk API client for fetching resource metadata and directory listings for `disk:/` and nested paths
- [x] 2.2 Define deserialization models for the Yandex Disk responses needed for files, directories, embedded children, and download URL resolution
- [x] 2.3 Implement error translation from Yandex/API failures into filesystem-friendly categories such as not found, forbidden, and I/O error

## 3. FUSE directory and inode handling

- [x] 3.1 Implement an in-memory inode and path registry with stable inode assignment for the lifetime of the mount
- [x] 3.2 Implement `lookup` and `getattr` so remote files and directories are exposed with correct read-only attributes and sizes
- [x] 3.3 Implement `readdir` for root and nested directories, including `.` and `..`, using Yandex Disk directory listings
- [x] 3.4 Add short-lived metadata caching so repeated directory and attribute requests do not always hit the remote API

## 4. Read-only file access

- [x] 4.1 Implement download URL resolution for remote files and cache reusable download metadata where practical
- [x] 4.2 Implement `open` to allow read-only access to regular files and reject directory opens or write-intent opens appropriately
- [x] 4.3 Implement `read` to return the requested file bytes from Yandex Disk using byte ranges when available and a safe fallback when they are not
- [x] 4.4 Ensure reads at or past EOF return correct empty results without corrupting offsets or byte order

## 5. Read-only enforcement and validation

- [x] 5.1 Explicitly reject unsupported mutation operations such as create, write, rename, unlink, mkdir, and rmdir with consistent read-only errors
- [x] 5.2 Validate the mount manually against real Yandex Disk content from the provided token, covering root listing, nested traversal, file reads, and failure cases
- [x] 5.3 Document the expected startup usage, required environment variables, and known limitations of the first read-only implementation
