//! Hands-free unit (HFP, HF role) through BlueZ `ProfileManager1`: signalling only, no SCO audio.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};

use serde::{Deserialize, Serialize};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use zbus::{Connection, ObjectPath, OwnedVariant, Variant, interface, proxy};

use crate::addr::BtAddr;
use crate::error::{Error, Result};
use crate::phone::clean;
use crate::t;

const UUID_HFP_HF: &str = "0000111e-0000-1000-8000-00805f9b34fb";
const PROFILE_PATH: &str = "/org/bluez_message/hfp";

/// SLC initialisation sequence; each command is sent after the previous one's `OK`.
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
    /// Secondary calls (Telephony only): `hold` if the main call is itself on hold;
    /// `held` another call put on hold, `waiting` a second ringing call (empty number if hidden).
    Lines { hold: bool, held: Option<String>, waiting: Option<String> },
    Error,
}

/// `+CIND`/`+CIEV` indicator; serialised under its HFP name (`call`, `callsetup`…), unknown name kept as is.
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

/// AT protocol state machine, no I/O (testable).
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

    /// Returns (commands to send, events).
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

/// `"service",(0,1)),("call",(0,1))...` → names in order (CIEV index = position + 1).
fn parse_indicator_names(s: &str) -> Vec<Indicator> {
    s.split('"').skip(1).step_by(2).map(Indicator::from).collect()
}

pub use crate::log::mask;

struct Agent {
    device_suffix: String,
    tx: mpsc::Sender<OwnedFd>,
}

// Signatures imposed by `org.bluez.Profile1` (zbus methods are async, unused arguments kept).
#[allow(clippy::unused_async)]
#[interface(name = "org.bluez.Profile1")]
impl Agent {
    #[allow(clippy::needless_pass_by_value, reason = "D-Bus method signature, arguments owned by zbus")]
    fn new_connection(&self, device: ObjectPath<'_>, fd: zbus::OwnedFd, _props: HashMap<String, OwnedVariant>) {
        if device.as_str().ends_with(&self.device_suffix) {
            // Never awaits: a blocked handler would stall every BlueZ call on this connection.
            if let Err(e) = self.tx.try_send(fd.into()) {
                tracing::warn!("HFP connection dropped: {e}");
            }
        } // otherwise: fd closed on drop, foreign device ignored
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
    fn register_profile(&self, profile: &ObjectPath<'_>, uuid: &str, options: HashMap<&str, Variant<'_>>) -> zbus::Result<()>;
    fn unregister_profile(&self, profile: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// RFCOMM socket; `File` gives safe `read`/`write` on the descriptor.
struct Rfcomm(AsyncFd<std::fs::File>);

impl Rfcomm {
    fn new(fd: OwnedFd) -> Result<Self> {
        // SAFETY: fcntl on a valid fd that we own.
        let rc = unsafe {
            let fl = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK)
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: the File owns the fd, which stays open and stable until the AsyncFd is dropped.
        let afd = unsafe { AsyncFd::register(std::fs::File::from(fd)) }.map_err(|e| e.into_parts().1)?;
        Ok(Self(afd))
    }

    /// `Ok(None)` = closed by the peer.
    async fn read(&self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        loop {
            let mut g = self.0.readable().await?;
            let r = g.try_io(|f| std::io::Read::read(&mut f.get_ref(), buf));
            match r {
                Ok(Ok(0)) => return Ok(None),
                Ok(Ok(n)) => return Ok(Some(n)),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => {}
            }
        }
    }

    async fn write_cmd(&self, cmd: &str) -> std::io::Result<()> {
        let data = format!("{cmd}\r");
        let mut rest = data.as_bytes();
        while !rest.is_empty() {
            let mut g = self.0.writable().await?;
            let r = g.try_io(|f| std::io::Write::write(&mut f.get_ref(), rest));
            match r {
                Ok(Ok(0)) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(Ok(n)) => rest = rest.get(n..).unwrap_or_default(),
                Ok(Err(e)) => return Err(e),
                Err(_would_block) => {}
            }
        }
        // ATD<number>: dialled number masked.
        if let Some(n) = cmd.strip_prefix("ATD") { tracing::debug!(number = mask(n.trim_end_matches(';')), "TX ATD") } else { tracing::debug!(cmd, "TX") }
        Ok(())
    }
}

/// Call commands accepted during a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HfCmd {
    Answer,
    Hangup,
    /// Rejects the ringing call or the second call.
    Reject,
    /// Puts the current call on hold and takes the second one.
    HoldAnswer,
    /// Switches between the current call and the one on hold (resumes a single held call).
    Swap,
    /// Number already validated ([`crate::engine::dial_number`]).
    Dial(String),
}

impl HfCmd {
    fn at(&self) -> String {
        match self {
            HfCmd::Answer => "ATA".into(),
            HfCmd::Hangup => "AT+CHUP".into(),
            HfCmd::Reject => "AT+CHLD=0".into(),
            HfCmd::HoldAnswer | HfCmd::Swap => "AT+CHLD=2".into(),
            // Safeguard: nothing but a number goes into the AT command.
            HfCmd::Dial(n) => format!("ATD{};", n.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect::<String>()),
        }
    }
}

/// Registers the HF profile then chains the sessions of `addr` (reconnection included); `on_event`
/// receives `Connected`/`Disconnected` and the AT events. Ends when `cmds` is closed.
/// Outside a session, commands are ignored.
///
/// # Errors
/// Profile registration or system bus impossible.
pub async fn run(addr: &BtAddr, mut cmds: mpsc::Receiver<HfCmd>, mut on_event: impl FnMut(&HfEvent)) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(4);
    let conn = Connection::system().await?;
    let path = ObjectPath::try_from(PROFILE_PATH)?;
    conn.object_server()
        .at(&path, Agent { device_suffix: addr.node(), tx })
        .await?;
    let pm = ProfileManagerProxy::new(&conn).await?;
    let opts = HashMap::from([
        ("Name", Variant::from("bluez-message HFP")),
        ("Role", Variant::from("client")),
        // Paired and encrypted link required: without a link key, a spoofed address cannot inject RING/CLIP.
        ("RequireAuthentication", Variant::from(true)),
        ("RequireAuthorization", Variant::from(false)),
        ("Version", Variant::from(0x0107u16)),
        ("Features", Variant::from(0u16)),
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
            Ok(true) => {}
            Ok(false) => break Ok(()),
            Err(e) => tracing::warn!("HFP session ended: {e}"),
        }
    };
    let _ = pm.unregister_profile(&path).await;
    result
}

/// `Ok(true)`: end of session (peer gone); `Ok(false)`: `cmds` closed, stop requested.
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
                // +CLCC, +CNUM…: quoted fields (numbers, names) never logged.
                None if line.contains('"') => tracing::debug!(cmd = line.split(':').next().unwrap_or_default(), "RX (quoted fields hidden)"),
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
