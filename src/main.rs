mod auth;
mod callback;
mod dbus_service;
mod fs;
mod mount;
mod secrets;
mod sync;
mod yadisk;

use std::{env, path::PathBuf, process, sync::Arc};

use auth::{AuthManager, YANDEX_CLIENT_ID, YandexOAuthClient};
use callback::{CallbackEvent, spawn_callback_server};
use dbus_service::{build_connection, emit_login_completed};
use dotenvy::dotenv;
use mount::MountManager;
use secrets::SecretServiceStore;
use sync::SyncService;
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::mpsc,
    task,
};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use yadisk::{AccessTokenProvider, YandexDiskClient};

fn usage() -> &'static str {
    "usage: discohack-daemon <mountpoint>"
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .finish();

    let _ = tracing::subscriber::set_global_default(subscriber);
}

fn mountpoint_from_args() -> Result<PathBuf, String> {
    env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| usage().to_owned())
}

async fn ensure_mount_started(mount_manager: Arc<MountManager>) {
    let mountpoint = mount_manager.mountpoint().to_path_buf();
    match task::spawn_blocking(move || mount_manager.ensure_mounted()).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            warn!(mountpoint = %mountpoint.display(), error = %err, "failed to start mount")
        }
        Err(err) => warn!(mountpoint = %mountpoint.display(), error = %err, "mount task failed"),
    }
}

async fn shutdown_mount(mount_manager: Arc<MountManager>, sync_service: Arc<SyncService>) -> i32 {
    let mountpoint = mount_manager.mountpoint().to_path_buf();
    match task::spawn_blocking(move || mount_manager.shutdown()).await {
        Ok(Ok(())) => {
            sync_service.shutdown();
            0
        }
        Ok(Err(err)) => {
            sync_service.shutdown();
            error!(mountpoint = %mountpoint.display(), error = %err, "mount shutdown failed");
            1
        }
        Err(err) => {
            sync_service.shutdown();
            error!(mountpoint = %mountpoint.display(), error = %err, "mount shutdown task failed");
            1
        }
    }
}

async fn run() -> i32 {
    let mountpoint = match mountpoint_from_args() {
        Ok(path) => path,
        Err(message) => {
            error!(usage = usage(), error = %message, "invalid command line arguments");
            return 2;
        }
    };

    info!(mountpoint = %mountpoint.display(), "starting discohack-daemon");

    let auth = match task::spawn_blocking(move || {
        let oauth_client = Arc::new(YandexOAuthClient::new(YANDEX_CLIENT_ID)?);
        let store = Arc::new(SecretServiceStore);
        AuthManager::new(oauth_client, store)
    })
    .await
    {
        Ok(Ok(auth)) => Arc::new(auth),
        Ok(Err(err)) => {
            error!(error = %err, "failed to initialize auth state");
            return 1;
        }
        Err(err) => {
            error!(error = %err, "auth initialization task failed");
            return 1;
        }
    };

    let token_provider: Arc<dyn AccessTokenProvider> = auth.clone();
    let client = match task::spawn_blocking(move || YandexDiskClient::new(token_provider)).await {
        Ok(Ok(client)) => client,
        Ok(Err(err)) => {
            error!(error = %err, "failed to initialize Yandex Disk client");
            return 1;
        }
        Err(err) => {
            error!(error = %err, "yandex client initialization task failed");
            return 1;
        }
    };

    let client = Arc::new(client);
    let sync_service = match SyncService::open_default(client.clone(), mountpoint.clone()) {
        Ok(service) => service,
        Err(err) => {
            error!(error = %err, "failed to initialize sync state");
            return 1;
        }
    };

    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let mount_manager = Arc::new(MountManager::new(
        mountpoint.clone(),
        sync_service.clone(),
        uid,
        gid,
    ));

    let (callback_tx, mut callback_rx) = mpsc::channel(8);
    let callback_handle = match spawn_callback_server(auth.clone(), callback_tx).await {
        Ok(handle) => handle,
        Err(err) => {
            error!(error = %err, "failed to start OAuth callback listener");
            return 1;
        }
    };

    let connection = match build_connection(auth.clone(), sync_service.clone()).await {
        Ok(connection) => connection,
        Err(err) => {
            error!(error = %err, "failed to start D-Bus service");
            callback_handle.abort();
            return 1;
        }
    };

    if auth.is_authenticated() {
        sync_service.start_background();
        ensure_mount_started(Arc::clone(&mount_manager)).await;
    }

    info!(service = dbus_service::BUS_NAME, mountpoint = %mountpoint.display(), authenticated = auth.is_authenticated(), "service ready");

    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(signal) => signal,
        Err(err) => {
            error!(error = %err, "failed to install SIGINT handler");
            callback_handle.abort();
            return shutdown_mount(mount_manager, sync_service).await;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(err) => {
            error!(error = %err, "failed to install SIGTERM handler");
            callback_handle.abort();
            return shutdown_mount(mount_manager, sync_service).await;
        }
    };

    loop {
        tokio::select! {
            _ = sigint.recv() => {
                info!(signal = "SIGINT", "shutdown requested");
                callback_handle.abort();
                return shutdown_mount(mount_manager, sync_service).await;
            }
            _ = sigterm.recv() => {
                info!(signal = "SIGTERM", "shutdown requested");
                callback_handle.abort();
                return shutdown_mount(mount_manager, sync_service).await;
            }
            maybe_event = callback_rx.recv() => {
                let Some(event) = maybe_event else {
                    warn!("callback event channel closed unexpectedly");
                    callback_handle.abort();
                    return shutdown_mount(mount_manager, sync_service).await;
                };

                match event {
                    CallbackEvent::LoginCompleted => {
                        sync_service.start_background();
                        ensure_mount_started(Arc::clone(&mount_manager)).await;
                        if let Err(err) = emit_login_completed(&connection).await {
                            warn!(error = %err, "failed to emit LoginCompleted signal");
                        }
                    }
                }
            }
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    dotenv().ok();
    init_tracing();
    process::exit(run().await);
}
