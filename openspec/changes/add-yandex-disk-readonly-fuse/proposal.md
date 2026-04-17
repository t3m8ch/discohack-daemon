## Why

The project currently mounts a single hardcoded file through FUSE, which is useful only as a minimal example. We need a real read-only filesystem view over Yandex Disk so the daemon can expose remote files and directories through the existing Rust/FUSE approach using the token already provided in `.env`.

## What Changes

- Replace the single-file demo filesystem in `src/main.rs` with a read-only FUSE filesystem backed by the Yandex Disk HTTP API.
- Add Yandex Disk API integration for listing directory contents and fetching file metadata from `disk:/` and nested paths.
- Implement file reads by resolving Yandex Disk download URLs and streaming or buffering remote file content for FUSE `read` calls.
- Represent remote directories and files with stable inode bookkeeping sufficient for lookup, getattr, readdir, open, and read operations.
- Load configuration from environment, including the OAuth token, and fail clearly when required configuration is missing.
- Keep the filesystem strictly read-only: no create, write, rename, delete, or metadata mutation support.

## Capabilities

### New Capabilities
- `yandex-disk-readonly-fuse`: Mount Yandex Disk as a read-only FUSE filesystem with directory traversal and file reads.

### Modified Capabilities

## Impact

- Affected code: `src/main.rs`, `Cargo.toml`, and likely new Rust modules for API client, inode/path mapping, and filesystem state.
- External API: Yandex Disk REST API endpoints for resource listing and download resolution.
- Dependencies: likely an HTTP client, JSON deserialization, dotenv/env loading, URL handling, and possibly lightweight caching utilities.
- Runtime behavior: mount now depends on network access and a valid Yandex Disk OAuth token in the environment.
