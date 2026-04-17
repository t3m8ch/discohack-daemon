## Context

The current daemon is a one-shot CLI that reads a Yandex OAuth token from `.env`/environment, constructs a `YandexDiskClient` with a fixed `Authorization` header, and mounts the read-only FUSE filesystem immediately. The requested change introduces a frontend-driven login flow over D-Bus, uses `zbus` for the service API under the session-bus name `ru.literallycats.daemon`, follows the PKCE-based Yandex OAuth sequence from `README.md`, uses the fixed callback endpoint `http://localhost:6532/oauth/yandex-disk`, and stores tokens securely instead of expecting a manually provisioned token.

This is a cross-cutting change:
- process lifecycle changes from "mount immediately or exit" to "run as a session service with auth state"
- authentication changes from static config to runtime OAuth with callback handling
- request authorization changes from a fixed token string to a refreshable credential source
- secrets move out of `.env` and into secure storage

The design must preserve the existing read-only FUSE behavior after authentication while keeping the control path understandable for a frontend client.

## Goals / Non-Goals

**Goals:**
- Expose a session-bus D-Bus API with `zbus` for auth state and login initiation.
- Implement Yandex OAuth Authorization Code + PKCE, including S256 `code_challenge` generation in Rust.
- Receive the OAuth redirect on localhost, exchange `code` + `code_verifier` for access/refresh tokens, and persist them securely.
- Make the Yandex Disk client use service-managed credentials and refresh tokens as needed.
- Keep the existing read-only FUSE semantics and graceful shutdown behavior once valid credentials are available.
- Allow the service to start without credentials and report `IsAuth = false` instead of failing immediately.

**Non-Goals:**
- Building the frontend UI itself.
- Adding write support or changing the read-only filesystem contract.
- Supporting multiple simultaneous Yandex accounts in one daemon instance.
- Moving to the system bus or adding privileged D-Bus integration.
- Replacing the Yandex Disk HTTP protocol with another backend.

## Decisions

### 1. Use a session-bus `zbus` service as the control plane
The daemon will export a single primary D-Bus interface on the session bus under the bus name `ru.literallycats.daemon`, with a matching stable object path/interface (for example `/ru/literallycats/daemon` and `ru.literallycats.daemon`), and surface auth state through properties/methods/signals.

Proposed interface shape:
- property `IsAuth: bool`
- method `BeginLogin()` returning login payload needed by the frontend (`authorize_url`, `code_challenge`, `redirect_uri`)
- signal `LoginCompleted` emitted after tokens are exchanged and persisted
- optional follow-up methods/properties for mount/service state can be added in the same interface without changing the auth model

**Why:** the frontend sequence in `README.md` is explicitly D-Bus-driven, and the session bus is the least-privileged fit for a per-user desktop flow.

**Alternatives considered:**
- System bus: unnecessary privilege and deployment complexity.
- Custom localhost control API only: duplicates IPC concerns already solved by D-Bus and breaks the requested architecture.
- `dbus` crate instead of `zbus`: rejected because the change explicitly requires `zbus`.

### 2. Split runtime into control-state modules instead of folding D-Bus into the existing `main.rs`
The implementation should introduce explicit modules such as:
- `dbus_service`: exported `zbus` interface and signal emission
- `auth`: PKCE generation, pending-login state, token exchange, token refresh logic
- `callback`: localhost HTTP listener for the OAuth redirect
- `secrets`: secure load/store of access and refresh tokens
- `mount`: existing FUSE session lifecycle and state transitions
- `yadisk`: HTTP client refactored to obtain fresh auth headers from a token provider

**Why:** this change cuts across lifecycle, networking, storage, and filesystem concerns. A small module split reduces accidental coupling and makes the auth flow testable without mounting FUSE.

**Alternatives considered:**
- Keep a monolithic `main.rs`: too hard to evolve once D-Bus, callback handling, and refresh logic interact.
- Refactor everything into a fully generic service framework first: unnecessary for the current scope.

### 3. `BeginLogin()` creates a pending PKCE session and returns browser-ready data
When the frontend calls `BeginLogin()`, the service will:
1. generate a cryptographically random `code_verifier`
2. derive `code_challenge = BASE64URL_NO_PAD(SHA256(code_verifier))`
3. create or reuse a pending login record with timeout protection
4. start the localhost callback listener if it is not already active
5. return the authorization payload to the frontend

The returned payload will include the final authorize URL in addition to the raw `code_challenge`, so the frontend can simply open the browser while still matching the documented sequence.

**Why:** returning both values keeps the API practical for the frontend and makes the service the source of truth for redirect URI and PKCE parameters.

**Alternatives considered:**
- Return only `code_challenge`: matches the diagram literally, but duplicates URL construction logic in the frontend.
- Have the backend launch the browser itself: reduces frontend control and is less testable over D-Bus.

### 4. Capture the OAuth authorization code through a loopback HTTP callback server
The service will host a small localhost-only HTTP listener dedicated to the OAuth redirect. The redirect URI is fixed to `http://localhost:6532/oauth/yandex-disk`, matching the Yandex application configuration already in use.

On callback:
- validate that a login session is pending
- extract the `code` query parameter
- exchange it against Yandex `/token` with `grant_type=authorization_code`, `client_id`, and the original `code_verifier`
- persist access/refresh tokens and expiry metadata
- clear pending login state
- emit `LoginCompleted`

**Why:** the README sequence explicitly requires Yandex to call back to localhost. A fixed loopback endpoint is simpler to document and configure than an ephemeral port strategy.

**Alternatives considered:**
- Manual paste of the auth code into the frontend: worse UX and diverges from the sequence.
- Ephemeral callback port: possible, but awkward if the OAuth client registration expects a fixed redirect URI.

### 5. Persist tokens in secret storage and restore auth state on service startup
Access token, refresh token, expiry time, and client metadata will be stored in Secret Service using the `secret-service` crate. On startup, the daemon will try to load stored credentials and derive initial auth state before exporting `IsAuth`.

**Why:** the goal is to remove token management from `.env` while keeping credentials available across restarts.

**Alternatives considered:**
- Plaintext file in the project directory: easy, but weak security and contrary to the intended libsecret flow.
- Environment-only tokens: the current model and the problem we are replacing.

### 6. Refactor Yandex API authorization behind a shared token provider with refresh-on-demand
`YandexDiskClient::new(token)` currently bakes the token into default headers. It should instead depend on a shared auth provider that can:
- return the current access token for each API request
- determine whether the token is expired or near expiry
- perform a single in-flight refresh using the stored refresh token
- update secret storage after successful refresh

The HTTP client should build the `Authorization: OAuth <token>` header per request rather than once at construction time.

**Why:** a D-Bus-authenticated service must survive access-token expiry without forcing the user to re-login during normal filesystem use.

**Alternatives considered:**
- Recreate the whole Yandex client whenever tokens change: workable, but adds session churn and shared-state complexity.
- Require re-login on each expiry: poor UX and unnecessary because refresh tokens are available.

### 7. Keep the existing FUSE data path mostly synchronous and isolate it from the async control plane
The D-Bus service, callback listener, and Secret Service interactions are a good fit for an async runtime. The existing FUSE implementation already uses worker threads and blocking HTTP calls. The design should keep FUSE request handling largely intact and let the control plane manage auth/mount readiness.

Practical model:
- main runtime hosts `zbus` on `ru.literallycats.daemon` and the callback listener on `http://localhost:6532/oauth/yandex-disk`
- mount lifecycle is managed by a `MountManager`
- successful login automatically triggers mount startup for the configured mountpoint
- FUSE worker threads continue to call blocking Yandex HTTP methods
- the shared auth provider is synchronized so blocking readers can safely obtain/refesh tokens

**Why:** this minimizes churn in the most sensitive path (filesystem operations) while still adopting the async service patterns needed for D-Bus and HTTP callbacks.

**Alternatives considered:**
- Rewrite FUSE I/O to fully async: high complexity for limited benefit because `fuser` callbacks are already synchronous.
- Use only blocking D-Bus APIs: possible, but makes concurrent callback and signal handling less ergonomic.

## Risks / Trade-offs

- [OAuth callback port is already occupied or unavailable] → Fail `BeginLogin()` clearly and keep the service running so the frontend can retry after configuration changes.
- [Concurrent FUSE requests trigger multiple token refreshes] → Guard refresh with a single shared mutex/once-cell style in-flight refresh path.
- [Secret Service is unavailable in the user session] → Surface a clear auth error and do not silently fall back to plaintext storage.
- [Frontend and backend disagree on the D-Bus contract] → Return browser-ready data from `BeginLogin()` and keep interface/version naming explicit in code and docs.
- [Auth succeeds but stale cached HTTP state still uses the previous token] → Build auth headers per request and invalidate any cached token snapshot after refresh/login.
- [The daemon no longer mounts immediately on startup] → Document the new lifecycle clearly and expose `IsAuth` so the frontend can guide the user.

## Migration Plan

1. Add dependencies for `zbus`, the `secret-service` crate, PKCE hashing/base64url encoding, and any async runtime required by the chosen `zbus` setup.
2. Introduce auth state, secret loading, and a token-provider abstraction without changing the existing read-only FUSE behavior yet.
3. Refactor `YandexDiskClient` to fetch authorization headers from the shared token provider and add refresh handling.
4. Add the localhost OAuth callback listener on `http://localhost:6532/oauth/yandex-disk` and token exchange flow.
5. Add the exported D-Bus interface on `ru.literallycats.daemon` (`IsAuth`, `BeginLogin`, `LoginCompleted`) and wire it to auth state transitions.
6. Change startup behavior so the service can remain alive unauthenticated and automatically start the mount after successful login.
7. Update README and operational docs from `.env` token provisioning to D-Bus-driven login.
8. Validate end-to-end: startup unauthenticated, call `BeginLogin`, open browser, receive callback, persist tokens, emit `LoginCompleted`, and confirm the filesystem still works.

Rollback strategy: revert to the previous env-token startup path and remove the D-Bus/auth modules if the service migration proves unstable.

## Open Questions

- Should `BeginLogin()` reject a second concurrent login attempt, or return the existing pending session data until it expires?
- Do we need an explicit `Logout()`/token-clear method in this same change, or can it wait for a follow-up?