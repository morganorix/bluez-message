//! Sessions OBEX d'obexd (bus session), communes à MAP et PBAP : ouverture, fermeture, suivi des transferts.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};
use zbus::{Connection, proxy};

use crate::addr::BtAddr;
use crate::error::{Error, Result};

#[proxy(interface = "org.bluez.obex.Client1", default_service = "org.bluez.obex", default_path = "/org/bluez/obex")]
trait Client {
    fn create_session(&self, dest: &str, args: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
    fn remove_session(&self, session: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(interface = "org.bluez.obex.Transfer1", default_service = "org.bluez.obex")]
trait Transfer {
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;
}

/// Session OBEX ouverte vers l'iPhone pour un profil (`map`, `pbap`).
pub struct Session {
    pub(crate) conn: Connection,
    pub(crate) path: OwnedObjectPath,
}

impl Session {
    /// Ouvre une session obexd vers `addr` pour le profil `target`.
    ///
    /// # Errors
    /// Bus session injoignable ou session refusée.
    pub async fn open(addr: &BtAddr, target: &str) -> Result<Self> {
        let conn = Connection::session().await?;
        let client = ClientProxy::new(&conn).await?;
        let path = client.create_session(addr.as_str(), HashMap::from([("Target", Value::from(target))])).await?;
        Ok(Self { conn, path })
    }

    /// Ferme la session.
    ///
    /// # Errors
    /// Erreur D-Bus ou d'obexd.
    pub async fn close(self) -> Result<()> {
        let client = ClientProxy::new(&self.conn).await?;
        client.remove_session(&self.path).await?;
        Ok(())
    }

    /// Attend la fin d'un transfert OBEX (au plus `max`) : succès seulement si `complete` est vu.
    /// Abonné aux changements avant la première lecture, rien n'est manqué même si l'objet disparaît ensuite.
    ///
    /// # Errors
    /// [`Error::TransferFailed`], [`Error::TransferTimeout`] ou erreur D-Bus.
    pub async fn wait_transfer(&self, path: OwnedObjectPath, max: Duration) -> Result<()> {
        let transfer = TransferProxy::builder(&self.conn).path(path)?.build().await?;
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
