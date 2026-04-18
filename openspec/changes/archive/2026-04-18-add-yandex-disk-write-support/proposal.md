## Why

The daemon currently exposes Yandex Disk as a read-only FUSE mount, which makes it useful for browsing and reading files but not for real file management or editing. We already have working OAuth authentication and example API flows for upload and resource mutations, so this is the right time to turn the mount into a practical writable filesystem.

## What Changes

- Add FUSE support for creating files, writing data, truncating files, and persisting file contents back to Yandex Disk.
- Add directory and resource mutation support for `mkdir`, `unlink`, `rmdir`, and `rename` by mapping them to Yandex Disk resource APIs.
- Change exposed file and directory attributes from read-only semantics to writable semantics where the mounted Yandex Disk resource can be mutated.
- Keep local inode, directory, and metadata caches coherent after successful uploads, deletes, and renames.
- Preserve the existing service-managed OAuth flow and return clear filesystem errors when Yandex Disk rejects a mutation or authentication is unavailable.

## Capabilities

### New Capabilities
<!-- None. -->

### Modified Capabilities
- `yandex-disk-readonly-fuse`: extend the existing mount contract from read-only browsing to authenticated read-write file and directory operations backed by Yandex Disk APIs.

## Impact

- Affected code: `src/fs.rs`, `src/yadisk.rs`, `src/mount.rs`, and related tests.
- External APIs: Yandex Disk resource mutation and upload endpoints in addition to the existing read endpoints.
- Dependencies: may require a temporary-file helper for staging uploads safely before remote commit.
- User-visible behavior: mounted files and directories become writable instead of always returning read-only filesystem errors.
