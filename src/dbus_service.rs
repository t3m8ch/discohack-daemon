use std::{collections::HashMap, sync::Arc};

use zbus::zvariant::OwnedValue;
use zbus::{Connection, connection::Builder, interface, object_server::InterfaceRef};

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
    fn sync_summary(&self) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        self.sync
            .sync_summary_dbus()
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    #[zbus(property)]
    fn sync_items(&self) -> zbus::fdo::Result<Vec<HashMap<String, OwnedValue>>> {
        self.sync
            .sync_items_dbus()
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    fn begin_login(&self) -> zbus::fdo::Result<BeginLoginResponse> {
        self.auth
            .begin_login()
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    #[zbus(signal)]
    async fn login_completed(
        signal_emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
}

pub async fn build_connection(
    auth: Arc<AuthManager>,
    sync: Arc<SyncService>,
) -> zbus::Result<Connection> {
    Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, DaemonInterface::new(auth, sync))?
        .build()
        .await
}

pub async fn emit_login_completed(connection: &Connection) -> zbus::Result<()> {
    let iface: InterfaceRef<DaemonInterface> =
        connection.object_server().interface(OBJECT_PATH).await?;
    DaemonInterface::login_completed(iface.signal_emitter().to_owned()).await
}

pub async fn emit_sync_summary_changed(connection: &Connection) -> zbus::Result<()> {
    let iface: InterfaceRef<DaemonInterface> =
        connection.object_server().interface(OBJECT_PATH).await?;
    let iface_ref = iface.get().await;
    iface_ref.sync_summary_changed(iface.signal_emitter()).await
}

pub async fn emit_sync_items_changed(connection: &Connection) -> zbus::Result<()> {
    let iface: InterfaceRef<DaemonInterface> =
        connection.object_server().interface(OBJECT_PATH).await?;
    let iface_ref = iface.get().await;
    iface_ref.sync_items_changed(iface.signal_emitter()).await
}
