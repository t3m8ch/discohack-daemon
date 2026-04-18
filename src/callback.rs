use std::sync::Arc;

use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::get,
};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::auth::AuthManager;

pub const CALLBACK_BIND_ADDR: &str = "127.0.0.1:6532";
pub const CALLBACK_PATH: &str = "/oauth/yandex-disk";

#[derive(Debug, Clone)]
pub enum CallbackEvent {
    LoginCompleted,
}

#[derive(Clone)]
struct CallbackState {
    auth: Arc<AuthManager>,
    events: mpsc::Sender<CallbackEvent>,
}

#[derive(Debug, Deserialize)]
struct OAuthCallbackQuery {
    code: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub async fn spawn_callback_server(
    auth: Arc<AuthManager>,
    events: mpsc::Sender<CallbackEvent>,
) -> Result<tokio::task::JoinHandle<()>, std::io::Error> {
    let state = CallbackState { auth, events };
    let app = Router::new()
        .route(CALLBACK_PATH, get(handle_oauth_callback))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(CALLBACK_BIND_ADDR).await?;
    let local_addr = listener.local_addr()?;
    tracing::info!(address = %local_addr, "oauth callback listener ready");

    Ok(tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!(error = %err, "oauth callback server exited unexpectedly");
        }
    }))
}

async fn handle_oauth_callback(
    State(state): State<CallbackState>,
    Query(query): Query<OAuthCallbackQuery>,
) -> impl IntoResponse {
    if let Some(error) = query.error {
        let description = query.error_description.unwrap_or_default();
        return (
            StatusCode::BAD_REQUEST,
            Html(format!("OAuth failed: {error}. {description}")),
        )
            .into_response();
    }

    let Some(code) = query.code else {
        return (
            StatusCode::BAD_REQUEST,
            Html("Missing authorization code".to_owned()),
        )
            .into_response();
    };

    let auth = Arc::clone(&state.auth);
    let result = tokio::task::spawn_blocking(move || auth.complete_login(&code)).await;

    match result {
        Ok(Ok(())) => {
            if state
                .events
                .send(CallbackEvent::LoginCompleted)
                .await
                .is_err()
            {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Html("Login completed, but backend event dispatch failed".to_owned()),
                )
                    .into_response();
            }

            (
                StatusCode::OK,
                Html(
                    "Yandex Disk login completed successfully. You can close this window."
                        .to_owned(),
                ),
            )
                .into_response()
        }
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("OAuth callback handling failed: {err}")),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("OAuth callback task failed: {err}")),
        )
            .into_response(),
    }
}
