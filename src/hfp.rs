//! Unité mains libres (HFP, rôle HF) via BlueZ `ProfileManager1` : signalisation seulement, pas d'audio SCO.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};

use serde::{Deserialize, Serialize};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{Connection, interface, proxy};

use crate::addr::BtAddr;
use crate::error::{Error, Result};
use crate::phone::clean;
use crate::t;

const UUID_HFP_HF: &str = "0000111e-0000-1000-8000-00805f9b34fb";
const PROFILE_PATH: &str = "/org/bluez_message/hfp";

/// Séquence d'initialisation SLC ; chaque commande est envoyée après le `OK` de la précédente.
const SLC: [&str; 5] = ["AT+BRSF=4", "AT+CIND=?", "AT+CIND?", "AT+CMER=3,0,0,1", "AT+CLIP=1"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HfEvent {
    Connected,
    Disconnected,
    SlcReady,
    Ring,
    Clip { number: String },
    Indicator { name: Indicator, value: u8 },
    IncomingCall,
    /// Appels secondaires (Telephony seulement) : `hold` si l'appel principal est lui-même en attente ;
    /// `held` un autre appel mis en attente, `waiting` un second appel qui sonne (numéro vide si masqué).
    Lines { hold: bool, held: Option<String>, waiting: Option<String> },
    Error,
}

/// Indicateur `+CIND`/`+CIEV` ; sérialisé sous son nom HFP (`call`, `callsetup`…), nom inconnu gardé tel quel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Indicator {
    Service,
    Call,
    CallSetup,
    CallHeld,
    Signal,
    Roam,
    BattChg,
    Other(String),
}

impl Indicator {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Service => "service",
            Self::Call => "call",
            Self::CallSetup => "callsetup",
            Self::CallHeld => "callheld",
            Self::Signal => "signal",
            Self::Roam => "roam",
            Self::BattChg => "battchg",
            Self::Other(s) => s,
        }
    }
}

impl From<&str> for Indicator {
    fn from(s: &str) -> Self {
        match s {
            "service" => Self::Service,
            "call" => Self::Call,
            "callsetup" => Self::CallSetup,
            "callheld" => Self::CallHeld,
            "signal" => Self::Signal,
            "roam" => Self::Roam,
            "battchg" => Self::BattChg,
            _ => Self::Other(s.to_owned()),
        }
    }
}

impl From<String> for Indicator {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

impl From<Indicator> for String {
    fn from(i: Indicator) -> Self {
        match i {
            Indicator::Other(s) => s,
            i => i.as_str().to_owned(),
        }
    }
}

/// Machine à états du protocole AT, sans E/S (testable).
#[derive(Default)]
pub struct Slc {
    sent: usize,
    ready: bool,
    indicators: Vec<Indicator>,
}

impl Slc {
    pub fn start(&mut self) -> Vec<String> {
        self.sent = 1;
        vec![SLC[0].to_owned()]
    }

    /// Retourne (commandes à émettre, événements).
    pub fn on_line(&mut self, line: &str) -> (Vec<String>, Vec<HfEvent>) {
        let (mut cmds, mut evs) = (vec![], vec![]);
        match line {
            "OK" => {
                if self.sent < SLC.len() {
                    cmds.push(SLC[self.sent].to_owned());
                    self.sent += 1;
                } else if !self.ready {
                    self.ready = true;
                    evs.push(HfEvent::SlcReady);
                }
            }
            "ERROR" => evs.push(HfEvent::Error),
            "RING" => evs.push(HfEvent::Ring),
            _ => {
                if let Some(rest) = line.strip_prefix("+CIND: (") {
                    self.indicators = parse_indicator_names(rest);
                } else if let Some(rest) = line.strip_prefix("+CIEV:") {
                    if let Some(ev) = self.parse_ciev(rest) {
                        if matches!(&ev, HfEvent::Indicator { name: Indicator::CallSetup, value: 1 }) {
                            evs.push(HfEvent::IncomingCall);
                        }
                        evs.push(ev);
                    }
                } else if let Some(rest) = line.strip_prefix("+CLIP:") {
                    let number = clean(rest.split('"').nth(1).unwrap_or_default(), false);
                    evs.push(HfEvent::Clip { number });
                }
            }
        }
        (cmds, evs)
    }

    fn parse_ciev(&self, rest: &str) -> Option<HfEvent> {
        let (i, v) = rest.trim().split_once(',')?;
        let idx: usize = i.trim().parse().ok()?;
        let value: u8 = v.trim().parse().ok()?;
        let name = self.indicators.get(idx.checked_sub(1)?)?.clone();
        Some(HfEvent::Indicator { name, value })
    }
}

/// `"service",(0,1)),("call",(0,1))...` → noms dans l'ordre (index CIEV = position + 1).
fn parse_indicator_names(s: &str) -> Vec<Indicator> {
    s.split('"').skip(1).step_by(2).map(Indicator::from).collect()
}

#[must_use]
pub fn mask(number: &str) -> String {
    let tail: String = number.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("****{tail}")
}

struct Agent {
    device_suffix: String,
    tx: mpsc::Sender<OwnedFd>,
}

#[interface(name = "org.bluez.Profile1")]
impl Agent {
    async fn new_connection(&self, device: ObjectPath<'_>, fd: zbus::zvariant::OwnedFd, _props: HashMap<String, OwnedValue>) {
        if device.as_str().ends_with(&self.device_suffix) {
            let _ = self.tx.send(fd.into()).await;
        } // sinon : fd refermé au drop, appareil étranger ignoré
    }

    async fn request_disconnection(&self, _device: ObjectPath<'_>) {}

    async fn release(&self) {}
}

#[proxy(
    interface = "org.bluez.ProfileManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez"
)]
trait ProfileManager {
    fn register_profile(&self, profile: &ObjectPath<'_>, uuid: &str, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn unregister_profile(&self, profile: &ObjectPath<'_>) -> zbus::Result<()>;
}

struct Rfcomm(AsyncFd<OwnedFd>);

impl Rfcomm {
    fn new(fd: OwnedFd) -> Result<Self> {
        // SAFETY: fcntl sur un fd valide dont nous sommes propriétaires.
        let rc = unsafe {
            let fl = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK)
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: OwnedFd garde le fd ouvert et stable jusqu'au drop de l'AsyncFd.
        let afd = unsafe { AsyncFd::register(fd) }.map_err(|e| e.into_parts().1)?;
        Ok(Self(afd))
    }

    /// `Ok(None)` = fermeture par le pair.
    async fn read(&self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        loop {
            let mut g = self.0.readable().await?;
            // SAFETY: buf valide pour buf.len() octets.
            let r = g.try_io(|fd| {
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                usize::try_from(n).map_err(|_| std::io::Error::last_os_error())
            });
            match r {
                Ok(Ok(0)) => return Ok(None),
                Ok(Ok(n)) => return Ok(Some(n)),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => continue,
            }
        }
    }

    async fn write_cmd(&self, cmd: &str) -> std::io::Result<()> {
        let data = format!("{cmd}\r");
        let mut off = 0;
        while off < data.len() {
            let mut g = self.0.writable().await?;
            let r = g.try_io(|fd| {
                // SAFETY: data[off..] valide.
                let n = unsafe { libc::write(fd.as_raw_fd(), data[off..].as_ptr().cast(), data.len() - off) };
                usize::try_from(n).map_err(|_| std::io::Error::last_os_error())
            });
            match r {
                Ok(Ok(n)) => off += n,
                Ok(Err(e)) => return Err(e),
                Err(_) => continue,
            }
        }
        tracing::debug!(cmd, "TX");
        Ok(())
    }
}

/// Commandes d'appel acceptées pendant une session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfCmd {
    Answer,
    Hangup,
    /// Refuse l'appel qui sonne ou le second appel.
    Reject,
    /// Met l'appel en cours en attente et prend le second.
    HoldAnswer,
    /// Bascule entre l'appel en cours et celui en attente (reprend un appel seul en attente).
    Swap,
    /// Numéro déjà validé ([`crate::engine::dial_number`]).
    Dial(String),
}

impl HfCmd {
    fn at(&self) -> String {
        match self {
            HfCmd::Answer => "ATA".into(),
            HfCmd::Hangup => "AT+CHUP".into(),
            HfCmd::Reject => "AT+CHLD=0".into(),
            HfCmd::HoldAnswer | HfCmd::Swap => "AT+CHLD=2".into(),
            // Garde-fou : rien d'autre qu'un numéro ne part dans la commande AT.
            HfCmd::Dial(n) => format!("ATD{};", n.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect::<String>()),
        }
    }
}

/// Enregistre le profil HF puis enchaîne les sessions de `addr` (reconnexion comprise) ; `on_event`
/// reçoit `Connected`/`Disconnected` et les événements AT. Se termine quand `cmds` est fermé.
/// Hors session, les commandes sont ignorées.
///
/// # Errors
/// Enregistrement du profil ou bus système impossible.
pub async fn run(addr: &BtAddr, mut cmds: mpsc::Receiver<HfCmd>, mut on_event: impl FnMut(&HfEvent)) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(1);
    let conn = Connection::system().await?;
    let path = ObjectPath::try_from(PROFILE_PATH).map_err(zbus::Error::from)?;
    conn.object_server()
        .at(&path, Agent { device_suffix: addr.node(), tx })
        .await?;
    let pm = ProfileManagerProxy::new(&conn).await?;
    let opts = HashMap::from([
        ("Name", Value::from("bluez-message HFP")),
        ("Role", Value::from("client")),
        // Lien appairé et chiffré exigé : sans clé de liaison, une adresse usurpée ne peut pas injecter RING/CLIP.
        ("RequireAuthentication", Value::from(true)),
        ("RequireAuthorization", Value::from(false)),
        ("Version", Value::from(0x0107u16)),
        ("Features", Value::from(0u16)),
    ]);
    pm.register_profile(&path, UUID_HFP_HF, opts).await?;
    tracing::info!("HF profile registered, waiting for the device to connect");

    let result = loop {
        let fd = tokio::select! {
            f = rx.recv() => match f { Some(f) => f, None => break Err(Error::Hfp(t!("channel closed").into())) },
            c = cmds.recv() => match c { Some(_) => continue, None => break Ok(()) },
        };
        on_event(&HfEvent::Connected);
        let r = session(fd, &mut cmds, &mut on_event).await;
        on_event(&HfEvent::Disconnected);
        match r {
            Ok(true) => continue,
            Ok(false) => break Ok(()),
            Err(e) => tracing::warn!("HFP session ended: {e}"),
        }
    };
    let _ = pm.unregister_profile(&path).await;
    result
}

/// `Ok(true)` : fin de session (pair parti) ; `Ok(false)` : `cmds` fermé, arrêt demandé.
async fn session(fd: OwnedFd, cmds: &mut mpsc::Receiver<HfCmd>, on_event: &mut impl FnMut(&HfEvent)) -> Result<bool> {
    let io = Rfcomm::new(fd)?;
    let mut slc = Slc::default();
    for c in slc.start() {
        io.write_cmd(&c).await?;
    }
    let (mut buf, mut acc) = ([0u8; 1024], Vec::<u8>::new());
    loop {
        let n = tokio::select! {
            r = io.read(&mut buf) => r?,
            c = cmds.recv() => match c {
                Some(c) => { io.write_cmd(&c.at()).await?; continue }
                None => return Ok(false),
            },
        };
        let Some(n) = n else { return Ok(true) };
        acc.extend_from_slice(&buf[..n]);
        while let Some(p) = acc.windows(2).position(|w| w == b"\r\n") {
            let raw: Vec<u8> = acc.drain(..p + 2).collect();
            let line = String::from_utf8_lossy(&raw[..p]).trim().to_owned();
            if line.is_empty() {
                continue;
            }
            match line.strip_prefix("+CLIP:") {
                Some(rest) => tracing::debug!(number = mask(rest.split('"').nth(1).unwrap_or_default()), "RX +CLIP"),
                None => tracing::debug!(%line, "RX"),
            }
            let (out, evs) = slc.on_line(&line);
            for c in out {
                io.write_cmd(&c).await?;
            }
            for ev in &evs {
                on_event(ev);
            }
        }
        if acc.len() > 8192 {
            return Err(Error::Hfp(t!("AT line too long").into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(slc: &mut Slc, lines: &[&str]) -> (Vec<String>, Vec<HfEvent>) {
        let (mut c, mut e) = (vec![], vec![]);
        for l in lines {
            let (cc, ee) = slc.on_line(l);
            c.extend(cc);
            e.extend(ee);
        }
        (c, e)
    }

    #[test]
    fn slc_sequence_then_ready() {
        let mut s = Slc::default();
        assert_eq!(s.start(), ["AT+BRSF=4"]);
        let (cmds, evs) = feed(&mut s, &["+BRSF: 1", "OK", "OK", "OK", "OK", "OK"]);
        assert_eq!(cmds, ["AT+CIND=?", "AT+CIND?", "AT+CMER=3,0,0,1", "AT+CLIP=1"]);
        assert_eq!(evs, [HfEvent::SlcReady]);
    }

    #[test]
    fn ciev_mapped_by_indicator_name() {
        let mut s = Slc::default();
        feed(&mut s, &[r#"+CIND: ("service",(0,1)),("call",(0,1)),("callsetup",(0-3))"#]);
        let (_, evs) = feed(&mut s, &["+CIEV: 3,1", "+CIEV: 9,1", "+CIEV: x"]);
        assert_eq!(
            evs,
            [HfEvent::IncomingCall, HfEvent::Indicator { name: Indicator::CallSetup, value: 1 }]
        );
    }

    #[test]
    fn indicator_names_round_trip() {
        for n in ["service", "call", "callsetup", "callheld", "signal", "roam", "battchg", "call_setup"] {
            let i = Indicator::from(n);
            assert_eq!(i.as_str(), n);
            assert_eq!(serde_json::to_string(&i).unwrap(), format!("\"{n}\""));
        }
        assert_eq!(Indicator::from("callheld"), Indicator::CallHeld);
        let ev: HfEvent = serde_json::from_str(r#"{"type":"indicator","name":"call","value":1}"#).unwrap();
        assert_eq!(ev, HfEvent::Indicator { name: Indicator::Call, value: 1 });
    }

    #[test]
    fn clip_ring_error() {
        let mut s = Slc::default();
        let (_, evs) = feed(&mut s, &["RING", r#"+CLIP: "+33712345601",145"#, "ERROR"]);
        assert_eq!(evs, [HfEvent::Ring, HfEvent::Clip { number: "+33712345601".into() }, HfEvent::Error]);
    }

    #[test]
    fn masking() {
        assert_eq!(mask("+33712345601"), "****5601");
        assert_eq!(mask("12"), "****12");
    }
}
