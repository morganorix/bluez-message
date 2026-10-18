//! obexd OBEX sessions (session bus), shared by MAP and PBAP: opening, closing, transfer tracking.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use zbus::{Connection, ObjectPath, OwnedObjectPath, Variant, proxy};

use crate::addr::BtAddr;
use crate::error::{Error, Result};

#[proxy(interface = "org.bluez.obex.Client1", default_service = "org.bluez.obex", default_path = "/org/bluez/obex")]
trait Client {
    fn create_session(&self, dest: &str, args: HashMap<&str, Variant<'_>>) -> zbus::Result<OwnedObjectPath>;
    fn remove_session(&self, session: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(interface = "org.bluez.obex.Transfer1", default_service = "org.bluez.obex")]
trait Transfer {
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;
}

/// OBEX session opened to the iPhone for a profile (`map`, `pbap`).
pub struct Session {
    pub(crate) conn: Connection,
    pub(crate) path: OwnedObjectPath,
}

impl Session {
    /// Opens an obexd session to `addr` for profile `target`.
    ///
    /// # Errors
    /// Session bus unreachable or session refused.
    pub async fn open(addr: &BtAddr, target: &str) -> Result<Self> {
        let conn = Connection::session().await?;
        let client = ClientProxy::new(&conn).await?;
        let path = client.create_session(addr.as_str(), HashMap::from([("Target", Variant::from(target))])).await?;
        Ok(Self { conn, path })
    }

    /// Closes the session.
    ///
    /// # Errors
    /// D-Bus or obexd error.
    pub async fn close(self) -> Result<()> {
        let client = ClientProxy::new(&self.conn).await?;
        client.remove_session(&self.path).await?;
        Ok(())
    }

    /// Waits for the end of an OBEX transfer (`max` at most): success only if `complete` is seen.
    /// Subscribed to changes before the first read, nothing is missed even if the object disappears afterwards.
    ///
    /// # Errors
    /// [`Error::TransferFailed`], [`Error::TransferTimeout`] or D-Bus error.
    pub async fn wait_transfer(&self, path: OwnedObjectPath, max: Duration) -> Result<()> {
        let transfer = TransferProxy::builder(&self.conn).path(path).build().await?;
        let mut changes = transfer.receive_status_changed().await;
        let done = async {
            let mut status = transfer.status().await.map_err(|_| Error::TransferFailed)?;
            loop {
                match status.as_str() {
                    "complete" => return Ok(()),
                    "error" => return Err(Error::TransferFailed),
                    _ => {}
                }
                let Some(c) = changes.next().await else { return Err(Error::TransferFailed) };
                status = c.get().await.map_err(|_| Error::TransferFailed)?;
            }
        };
        tokio::time::timeout(max, done).await.map_err(|_| Error::TransferTimeout)?
    }
}
