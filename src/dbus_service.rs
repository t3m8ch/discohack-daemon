use std::{collections::HashMap, sync::Arc, time::Duration};

use zbus::{Connection, connection::Builder, interface, object_server::InterfaceRef};
use zbus::zvariant::OwnedValue;

use crate::auth::{AuthManager, BeginLoginResponse};
use crate::sync::SyncService;

pub const BUS_NAME: &str = "ru.literallycats.daemon";
pub const OBJECT_PATH: &str = "/ru/literallycats/daemon";
pub const INTERFACE_NAME: &str = "ru.literallycats.daemon";

pub struct DaemonInterface {
    auth: Arc<AuthManager>,
    sync: Arc<SyncService>,
}

impl DaemonInterface {
    pub fn new(auth: Arc<AuthManager>, sync: Arc<SyncService>) -> Self {
        Self { auth, sync }
    }
}

#[interface(
    name = "ru.literallycats.daemon",
    proxy(
        default_service = "ru.literallycats.daemon",
        default_path = "/ru/literallycats/daemon"
    )
)]
impl DaemonInterface {
    #[zbus(property)]
    fn is_auth(&self) -> bool {
        self.auth.is_authenticated()
    }

    #[zbus(property)]
    fn mount_point(&self) -> String {
        self.sync.mountpoint().display().to_string()
    }

    #[zbus(property)]
    fn sync_summary(&self) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        self.sync
            .sync_summary_dict()
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    #[zbus(property)]
    fn sync_items(&self) -> zbus::fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        self.sync
            .sync_items_dict()
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    fn begin_login(&self) -> zbus::fdo::Result<BeginLoginResponse> {
        self.auth
            .begin_login()
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    fn get_sync_status(&self, path: &str) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        self.sync
            .status_dict(path)
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    fn list_directory_statuses(
        &self,
        path: &str,
    ) -> zbus::fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        self.sync
            .directory_statuses_dict(path)
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    fn request_refresh(&self, path: &str) -> zbus::fdo::Result<()> {
        self.sync
            .request_refresh(path)
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    #[zbus(signal)]
    async fn login_completed(
        signal_emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
}

pub async fn build_connection(auth: Arc<AuthManager>, sync: Arc<SyncService>) -> zbus::Result<Connection> {
    let connection = Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, DaemonInterface::new(auth, sync.clone()))?
        .build()
        .await?;

    spawn_sync_property_updates(connection.clone(), sync);
    Ok(connection)
}

pub async fn emit_login_completed(connection: &Connection) -> zbus::Result<()> {
    let iface: InterfaceRef<DaemonInterface> =
        connection.object_server().interface(OBJECT_PATH).await?;
    DaemonInterface::login_completed(iface.signal_emitter().to_owned()).await
}

fn spawn_sync_property_updates(connection: Connection, sync: Arc<SyncService>) {
    tokio::spawn(async move {
        let mut last_seen = sync.status_version();
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let current = sync.status_version();
            if current == last_seen {
                continue;
            }
            last_seen = current;

            let iface: InterfaceRef<DaemonInterface> = match connection.object_server().interface(OBJECT_PATH).await {
                Ok(iface) => iface,
                Err(err) => {
                    tracing::warn!(error = %err, "failed to access D-Bus interface for sync property update");
                    continue;
                }
            };

            let iface_guard = iface.get().await;
            if let Err(err) = iface_guard.sync_summary_changed(iface.signal_emitter()).await {
                tracing::warn!(error = %err, "failed to emit SyncSummary property update");
            }
            if let Err(err) = iface_guard.sync_items_changed(iface.signal_emitter()).await {
                tracing::warn!(error = %err, "failed to emit SyncItems property update");
            }
        }
    });
}
