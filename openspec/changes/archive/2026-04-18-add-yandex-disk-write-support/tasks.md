## 1. Extend the Yandex Disk client for write operations

- [x] 1.1 Verify the Yandex Disk mutation endpoints needed for upload, delete, mkdir, and rename, and model their responses in `src/yadisk.rs`
- [x] 1.2 Implement authenticated client helpers for upload URL resolution, file upload, directory creation, resource deletion, and rename/move with the existing token refresh flow
- [x] 1.3 Add focused tests for new client-side error mapping and mutation response handling where practical

## 2. Add write-back file handle support in the FUSE layer

- [x] 2.1 Extend `FsState` with writable handle/session tracking, including staged temp files, dirty flags, and handle-to-path bookkeeping
- [x] 2.2 Implement writable `open`, `create`, `write`, `setattr(size)`, `flush`, `fsync`, and `release` behavior that stages local changes and uploads them on commit
- [x] 2.3 Remove read-only mount behavior and update exposed permissions, access checks, and mount options so the filesystem is advertised as writable

## 3. Implement remote mutation flows and cache updates

- [x] 3.1 Implement `mkdir`, `unlink`, `rmdir`, and `rename` by calling the new Yandex Disk client mutation helpers and returning operation-specific errno values on failure
- [x] 3.2 Update or invalidate inode, path, directory-child, and download URL caches after successful uploads, deletes, and renames so later lookups reflect remote state
- [x] 3.3 Preserve unrelated-request concurrency by keeping blocking network I/O outside the main filesystem state lock during downloads and uploads

## 4. Verify behavior and document writable support

- [x] 4.1 Add or expand tests for create/write/overwrite/truncate, delete, rename, concurrent upload behavior, and failed commit scenarios
- [x] 4.2 Update `README.md` and relevant docs to describe writable mount behavior, save semantics, and remaining write-path limitations
