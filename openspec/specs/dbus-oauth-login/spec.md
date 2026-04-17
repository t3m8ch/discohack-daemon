# dbus-oauth-login Specification

## Purpose
TBD - created by archiving change add-dbus-oauth-service. Update Purpose after archive.
## Requirements
### Requirement: Expose authentication state over the session bus
The daemon SHALL publish a `zbus`-backed D-Bus interface on the session bus under the bus name `ru.literallycats.daemon` and SHALL expose whether Yandex credentials are currently available through an `IsAuth` property.

#### Scenario: Service starts without stored credentials
- **WHEN** the daemon starts and no persisted Yandex credentials can be loaded
- **THEN** the D-Bus interface is still available and `IsAuth` is `false`

#### Scenario: Service restores existing credentials
- **WHEN** the daemon starts and previously stored Yandex credentials are available for use
- **THEN** the D-Bus interface reports `IsAuth` as `true`

### Requirement: Begin Yandex OAuth login with PKCE over D-Bus
The daemon SHALL provide a D-Bus method that starts a Yandex OAuth Authorization Code + PKCE flow, generates a `code_verifier`, derives an S256 `code_challenge`, and returns the browser-launch data needed by the frontend.

#### Scenario: Frontend starts login
- **WHEN** a frontend calls `BeginLogin()` while no login is already pending
- **THEN** the daemon returns an authorization payload that includes a valid `code_challenge`, the redirect URI `http://localhost:6532/oauth/yandex-disk`, and an authorize URL for the Yandex OAuth page

#### Scenario: Concurrent login attempt is rejected
- **WHEN** a frontend calls `BeginLogin()` while another login flow is still pending
- **THEN** the daemon returns a clear error instead of creating a second concurrent PKCE session

### Requirement: Complete the localhost OAuth callback and persist tokens securely
The daemon SHALL listen for the OAuth redirect on `http://localhost:6532/oauth/yandex-disk`, exchange the returned authorization code for Yandex access and refresh tokens, and persist the resulting credentials in Secret Service rather than `.env`.

#### Scenario: Successful callback stores tokens
- **WHEN** Yandex redirects to the daemon's localhost callback with a valid authorization `code`
- **THEN** the daemon exchanges the code using the original `code_verifier`, stores the access and refresh tokens in Secret Service, and updates `IsAuth` to `true`

#### Scenario: Callback arrives without a pending login
- **WHEN** the localhost callback receives a request but no PKCE login session is pending
- **THEN** the daemon rejects the callback and does not overwrite any stored credentials

### Requirement: Start the mount automatically after successful login
The daemon SHALL automatically start the configured read-only Yandex Disk mount after credentials have been persisted successfully.

#### Scenario: Login completes with mount configuration available
- **WHEN** the OAuth exchange succeeds and the daemon has the information required to mount the filesystem
- **THEN** it starts the read-only Yandex Disk mount without requiring a second frontend command

#### Scenario: Automatic mount happens after auth state is updated
- **WHEN** login succeeds and the daemon transitions into the authenticated state
- **THEN** the automatic mount uses the newly persisted managed credentials instead of any token from `.env`

### Requirement: Notify clients when login completes
The daemon SHALL emit a D-Bus signal after successful login so subscribed clients can react without polling.

#### Scenario: Frontend subscribes before OAuth completion
- **WHEN** a frontend subscribes to `LoginCompleted` and the OAuth exchange succeeds
- **THEN** the daemon emits `LoginCompleted` after tokens have been persisted, `IsAuth` has been updated, and automatic mount startup has been initiated

### Requirement: Reuse and refresh managed credentials for later Yandex API access
The daemon SHALL use the stored credentials for subsequent Yandex API requests and SHALL refresh the access token with the refresh token when the active access token is no longer usable.

#### Scenario: Expired access token is refreshed transparently
- **WHEN** the daemon needs to call the Yandex API and the stored access token is expired or rejected but a valid refresh token exists
- **THEN** the daemon refreshes the credentials, persists the new token set, and continues the API call with the refreshed access token

#### Scenario: No valid credentials remain
- **WHEN** the daemon needs Yandex API access and neither a usable access token nor a refreshable token set is available
- **THEN** the daemon reports an authentication-required state and keeps `IsAuth` as `false` until a new login succeeds

