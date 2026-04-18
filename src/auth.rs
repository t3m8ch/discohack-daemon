use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use rand::RngCore;
use reqwest::{
    StatusCode, Url,
    blocking::{Client, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tracing::{debug, info, warn};
use zbus::zvariant::Type;

pub const YANDEX_CLIENT_ID: &str = "f0a18436bca24de4acf033329b0d5933";
pub const YANDEX_AUTHORIZE_URL: &str = "https://oauth.yandex.ru/authorize";
pub const YANDEX_TOKEN_URL: &str = "https://oauth.yandex.ru/token";
pub const YANDEX_REDIRECT_URI: &str = "http://localhost:6532/oauth/yandex-disk";

const TOKEN_REFRESH_SKEW: i64 = 30;
const PENDING_LOGIN_TTL: Duration = Duration::from_secs(600);
const PKCE_RANDOM_BYTES: usize = 32;
const USER_AGENT: &str = "discohack-daemon/0.1.0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct BeginLoginResponse {
    pub authorize_url: String,
    pub code_challenge: String,
    pub redirect_uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_epoch_seconds: Option<i64>,
}

impl StoredCredentials {
    fn is_expired(&self) -> bool {
        self.expires_at_epoch_seconds
            .map(|expires_at| expires_at <= Utc::now().timestamp() + TOKEN_REFRESH_SKEW)
            .unwrap_or(false)
    }
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("login is already pending")]
    LoginAlreadyPending,
    #[error("no pending login session")]
    NoPendingLogin,
    #[error("authentication is required")]
    AuthenticationRequired,
    #[error("failed to build authorize URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("token endpoint returned {status}: {body}")]
    TokenEndpoint { status: StatusCode, body: String },
    #[error("token response was invalid: {0}")]
    InvalidTokenResponse(String),
    #[error("secret storage error: {0}")]
    SecretStorage(String),
}

pub trait CredentialStore: Send + Sync {
    fn load_credentials(&self) -> Result<Option<StoredCredentials>, AuthError>;
    fn save_credentials(&self, credentials: &StoredCredentials) -> Result<(), AuthError>;
}

pub trait OAuthClient: Send + Sync {
    fn exchange_authorization_code(
        &self,
        code: &str,
        code_verifier: &str,
    ) -> Result<StoredCredentials, AuthError>;
    fn refresh_access_token(&self, refresh_token: &str) -> Result<StoredCredentials, AuthError>;
}

#[derive(Debug)]
struct PendingLogin {
    code_verifier: String,
    code_challenge: String,
    created_at: Instant,
}

#[derive(Debug, Default)]
struct AuthState {
    credentials: Option<StoredCredentials>,
    pending_login: Option<PendingLogin>,
}

pub struct AuthManager {
    oauth_client: Arc<dyn OAuthClient>,
    store: Arc<dyn CredentialStore>,
    state: Mutex<AuthState>,
}

impl AuthManager {
    pub fn new(
        oauth_client: Arc<dyn OAuthClient>,
        store: Arc<dyn CredentialStore>,
    ) -> Result<Self, AuthError> {
        let credentials = store.load_credentials()?;
        Ok(Self {
            oauth_client,
            store,
            state: Mutex::new(AuthState {
                credentials,
                pending_login: None,
            }),
        })
    }

    pub fn is_authenticated(&self) -> bool {
        self.state.lock().unwrap().credentials.as_ref().is_some()
    }

    pub fn begin_login(&self) -> Result<BeginLoginResponse, AuthError> {
        let mut state = self.state.lock().unwrap();
        if state
            .pending_login
            .as_ref()
            .is_some_and(|pending| pending.created_at.elapsed() < PENDING_LOGIN_TTL)
        {
            return Err(AuthError::LoginAlreadyPending);
        }

        let code_verifier = generate_code_verifier()?;
        let code_challenge = derive_code_challenge(&code_verifier);
        let authorize_url = build_authorize_url(&code_challenge)?;
        let response = BeginLoginResponse {
            authorize_url,
            code_challenge,
            redirect_uri: YANDEX_REDIRECT_URI.to_owned(),
        };

        info!(
            code_challenge = %response.code_challenge,
            redirect_uri = %response.redirect_uri,
            "created pending PKCE login"
        );
        debug!(code_verifier = %code_verifier, "generated PKCE verifier");

        state.pending_login = Some(PendingLogin {
            code_verifier,
            code_challenge: response.code_challenge.clone(),
            created_at: Instant::now(),
        });

        Ok(response)
    }

    pub fn complete_login(&self, code: &str) -> Result<(), AuthError> {
        let pending = {
            let mut state = self.state.lock().unwrap();
            state
                .pending_login
                .take()
                .ok_or(AuthError::NoPendingLogin)?
        };

        debug!(
            code = %code,
            code_verifier = %pending.code_verifier,
            code_challenge = %pending.code_challenge,
            "handling OAuth callback"
        );

        let credentials = match self
            .oauth_client
            .exchange_authorization_code(code, &pending.code_verifier)
        {
            Ok(credentials) => credentials,
            Err(err) => {
                warn!(
                    error = %err,
                    code_challenge = %pending.code_challenge,
                    "OAuth code exchange failed"
                );
                let mut state = self.state.lock().unwrap();
                state.pending_login = Some(pending);
                return Err(err);
            }
        };

        self.store.save_credentials(&credentials)?;
        let mut state = self.state.lock().unwrap();
        state.credentials = Some(credentials);
        Ok(())
    }

    pub fn access_token(&self) -> Result<String, AuthError> {
        let mut state = self.state.lock().unwrap();
        let credentials = state
            .credentials
            .clone()
            .ok_or(AuthError::AuthenticationRequired)?;

        if !credentials.is_expired() {
            return Ok(credentials.access_token);
        }

        let refreshed = self
            .oauth_client
            .refresh_access_token(&credentials.refresh_token)?;
        self.store.save_credentials(&refreshed)?;
        let token = refreshed.access_token.clone();
        state.credentials = Some(refreshed);
        Ok(token)
    }

    pub fn refresh_access_token(&self) -> Result<String, AuthError> {
        let mut state = self.state.lock().unwrap();
        let credentials = state
            .credentials
            .clone()
            .ok_or(AuthError::AuthenticationRequired)?;
        let refreshed = self
            .oauth_client
            .refresh_access_token(&credentials.refresh_token)?;
        self.store.save_credentials(&refreshed)?;
        let token = refreshed.access_token.clone();
        state.credentials = Some(refreshed);
        Ok(token)
    }
}

pub fn generate_code_verifier() -> Result<String, AuthError> {
    let mut random = [0u8; PKCE_RANDOM_BYTES];
    rand::thread_rng().fill_bytes(&mut random);
    Ok(URL_SAFE_NO_PAD.encode(random))
}

pub fn derive_code_challenge(code_verifier: &str) -> String {
    let digest = Sha256::digest(code_verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn build_authorize_url(code_challenge: &str) -> Result<String, AuthError> {
    Ok(Url::parse_with_params(
        YANDEX_AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", YANDEX_CLIENT_ID),
            ("code_challenge", code_challenge),
            ("code_challenge_method", "S256"),
        ],
    )?
    .into())
}

pub struct YandexOAuthClient {
    client: Client,
    client_id: String,
}

impl YandexOAuthClient {
    pub fn new(client_id: impl Into<String>) -> Result<Self, AuthError> {
        Ok(Self {
            client: Client::builder().user_agent(USER_AGENT).build()?,
            client_id: client_id.into(),
        })
    }

    fn credentials_from_response(response: TokenResponse) -> Result<StoredCredentials, AuthError> {
        let access_token = response.access_token.ok_or_else(|| {
            AuthError::InvalidTokenResponse("missing access_token in token response".to_owned())
        })?;
        let refresh_token = response.refresh_token.ok_or_else(|| {
            AuthError::InvalidTokenResponse("missing refresh_token in token response".to_owned())
        })?;

        Ok(StoredCredentials {
            access_token,
            refresh_token,
            expires_at_epoch_seconds: response
                .expires_in
                .map(|expires_in| Utc::now().timestamp() + i64::from(expires_in)),
        })
    }

    fn send_token_request(&self, params: &[(&str, &str)]) -> Result<StoredCredentials, AuthError> {
        let response = self.client.post(YANDEX_TOKEN_URL).form(params).send()?;
        let status = response.status();
        if !status.is_success() {
            return Err(error_from_response(response)?);
        }

        let token_response: TokenResponse = response.json()?;
        Self::credentials_from_response(token_response)
    }
}

impl OAuthClient for YandexOAuthClient {
    fn exchange_authorization_code(
        &self,
        code: &str,
        code_verifier: &str,
    ) -> Result<StoredCredentials, AuthError> {
        self.send_token_request(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", &self.client_id),
            ("code_verifier", code_verifier),
        ])
    }

    fn refresh_access_token(&self, refresh_token: &str) -> Result<StoredCredentials, AuthError> {
        self.send_token_request(&[
            ("grant_type", "refresh_token"),
            ("client_id", &self.client_id),
            ("refresh_token", refresh_token),
        ])
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u32>,
}

fn error_from_response(response: Response) -> Result<AuthError, AuthError> {
    let status = response.status();
    let body = response
        .text()
        .unwrap_or_else(|_| String::from("<unable to read body>"));
    Err(AuthError::TokenEndpoint { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeStore {
        saved: Mutex<Option<StoredCredentials>>,
        initial: Mutex<Option<StoredCredentials>>,
    }

    impl FakeStore {
        fn with_initial(credentials: StoredCredentials) -> Self {
            Self {
                saved: Mutex::new(None),
                initial: Mutex::new(Some(credentials)),
            }
        }
    }

    impl CredentialStore for FakeStore {
        fn load_credentials(&self) -> Result<Option<StoredCredentials>, AuthError> {
            Ok(self.initial.lock().unwrap().clone())
        }

        fn save_credentials(&self, credentials: &StoredCredentials) -> Result<(), AuthError> {
            *self.saved.lock().unwrap() = Some(credentials.clone());
            Ok(())
        }
    }

    struct FakeOAuthClient {
        exchanged: StoredCredentials,
        refreshed: StoredCredentials,
        exchange_calls: Mutex<Vec<(String, String)>>,
        refresh_calls: Mutex<Vec<String>>,
    }

    impl FakeOAuthClient {
        fn new(exchanged: StoredCredentials, refreshed: StoredCredentials) -> Self {
            Self {
                exchanged,
                refreshed,
                exchange_calls: Mutex::new(Vec::new()),
                refresh_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl OAuthClient for FakeOAuthClient {
        fn exchange_authorization_code(
            &self,
            code: &str,
            code_verifier: &str,
        ) -> Result<StoredCredentials, AuthError> {
            self.exchange_calls
                .lock()
                .unwrap()
                .push((code.to_owned(), code_verifier.to_owned()));
            Ok(self.exchanged.clone())
        }

        fn refresh_access_token(
            &self,
            refresh_token: &str,
        ) -> Result<StoredCredentials, AuthError> {
            self.refresh_calls
                .lock()
                .unwrap()
                .push(refresh_token.to_owned());
            Ok(self.refreshed.clone())
        }
    }

    fn credentials(
        access: &str,
        refresh: &str,
        expires_at_epoch_seconds: Option<i64>,
    ) -> StoredCredentials {
        StoredCredentials {
            access_token: access.to_owned(),
            refresh_token: refresh.to_owned(),
            expires_at_epoch_seconds,
        }
    }

    #[test]
    fn derives_rfc_pkce_challenge() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(derive_code_challenge(verifier), expected);
    }

    #[test]
    fn generated_verifier_is_base64url_without_padding() {
        let verifier = generate_code_verifier().unwrap();
        assert_eq!(verifier.len(), 43);
        assert!(!verifier.contains('='));
        assert!(
            verifier
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        );
    }

    #[test]
    fn begin_login_rejects_concurrent_attempts() {
        let oauth = Arc::new(FakeOAuthClient::new(
            credentials("access", "refresh", None),
            credentials("access2", "refresh2", None),
        ));
        let store = Arc::new(FakeStore::default());
        let auth = AuthManager::new(oauth, store).unwrap();

        let first = auth.begin_login().unwrap();
        assert_eq!(first.redirect_uri, YANDEX_REDIRECT_URI);
        assert!(matches!(
            auth.begin_login(),
            Err(AuthError::LoginAlreadyPending)
        ));
    }

    #[test]
    fn callback_success_persists_credentials_and_sets_auth_state() {
        let exchanged = credentials("access", "refresh", Some(Utc::now().timestamp() + 3600));
        let refreshed = credentials("access2", "refresh2", Some(Utc::now().timestamp() + 7200));
        let oauth = Arc::new(FakeOAuthClient::new(exchanged.clone(), refreshed));
        let store = Arc::new(FakeStore::default());
        let auth = AuthManager::new(oauth.clone(), store.clone()).unwrap();

        let login = auth.begin_login().unwrap();
        auth.complete_login("callback-code").unwrap();

        assert!(auth.is_authenticated());
        assert_eq!(store.saved.lock().unwrap().clone(), Some(exchanged));
        let exchange_calls = oauth.exchange_calls.lock().unwrap();
        assert_eq!(exchange_calls.len(), 1);
        assert_eq!(exchange_calls[0].0, "callback-code");
        assert_eq!(
            derive_code_challenge(&exchange_calls[0].1),
            login.code_challenge
        );
    }

    #[test]
    fn callback_failure_keeps_pending_login_for_retry() {
        struct FailingOAuthClient;
        impl OAuthClient for FailingOAuthClient {
            fn exchange_authorization_code(
                &self,
                _code: &str,
                _code_verifier: &str,
            ) -> Result<StoredCredentials, AuthError> {
                Err(AuthError::InvalidTokenResponse("boom".to_owned()))
            }

            fn refresh_access_token(
                &self,
                _refresh_token: &str,
            ) -> Result<StoredCredentials, AuthError> {
                unreachable!()
            }
        }

        let store = Arc::new(FakeStore::default());
        let auth = AuthManager::new(Arc::new(FailingOAuthClient), store).unwrap();
        auth.begin_login().unwrap();
        assert!(matches!(
            auth.complete_login("callback-code"),
            Err(AuthError::InvalidTokenResponse(_))
        ));
        assert!(matches!(
            auth.begin_login(),
            Err(AuthError::LoginAlreadyPending)
        ));
    }

    #[test]
    fn callback_without_pending_session_fails() {
        let oauth = Arc::new(FakeOAuthClient::new(
            credentials("access", "refresh", None),
            credentials("access2", "refresh2", None),
        ));
        let store = Arc::new(FakeStore::default());
        let auth = AuthManager::new(oauth, store).unwrap();

        assert!(matches!(
            auth.complete_login("callback-code"),
            Err(AuthError::NoPendingLogin)
        ));
    }

    #[test]
    fn expired_access_token_is_refreshed_and_saved() {
        let initial = credentials("expired", "refresh", Some(Utc::now().timestamp() - 1));
        let refreshed = credentials("fresh", "refresh-2", Some(Utc::now().timestamp() + 3600));
        let oauth = Arc::new(FakeOAuthClient::new(
            credentials("unused", "unused", None),
            refreshed.clone(),
        ));
        let store = Arc::new(FakeStore::with_initial(initial));
        let auth = AuthManager::new(oauth.clone(), store.clone()).unwrap();

        assert_eq!(auth.access_token().unwrap(), "fresh");
        assert_eq!(store.saved.lock().unwrap().clone(), Some(refreshed));
        assert_eq!(oauth.refresh_calls.lock().unwrap().as_slice(), &["refresh"]);
    }

    #[test]
    fn unauthenticated_access_returns_clear_error() {
        let oauth = Arc::new(FakeOAuthClient::new(
            credentials("access", "refresh", None),
            credentials("access2", "refresh2", None),
        ));
        let store = Arc::new(FakeStore::default());
        let auth = AuthManager::new(oauth, store).unwrap();

        assert!(matches!(
            auth.access_token(),
            Err(AuthError::AuthenticationRequired)
        ));
    }
}
