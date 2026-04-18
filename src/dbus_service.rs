use std::sync::Arc;

use zbus::{Connection, connection::Builder, interface, object_server::InterfaceRef};

use crate::auth::{AuthManager, BeginLoginResponse};

pub const BUS_NAME: &str = "ru.literallycats.daemon";
pub const OBJECT_PATH: &str = "/ru/literallycats/daemon";
pub const INTERFACE_NAME: &str = "ru.literallycats.daemon";

pub struct DaemonInterface {
    auth: Arc<AuthManager>,
}

impl DaemonInterface {
    pub fn new(auth: Arc<AuthManager>) -> Self {
        Self { auth }
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

pub async fn build_connection(auth: Arc<AuthManager>) -> zbus::Result<Connection> {
    Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, DaemonInterface::new(auth))?
        .build()
        .await
}

pub async fn emit_login_completed(connection: &Connection) -> zbus::Result<()> {
    let iface: InterfaceRef<DaemonInterface> =
        connection.object_server().interface(OBJECT_PATH).await?;
    DaemonInterface::login_completed(iface.signal_emitter().to_owned()).await
}
