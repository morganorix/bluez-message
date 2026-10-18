//! MAP client through obexd (session bus): read, send, polling.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use futures_util::Stream;
use serde::{Deserialize, Serialize};
use zbus::{OwnedObjectPath, OwnedVariant, Variant, proxy};

use crate::addr::BtAddr;
use crate::bmsg;
use crate::bluez::{bounded, prop_bool, prop_str};
use crate::error::{Error, Result};
use crate::obex::{CALL_TIMEOUT, Session};
use crate::phone::clean;
use crate::t;

/// Messages kept per listing, the most recent ones (default `MaxListCount` of the MAP specification).
const LIST_MAX: usize = 1024;

#[proxy(interface = "org.bluez.obex.MessageAccess1", default_service = "org.bluez.obex")]
trait MessageAccess {
    fn set_folder(&self, name: &str) -> zbus::Result<()>;
    fn list_messages(
        &self,
        folder: &str,
        filter: HashMap<&str, Variant<'_>>,
    ) -> zbus::Result<HashMap<OwnedObjectPath, HashMap<String, OwnedVariant>>>;
    fn push_message(
        &self,
        file: &str,
        folder: &str,
        args: HashMap<&str, Variant<'_>>,
    ) -> zbus::Result<(OwnedObjectPath, HashMap<String, OwnedVariant>)>;
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
    /// Recipient (sent messages).
    pub recipient: String,
    pub recipient_address: String,
}

/// Sorted by timestamp, the [`LIST_MAX`] most recent kept (bound if the phone ignores the specification).
fn newest(mut v: Vec<Sms>) -> Vec<Sms> {
    v.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    if v.len() > LIST_MAX {
        tracing::warn!("MAP listing of {} messages: {LIST_MAX} most recent kept", v.len());
        v.drain(..v.len() - LIST_MAX);
    }
    v
}

/// Cleaned text property ([`clean`]): sender and text are controlled by the SMS sender.
fn str_prop(p: &HashMap<String, OwnedVariant>, k: &str, multiline: bool) -> String {
    clean(prop_str(p, k).unwrap_or_default(), multiline)
}

fn sms_from(path: &OwnedObjectPath, p: &HashMap<String, OwnedVariant>) -> Sms {
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

/// Allowed MAP folders (allow-list: never a free path to `SetFolder`).
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

    /// Path passed to `SetFolder`.
    #[must_use]
    pub fn path(self) -> String {
        format!("/telecom/msg/{}", self.as_str())
    }

    /// Folder of the messages sent from the phone.
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

/// Never-seen messages, ordered by timestamp (MAP format `YYYYMMDDTHHMMSS`). Handles absent from
/// the listing are forgotten: `seen` stays bounded by the folder size. An empty listing (transient
/// phone answer) keeps `seen`, otherwise the next full listing would replay the whole inbox.
pub fn new_messages(seen: &mut HashSet<String>, mut all: Vec<Sms>) -> Vec<Sms> {
    all.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    if !all.is_empty() {
        let listed: HashSet<&str> = all.iter().map(|m| m.handle.as_str()).collect();
        seen.retain(|h| listed.contains(h.as_str()));
    }
    all.into_iter().filter(|m| seen.insert(m.handle.clone())).collect()
}

pub struct MapClient {
    session: Session,
}

impl MapClient {
    /// Opens a MAP session to `addr`.
    ///
    /// # Errors
    /// obexd unreachable or session refused.
    pub async fn connect(addr: &BtAddr) -> Result<Self> {
        Ok(Self { session: Session::open(addr, "map").await? })
    }

    async fn access(&self) -> Result<MessageAccessProxy<'_>> {
        Ok(MessageAccessProxy::builder(&self.session.conn)
            .path(self.session.path.as_ref())
            .build()
            .await?)
    }

    /// Messages of folder `folder`, oldest first, [`LIST_MAX`] most recent at most.
    ///
    /// # Errors
    /// D-Bus or obexd error, or timeout.
    pub async fn list(&self, folder: Folder) -> Result<Vec<Sms>> {
        let raw = bounded(CALL_TIMEOUT, async {
            let ma = self.access().await?;
            ma.set_folder(&folder.path()).await?;
            // MAP lists the most recent first: the phone truncates, `newest` still bounds a phone ignoring it.
            let max = u16::try_from(LIST_MAX).unwrap_or(u16::MAX);
            Ok::<_, Error>(ma.list_messages("", HashMap::from([("MaxCount", Variant::from(max))])).await?)
        })
        .await?;
        Ok(newest(raw.iter().map(|(p, props)| sms_from(p, props)).collect()))
    }

    /// Sends an SMS and waits for the end of the transfer.
    ///
    /// # Errors
    /// Invalid number, temporary file impossible, or transfer refused, failed or too long.
    /// After a timeout the phone may still have sent the message: never resend automatically.
    pub async fn send(&self, to: &str, text: &str) -> Result<()> {
        let bmsg = bmsg::encode_sms(to, text)?;
        let mut file = tempfile::NamedTempFile::new()?; // 0600
        std::io::Write::write_all(&mut file, bmsg.as_bytes())?;
        let path = file.path().to_str().ok_or_else(|| Error::Io(std::io::Error::other(t!("non UTF-8 path"))))?.to_owned();

        let (tpath, _) = bounded(CALL_TIMEOUT, async {
            let ma = self.access().await?;
            ma.set_folder(&Folder::Outbox.path()).await?;
            // Pitfall: "UTF-8"/"utf-8" => InvalidArguments; only "utf8" is accepted.
            let args = HashMap::from([("Charset", Variant::from("utf8"))]);
            Ok::<_, Error>(ma.push_message(&path, "", args).await?)
        })
        .await?;
        self.session.wait_transfer(tpath, Duration::from_secs(15)).await
    }

    /// Inbox polling; messages already present at startup are not emitted.
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

    /// Closes the MAP session.
    ///
    /// # Errors
    /// D-Bus or obexd error.
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
        assert_eq!(seen, HashSet::from(["2".to_owned()]));
        // Transient empty listing: nothing forgotten, nothing replayed afterwards.
        assert!(new_messages(&mut seen, vec![]).is_empty());
        assert!(new_messages(&mut seen, vec![sms("2", "a")]).is_empty());
    }

    #[test]
    fn listing_keeps_the_most_recent() {
        let v: Vec<Sms> = (0..LIST_MAX + 3).rev().map(|i| sms(&i.to_string(), &format!("{i:08}"))).collect();
        let kept = newest(v);
        assert_eq!(kept.len(), LIST_MAX);
        assert_eq!(kept.first().map(|m| m.handle.as_str()), Some("3"));
        assert_eq!(kept.last().map(|m| m.handle.clone()), Some((LIST_MAX + 2).to_string()));
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
