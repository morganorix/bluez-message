//! Client MAP via obexd (bus session) : lecture, envoi, polling.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use futures_util::Stream;
use serde::{Deserialize, Serialize};
use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::addr::BtAddr;
use crate::bmsg;
use crate::bluez::{prop_bool, prop_str};
use crate::error::{Error, Result};
use crate::obex::Session;
use crate::phone::clean;
use crate::t;

#[proxy(interface = "org.bluez.obex.MessageAccess1", default_service = "org.bluez.obex")]
trait MessageAccess {
    fn set_folder(&self, name: &str) -> zbus::Result<()>;
    fn list_messages(
        &self,
        folder: &str,
        filter: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<HashMap<OwnedObjectPath, HashMap<String, OwnedValue>>>;
    fn push_message(
        &self,
        file: &str,
        folder: &str,
        args: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<(OwnedObjectPath, HashMap<String, OwnedValue>)>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sms {
    pub handle: String,
    pub sender: String,
    pub address: String,
    pub timestamp: String,
    pub read: bool,
    pub text: String,
    pub kind: String,
    /// Destinataire (messages envoyés).
    pub recipient: String,
    pub recipient_address: String,
}

/// Propriété texte nettoyée ([`clean`]) : expéditeur et texte sont contrôlés par l'émetteur du SMS.
fn str_prop(p: &HashMap<String, OwnedValue>, k: &str, multiline: bool) -> String {
    clean(prop_str(p, k).unwrap_or_default(), multiline)
}

fn sms_from(path: &OwnedObjectPath, p: &HashMap<String, OwnedValue>) -> Sms {
    Sms {
        handle: path.as_str().rsplit('/').next().unwrap_or_default().to_owned(),
        sender: str_prop(p, "Sender", false),
        address: str_prop(p, "SenderAddress", false),
        timestamp: str_prop(p, "Timestamp", false),
        read: prop_bool(p, "Read"),
        text: str_prop(p, "Subject", true),
        kind: str_prop(p, "Type", false),
        recipient: str_prop(p, "Recipient", false),
        recipient_address: str_prop(p, "RecipientAddress", false),
    }
}

/// Dossiers MAP autorisés (liste blanche : jamais de chemin libre vers `SetFolder`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Folder {
    Inbox,
    Sent,
    Outbox,
    Deleted,
}

impl Folder {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Sent => "sent",
            Self::Outbox => "outbox",
            Self::Deleted => "deleted",
        }
    }

    /// Chemin passé à `SetFolder`.
    #[must_use]
    pub fn path(self) -> String {
        format!("/telecom/msg/{}", self.as_str())
    }

    /// Dossier des messages envoyés depuis le téléphone.
    #[must_use]
    pub fn is_sent(self) -> bool {
        self == Self::Sent
    }
}

impl std::str::FromStr for Folder {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self> {
        match name {
            "inbox" => Ok(Self::Inbox),
            "sent" => Ok(Self::Sent),
            "outbox" => Ok(Self::Outbox),
            "deleted" => Ok(Self::Deleted),
            _ => Err(Error::InvalidFolder(name.to_owned())),
        }
    }
}

/// Messages jamais vus, ordonnés par horodatage (format MAP `YYYYMMDDTHHMMSS`).
pub fn new_messages(seen: &mut HashSet<String>, mut all: Vec<Sms>) -> Vec<Sms> {
    all.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    all.into_iter().filter(|m| seen.insert(m.handle.clone())).collect()
}

pub struct MapClient {
    session: Session,
}

impl MapClient {
    /// Ouvre une session MAP vers `addr`.
    ///
    /// # Errors
    /// obexd injoignable ou session refusée.
    pub async fn connect(addr: &BtAddr) -> Result<Self> {
        Ok(Self { session: Session::open(addr, "map").await? })
    }

    async fn access(&self) -> Result<MessageAccessProxy<'_>> {
        Ok(MessageAccessProxy::builder(&self.session.conn)
            .path(self.session.path.as_ref())?
            .build()
            .await?)
    }

    /// Messages du dossier `folder`.
    ///
    /// # Errors
    /// Erreur D-Bus ou d'obexd.
    pub async fn list(&self, folder: Folder) -> Result<Vec<Sms>> {
        let ma = self.access().await?;
        ma.set_folder(&folder.path()).await?;
        let raw = ma.list_messages("", HashMap::new()).await?;
        let mut v: Vec<Sms> = raw.iter().map(|(p, props)| sms_from(p, props)).collect();
        v.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        Ok(v)
    }

    /// Envoie un SMS et attend la fin du transfert.
    ///
    /// # Errors
    /// Numéro invalide, fichier temporaire impossible, ou transfert refusé, en échec ou trop long.
    pub async fn send(&self, to: &str, text: &str) -> Result<()> {
        let bmsg = bmsg::encode_sms(to, text)?;
        let mut file = tempfile::NamedTempFile::new()?; // 0600
        std::io::Write::write_all(&mut file, bmsg.as_bytes())?;
        let path = file.path().to_str().ok_or_else(|| Error::Io(std::io::Error::other(t!("non UTF-8 path"))))?.to_owned();

        let ma = self.access().await?;
        ma.set_folder(&Folder::Outbox.path()).await?;
        // Piège : "UTF-8"/"utf-8" => InvalidArguments ; seul "utf8" est accepté.
        let args = HashMap::from([("Charset", Value::from("utf8"))]);
        let (tpath, _) = ma.push_message(&path, "", args).await?;
        self.session.wait_transfer(tpath, Duration::from_secs(15)).await
    }

    /// Polling de l'inbox ; les messages déjà présents au démarrage ne sont pas émis.
    pub fn watch(&self, interval: Duration) -> impl Stream<Item = Result<Sms>> + '_ {
        futures_util::stream::unfold(
            (None::<HashSet<String>>, Vec::<Sms>::new().into_iter()),
            move |(mut seen, mut pending)| async move {
                loop {
                    if let Some(m) = pending.next() {
                        return Some((Ok(m), (seen, pending)));
                    }
                    match self.list(Folder::Inbox).await {
                        Err(e) => return Some((Err(e), (seen, pending))),
                        Ok(all) => match seen.as_mut() {
                            None => seen = Some(all.into_iter().map(|m| m.handle).collect()),
                            Some(s) => pending = new_messages(s, all).into_iter(),
                        },
                    }
                    if pending.len() == 0 {
                        tokio::time::sleep(interval).await;
                    }
                }
            },
        )
    }

    /// Ferme la session MAP.
    ///
    /// # Errors
    /// Erreur D-Bus ou d'obexd.
    pub async fn close(self) -> Result<()> {
        self.session.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sms(h: &str, ts: &str) -> Sms {
        Sms { handle: h.into(), timestamp: ts.into(), ..Default::default() }
    }

    #[test]
    fn diff_emits_only_new_in_order() {
        let mut seen: HashSet<String> = ["1".to_owned()].into();
        let out = new_messages(&mut seen, vec![sms("3", "20261015T120000"), sms("1", "x"), sms("2", "20261015T110000")]);
        assert_eq!(out.iter().map(|m| m.handle.as_str()).collect::<Vec<_>>(), ["2", "3"]);
        assert!(new_messages(&mut seen, vec![sms("2", "a")]).is_empty());
    }

    #[test]
    fn folder_whitelist() {
        assert_eq!("inbox".parse::<Folder>().unwrap().path(), "/telecom/msg/inbox");
        assert!("../x".parse::<Folder>().is_err());
        for f in [Folder::Inbox, Folder::Sent, Folder::Outbox, Folder::Deleted] {
            assert_eq!(f.as_str().parse::<Folder>().unwrap(), f);
            assert_eq!(serde_json::to_string(&f).unwrap(), format!("\"{}\"", f.as_str()));
        }
    }
}
