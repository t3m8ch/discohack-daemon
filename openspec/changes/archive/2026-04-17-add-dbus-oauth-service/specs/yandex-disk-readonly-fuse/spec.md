## REMOVED Requirements

### Requirement: Require valid API authentication at startup
**Reason**: Authentication is no longer sourced primarily from `.env` or startup environment variables. The daemon now relies on service-managed OAuth credentials obtained through the D-Bus login flow and persisted secret storage.
**Migration**: Start the daemon service, check `IsAuth` over D-Bus, complete login with `BeginLogin()` if needed, and only then continue normal filesystem use.

## ADDED Requirements

### Requirement: Use service-managed OAuth credentials for Yandex Disk access
The daemon SHALL obtain Yandex Disk credentials from the service-managed auth state and SHALL not require `YANDEX_DISK_TOKEN`, `TOKEN`, or `YANDEX_TOKEN` in `.env` for normal operation.

#### Scenario: Stored credentials are reused after restart
- **WHEN** the daemon restarts after a previous successful login
- **THEN** it uses the persisted service-managed credentials for Yandex Disk API access without requiring the token to be re-entered in `.env`

#### Scenario: Refreshed credentials are used by filesystem operations
- **WHEN** the auth subsystem refreshes an expired access token while the filesystem is active
- **THEN** subsequent metadata and file-read requests use the refreshed credentials without changing the read-only filesystem behavior

### Requirement: Start the filesystem mount automatically after successful login
The daemon SHALL automatically start the read-only Yandex Disk mount after a successful D-Bus login once managed credentials are available.

#### Scenario: First successful login starts the mount
- **WHEN** the daemon completes the Yandex OAuth flow successfully for an unauthenticated session
- **THEN** it starts the read-only filesystem mount without requiring a separate mount command

#### Scenario: Automatic mount uses managed credentials
- **WHEN** the automatic mount starts after login
- **THEN** it authenticates Yandex Disk access with the newly managed service credentials rather than any token from `.env`

### Requirement: Unauthenticated filesystem startup or access fails clearly
When the daemon has no valid service-managed Yandex credentials, it SHALL report a clear unauthenticated state instead of silently falling back to environment-token configuration.

#### Scenario: Service starts before login
- **WHEN** the daemon starts and no valid service-managed credentials are available yet
- **THEN** it remains available for D-Bus login and reports that authentication is required before normal Yandex Disk access can proceed

#### Scenario: Mount or API access is attempted without valid credentials
- **WHEN** the daemon needs Yandex Disk access but no valid managed credentials are available
- **THEN** it returns a clear authentication error rather than attempting to read a token from `.env`
