## Why

The daemon currently starts as a CLI process and can mount Yandex Disk only when a long-lived OAuth token is preloaded through `.env` or environment variables. We need to turn it into a D-Bus-driven service so a frontend can initiate OAuth login, open the browser for the user, complete the Yandex PKCE flow, and then continue using the existing read-only mount behavior without manual token management.

## What Changes

- Convert the daemon into a session-bus D-Bus service named `ru.literallycats.daemon` implemented with `zbus`.
- Add D-Bus methods and signals for the authorization flow described in `README.md`, including auth status inspection, login start, localhost callback completion at `http://localhost:6532/oauth/yandex-disk`, and login completion notification.
- Implement Yandex OAuth Authorization Code + PKCE in Rust, including correct S256 `code_challenge` generation from a `code_verifier`.
- Replace `.env` token startup requirements with runtime authorization and persisted token storage for access and refresh tokens using the `secret-service` crate.
- Add token lifecycle handling so the mounted filesystem can keep working after login and can obtain a fresh access token when needed.
- Automatically start the read-only Yandex Disk mount after successful login, while routing credential access through the new service state instead of static environment configuration.
- **BREAKING**: the daemon will no longer rely on `YANDEX_DISK_TOKEN` as the primary authentication path for normal operation.

## Capabilities

### New Capabilities
- `dbus-oauth-login`: Expose a `zbus` session-bus API on `ru.literallycats.daemon` for auth status, starting the Yandex PKCE login flow, receiving the localhost OAuth callback at `http://localhost:6532/oauth/yandex-disk`, persisting tokens securely, and notifying clients when login completes.

### Modified Capabilities
- `yandex-disk-readonly-fuse`: Change authentication requirements so filesystem access is backed by service-managed OAuth credentials rather than a token loaded from `.env` at startup.

## Impact

- Affected code: `src/main.rs`, `src/yadisk.rs`, FUSE/session lifecycle code in `src/fs.rs`, plus new modules for D-Bus API, OAuth/PKCE flow, token persistence, localhost callback handling, and post-login mount orchestration.
- External APIs and systems: D-Bus session bus, Yandex OAuth authorize/token endpoints, the fixed localhost callback endpoint `http://localhost:6532/oauth/yandex-disk`, Secret Service for secure token storage, and the existing Yandex Disk REST API.
- Dependencies: `zbus`, async runtime support if required by the D-Bus server, PKCE hashing/base64url helpers, a browser-launch or URL-return strategy, and the `secret-service` crate.
- Runtime behavior: startup shifts from "mount immediately with env token" to "run `ru.literallycats.daemon` as a service, authorize via D-Bus, automatically mount after successful login, and then operate the existing read-only filesystem using managed credentials."