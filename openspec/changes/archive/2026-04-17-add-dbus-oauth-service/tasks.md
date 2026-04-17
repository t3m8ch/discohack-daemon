## 1. Service and dependency setup

- [x] 1.1 Add the Rust dependencies needed for `zbus`, runtime support, PKCE hashing/base64url encoding, and the `secret-service` crate
- [x] 1.2 Refactor the binary entrypoint into modules for D-Bus service, auth state, callback handling, secret storage, and mount/session management
- [x] 1.3 Define shared service state types for auth status, pending login sessions, persisted credentials, and mount readiness

## 2. OAuth PKCE and credential management

- [x] 2.1 Implement PKCE helper functions that generate a secure `code_verifier` and derive the S256 `code_challenge` in Rust
- [x] 2.2 Implement Yandex OAuth token exchange for the localhost callback flow at `http://localhost:6532/oauth/yandex-disk` using `grant_type=authorization_code`, `client_id`, `code`, and `code_verifier`
- [x] 2.3 Implement refresh-token exchange and shared logic for detecting expired or rejected access tokens
- [x] 2.4 Implement secure load/store of access token, refresh token, and expiry metadata in Secret Service via the `secret-service` crate and restore auth state on startup

## 3. D-Bus login flow

- [x] 3.1 Export a session-bus `zbus` interface on `ru.literallycats.daemon` with the `IsAuth` property and a `BeginLogin()` method
- [x] 3.2 Make `BeginLogin()` create a pending login session, reject concurrent login attempts, and return the authorize URL, redirect URI `http://localhost:6532/oauth/yandex-disk`, and `code_challenge`
- [x] 3.3 Implement the localhost OAuth callback listener on `http://localhost:6532/oauth/yandex-disk` that validates pending login state, exchanges the authorization code, persists tokens, and updates auth state
- [x] 3.4 Emit the `LoginCompleted` D-Bus signal only after credentials are stored and `IsAuth` has been updated

## 4. Yandex Disk client and filesystem integration

- [x] 4.1 Refactor `YandexDiskClient` so API requests obtain `Authorization: OAuth <token>` from a shared token provider instead of a token baked in at construction time
- [x] 4.2 Wire the refreshable auth provider into the existing read-only filesystem path so metadata lookups and file reads continue to work after login, automatic mount startup, and token refresh
- [x] 4.3 Remove the current `.env`-driven token requirement as the normal auth path and return clear unauthenticated errors when managed credentials are unavailable

## 5. Runtime lifecycle and shutdown

- [x] 5.1 Change startup behavior so the daemon can run as a D-Bus service with `IsAuth = false` instead of exiting immediately when credentials are absent
- [x] 5.2 Integrate the `ru.literallycats.daemon` D-Bus service, the `http://localhost:6532/oauth/yandex-disk` callback listener, auth state, and FUSE mount lifecycle into one coordinated runtime without regressing graceful shutdown behavior
- [x] 5.3 Ensure successful login automatically starts the mount and that later service-managed credential updates become visible to already running filesystem operations without requiring a process restart

## 6. Validation and documentation

- [x] 6.1 Add automated tests for PKCE generation, pending-login rejection, callback success/failure handling, token refresh, and unauthenticated error paths
- [ ] 6.2 Run end-to-end validation for the documented frontend sequence: check `IsAuth`, call `BeginLogin()`, open the browser URL, receive `LoginCompleted`, verify the mount starts automatically, and confirm the filesystem still behaves as read-only
- [x] 6.3 Update `README.md` and operational docs to describe the `ru.literallycats.daemon` D-Bus login flow, the `http://localhost:6532/oauth/yandex-disk` callback, Secret Service storage via `secret-service`, automatic mount after login, and the removal of `.env` as the primary auth path
