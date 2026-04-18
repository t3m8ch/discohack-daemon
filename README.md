# discohack-daemon

FUSE mount for Yandex Disk with read and write support.

## Requirements

- Linux with FUSE support (`/dev/fuse`)
- A running session D-Bus
- A Secret Service provider in the user session (for example GNOME Keyring or KWallet)
- Rust toolchain

## Configuration

The daemon no longer requires `YANDEX_DISK_TOKEN` in `.env` as the primary auth path.

Authentication is handled at runtime through:
- D-Bus service `ru.literallycats.daemon`
- Yandex OAuth PKCE flow
- localhost callback `http://localhost:6532/oauth/yandex-disk`
- Secret Service storage via the `secret-service` crate

The Yandex OAuth client is configured with redirect URI:

```text
http://localhost:6532/oauth/yandex-disk
```

## Run

```bash
cargo run -- <mountpoint>
```

Example:

```bash
mkdir -p /tmp/yadisk-mnt
cargo run -- /tmp/yadisk-mnt
```

Startup behavior:
- the daemon registers the session-bus name `ru.literallycats.daemon`
- it starts the OAuth callback listener on `http://localhost:6532/oauth/yandex-disk`
- if stored credentials already exist in Secret Service, it can mount immediately
- otherwise it stays alive with `IsAuth = false` until login completes

After successful login the daemon mounts Yandex Disk automatically. Then inspect and modify the mounted filesystem:

```bash
ls -la /tmp/yadisk-mnt
cat /tmp/yadisk-mnt/some-file.txt
echo hello > /tmp/yadisk-mnt/hello.txt
mkdir -p /tmp/yadisk-mnt/new-dir
mv /tmp/yadisk-mnt/hello.txt /tmp/yadisk-mnt/new-dir/hello.txt
```

Stop the daemon with `Ctrl-C` or `SIGTERM` for a graceful shutdown. The daemon now attempts to unmount the FUSE mount before exiting so the same mountpoint can be reused immediately.

If graceful cleanup fails unexpectedly, recover the mountpoint manually:

```bash
fusermount -u /tmp/yadisk-mnt
```

## Logging

The daemon uses `tracing` for structured lifecycle logs.

- Default log level: `info`
- Override with `RUST_LOG`, for example:

```bash
RUST_LOG=debug cargo run -- /tmp/yadisk-mnt
```

## Current behavior

- Exposes `disk:/` as the mount root
- Supports directory traversal via `lookup`, `getattr`, and `readdir`
- Supports regular file reads through Yandex Disk download URLs
- Supports writable opens, file creation, writes, truncation, flush/fsync commit, and release-time commit
- Supports `mkdir`, `unlink`, `rmdir`, and `rename`
- Uses short-lived metadata and download-URL caching to reduce repeated API calls
- Uses local staging files for writable handles and uploads the full staged file back to Yandex Disk on commit

## Save semantics

Writes are implemented as a write-back filesystem:

- opening an existing file for write stages the current remote contents in a local temp file
- `write` and `truncate` modify that staged file locally
- `flush`, `fsync`, and final `release` upload the whole staged file back to Yandex Disk
- a failed upload returns an error and leaves the previous remote contents authoritative

## Limitations

- Writes are whole-file uploads, not remote block patches
- Large writes depend on local temp storage and full upload latency
- `mknod` and other advanced filesystem operations are still not implemented
- Behavior for complex concurrent mutation patterns follows a practical first implementation rather than full POSIX parity in every edge case

# Authentication

The daemon exposes a D-Bus API on:
- service: `ru.literallycats.daemon`
- object path: `/ru/literallycats/daemon`
- interface: `ru.literallycats.daemon`

Current interface:
- property `IsAuth`
- method `BeginLogin()`
- signal `LoginCompleted`

`BeginLogin()` returns:
- `authorize_url`
- `code_challenge`
- `redirect_uri`

PKCE details:
- the daemon generates a random `code_verifier`
- it computes `code_challenge = BASE64URL_NO_PAD(SHA256(code_verifier))`
- Yandex receives the `code_challenge` during `/authorize`
- the daemon sends the original `code_verifier` to `/token`

High-level flow:

```mermaid
sequenceDiagram
    participant Y as Yandex
    participant U as User
    participant F as Frontend
    participant D as DBus
    participant B as Backend
    participant S as SecretService

    F->>B: Read IsAuth
    B->>F: false
    U->>F: Click Login
    F->>B: BeginLogin()
    B->>F: authorize_url, code_challenge, redirect_uri
    F->>D: Subscribe LoginCompleted
    F->>U: Open authorize_url in browser
    U->>Y: Grant access
    Y->>B: Redirect to http://localhost:6532/oauth/yandex-disk?code=...
    B->>Y: POST /token with code, client_id, code_verifier
    Y->>B: access_token, refresh_token
    B->>S: Store tokens in Secret Service
    B->>B: Start FUSE mount automatically
    B->>F: Emit LoginCompleted
```

Operational notes:
- the daemon uses Secret Service as the persistent token store
- access tokens are refreshed with the stored refresh token when needed
- the mounted filesystem continues using service-managed credentials instead of `.env`
