//! MAP + calls engine (PipeWire Telephony) shared by the daemon (`serve`) and the interface (`bluez-message`):
//! commands come in through [`Engine::submit`], [`Out`] events go out through a channel.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;

use crate::addr::BtAddr;
use crate::bmsg;
use crate::error::Result;
use crate::hfp::{HfCmd, HfEvent};
use crate::map::{self, Folder, MapClient, Sms};
use crate::settings::Settings;
use crate::telephony;
use crate::t;

/// Maximum length of an accepted SMS text (characters).
pub const MAX_TEXT: usize = 2000;
const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Reconnection delay doubled on each failure, from `BACKOFF_MIN` to `BACKOFF_MAX`.
struct Backoff(Duration);

impl Backoff {
    fn new() -> Self {
        Self(BACKOFF_MIN)
    }

    /// Delay to wait now; the next one is doubled (capped).
    fn next(&mut self) -> Duration {
        let d = self.0;
        self.0 = (d * 2).min(BACKOFF_MAX);
        d
    }

    fn reset(&mut self) {
        self.0 = BACKOFF_MIN;
    }
}

/// Tracked folders: `inbox` for arrivals, `sent` to complete conversations.
const FOLDERS: [Folder; 2] = [Folder::Inbox, Folder::Sent];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Cmd {
    Send { to: String, text: String },
    Answer,
    Hangup,
    /// Rejects the ringing call or the second call.
    Reject,
    /// Puts the current call on hold and takes the second one.
    HoldAnswer,
    /// Switches between the current call and the one on hold.
    Swap,
    /// Dials a number from the iPhone.
    Dial { number: String },
    /// Mutes or unmutes the PC microphone: handled by the daemon, not by the engine.
    Mute,
    /// Conversation read up to `until` (MAP timestamp): recorded by the daemon, not by the engine.
    MarkRead { key: String, until: String },
    /// Removes an item (`item`) or the whole conversation from the local history: handled by the daemon.
    Delete { key: String, item: Option<Item> },
    /// App settings: handled by the daemon.
    Settings { settings: Settings },
    /// Restarts the iPhone's Bluetooth link (Disconnect + Connect): handled by the daemon.
    Reconnect,
    /// Switches iPhone (paired device): the daemon stores the address and restarts.
    SetDevice { addr: BtAddr },
    /// Permanently erases the SMS deleted from the app: handled by the daemon.
    PurgeHidden,
    /// Clears the local history: handled by the daemon.
    ClearHistory,
    Quit,
}

/// Paired Bluetooth device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub addr: BtAddr,
    pub name: String,
    pub connected: bool,
}

/// Item of a conversation (SMS or call line), designated for a deletion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Item {
    /// MAP timestamp of the item.
    pub timestamp: String,
    /// SMS text (empty for a call).
    pub text: String,
    pub outgoing: bool,
    #[serde(default)]
    pub call: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Out {
    /// `new`: false for the folder's initial state, true for an arrival.
    Sms { #[serde(flatten)] sms: Sms, folder: Folder, new: bool },
    Sent {
        to: String,
        text: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// Local time of the send, MAP format (`YYYYMMDDTHHMMSS`).
        #[serde(default)]
        timestamp: String,
    },
    Call(HfEvent),
    /// Call ended, recorded in the history by the daemon.
    CallLog {
        number: String,
        /// Start of the call, MAP format.
        timestamp: String,
        kind: CallKind,
        /// Duration of the conversation (0 if not answered).
        secs: u64,
        /// Replayed from the history: a missed call no longer counts as unread.
        #[serde(default)]
        read: bool,
    },
    /// iPhone contacts (PBAP): (number, name).
    Contacts { names: Vec<(String, String)> },
    /// PC microphone muted or unmuted by the daemon during the call.
    Muted { on: bool },
    /// Conversation read up to `until` (MAP timestamp).
    Read { key: String, until: String },
    /// Item (or whole conversation if `item` is absent) removed from the history.
    Deleted { key: String, item: Option<Item> },
    Status { link: Profile, up: bool },
    /// Settings in effect, available languages, paired devices and tracked address; `home`: country
    /// numbers are read with (ISO code, see [`crate::numbering`]), `auto`: the one without a setting.
    Settings {
        settings: Settings,
        langs: Vec<String>,
        devices: Vec<Device>,
        device: BtAddr,
        #[serde(default)]
        home: Option<String>,
        #[serde(default)]
        auto: Option<String>,
    },
    /// Local history cleared.
    Cleared,
    /// Report of a command (purge…), addressed to the requester only.
    Info { msg: String },
    Error { msg: String },
}

/// Tracked Bluetooth link: calls (HFP), messages (MAP) or contacts (PBAP, result of the last synchronisation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Hfp,
    Map,
    Pbap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    In,
    Out,
    Missed,
}

impl CallKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
            Self::Missed => "missed",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "in" => Some(Self::In),
            "out" => Some(Self::Out),
            "missed" => Some(Self::Missed),
            _ => None,
        }
    }
}

/// Validates a send before any action; the error message is meant for the interface.
///
/// # Errors
/// Translated message: invalid recipient, empty or too long text.
pub fn validate_send(to: &str, text: &str) -> std::result::Result<(), String> {
    bmsg::validate_number(to).map_err(|e| e.to_string())?;
    if text.trim().is_empty() || text.chars().count() > MAX_TEXT {
        return Err(t!("empty text or longer than {0} characters", MAX_TEXT));
    }
    Ok(())
}

/// Number to dial: presentation separators removed (` `, `-`, `.`, `(`, `)`), then optional leading `+`
/// and 2 to 30 digits. `*` and `#` rejected: a spoofed sender (`**21*…#`) would turn a
/// "Call back" into an MMI code (call forwarding).
///
/// # Errors
/// Translated message if the number does not have that form.
pub fn dial_number(n: &str) -> std::result::Result<String, String> {
    let n = crate::phone::strip_separators(n);
    let body = n.strip_prefix('+').unwrap_or(&n);
    if (2..=30).contains(&body.len()) && body.bytes().all(|b| b.is_ascii_digit()) {
        Ok(n)
    } else {
        Err(t!("invalid number: “{0}”", n))
    }
}

fn is_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && !key.chars().any(char::is_control)
}

fn validate_read(key: &str, until: &str) -> std::result::Result<(), String> {
    if !is_key(key) || !crate::stamp::is_valid(until) {
        return Err(t!("invalid read marker").into());
    }
    Ok(())
}

fn validate_delete(key: &str, item: Option<&Item>) -> std::result::Result<(), String> {
    if !is_key(key) || item.is_some_and(|i| !crate::stamp::is_valid(&i.timestamp) || (i.call && !i.text.is_empty())) {
        return Err(t!("invalid deletion").into());
    }
    Ok(())
}

/// Decodes and validates a JSON command.
///
/// # Errors
/// Translated message: unreadable JSON or field rejected by validation.
pub fn parse_cmd(line: &str) -> std::result::Result<Cmd, String> {
    let cmd: Cmd = serde_json::from_str(line).map_err(|e| t!("unreadable command: {0}", e))?;
    match &cmd {
        Cmd::Send { to, text } => validate_send(to, text)?,
        Cmd::Dial { number } => drop(dial_number(number)?),
        Cmd::MarkRead { key, until } => validate_read(key, until)?,
        Cmd::Delete { key, item } => validate_delete(key, item.as_ref())?,
        Cmd::Settings { settings } => settings.validate()?,
        _ => {}
    }
    Ok(cmd)
}

/// Wake-up of the MAP session waiting to reconnect (iPhone is back).
#[derive(Clone, Default)]
pub struct Nudge(Arc<Notify>);

impl Nudge {
    /// Immediate MAP retry, backoff reset; no noticeable effect if the session is open.
    pub fn nudge(&self) {
        self.0.notify_one();
    }
}

pub struct Engine {
    send_tx: mpsc::Sender<(String, String)>,
    nudge: Nudge,
    hf_tx: mpsc::Sender<HfCmd>,
    map_task: JoinHandle<()>,
    hfp_task: JoinHandle<()>,
}

impl Engine {
    /// Starts the MAP and HFP tasks; to be called inside a tokio runtime.
    #[must_use]
    pub fn start(addr: &BtAddr, interval: Duration) -> (Self, mpsc::UnboundedReceiver<Out>) {
        let addr = addr.clone();
        // Unbounded on purpose: telephony callbacks are synchronous (no back-pressure possible without
        // losing call events), producers are bounded by the phone's folders and the owner drains it continuously.
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (send_tx, send_rx) = mpsc::channel(8);
        let (hf_tx, hf_rx) = mpsc::channel(8);

        let nudge = Nudge::default();
        let map_task = tokio::spawn(map_loop(addr.clone(), interval, send_rx, out_tx.clone(), nudge.clone()));
        let hfp_task = tokio::spawn(async move {
            let (out, warn) = (out_tx.clone(), out_tx.clone());
            let mut hf_rx = hf_rx;
            let mut on_event = move |ev: &HfEvent| {
                let _ = out.send(match ev {
                    HfEvent::Connected => Out::Status { link: Profile::Hfp, up: true },
                    HfEvent::Disconnected => Out::Status { link: Profile::Hfp, up: false },
                    ev => Out::Call(ev.clone()),
                });
            };
            let mut on_warn = move |msg| {
                let _ = warn.send(Out::Error { msg });
            };
            // Session bus lost: retry with backoff; the error is reported only once.
            let mut backoff = Backoff::new();
            let mut reported = false;
            while let Err(e) = telephony::run(&addr, &mut hf_rx, &mut on_event, &mut on_warn).await {
                tracing::warn!("calls: {e}");
                if !reported {
                    reported = true;
                    let _ = out_tx.send(Out::Error { msg: t!("calls unavailable: {0}", e) });
                }
                tokio::time::sleep(backoff.next()).await;
            }
        });
        (Self { send_tx, nudge, hf_tx, map_task, hfp_task }, out_rx)
    }

    /// Forwards a command (non-blocking). `Quit` is for the caller to handle via [`Engine::shutdown`].
    ///
    /// # Errors
    /// Translated message: field rejected, queue full, daemon-only command or engine stopped.
    pub fn submit(&self, cmd: Cmd) -> std::result::Result<(), String> {
        match cmd {
            Cmd::Send { to, text } => {
                validate_send(&to, &text)?;
                self.send_tx.try_send((to, text)).map_err(|_| t!("send queue full").to_owned())
            }
            Cmd::Answer => self.call(HfCmd::Answer),
            Cmd::Hangup => self.call(HfCmd::Hangup),
            Cmd::Reject => self.call(HfCmd::Reject),
            Cmd::HoldAnswer => self.call(HfCmd::HoldAnswer),
            Cmd::Swap => self.call(HfCmd::Swap),
            Cmd::Dial { number } => self.call(HfCmd::Dial(dial_number(&number)?)),
            Cmd::Mute
            | Cmd::MarkRead { .. }
            | Cmd::Delete { .. }
            | Cmd::Settings { .. }
            | Cmd::Reconnect
            | Cmd::SetDevice { .. }
            | Cmd::PurgeHidden
            | Cmd::ClearHistory => Err(t!("command handled by the bluez-messaged daemon").to_owned()),
            Cmd::Quit => Ok(()),
        }
    }

    /// MAP session wake-up handle, for link monitoring.
    #[must_use]
    pub fn nudger(&self) -> Nudge {
        self.nudge.clone()
    }

    fn call(&self, cmd: HfCmd) -> std::result::Result<(), String> {
        self.hf_tx.try_send(cmd).map_err(|_| t!("HFP busy").to_owned())
    }

    /// Closing the channels stops the calls task cleanly; MAP is interrupted.
    pub async fn shutdown(self) {
        drop(self.hf_tx);
        drop(self.send_tx);
        let mut hfp = self.hfp_task;
        if tokio::time::timeout(Duration::from_secs(3), &mut hfp).await.is_err() {
            hfp.abort();
        }
        self.map_task.abort();
    }
}

/// MAP session kept open: initial state, then polling and sends; reconnection with backoff.
/// The link state is emitted only when it changes.
async fn map_loop(addr: BtAddr, interval: Duration, mut sends: mpsc::Receiver<(String, String)>, out: UnboundedSender<Out>, nudge: Nudge) {
    let mut seen: [Option<HashSet<String>>; 2] = [None, None];
    let mut backoff = Backoff::new();
    let mut last: Option<bool> = None;
    let set_up = |last: &mut Option<bool>, up: bool| {
        if *last != Some(up) {
            *last = Some(up);
            let _ = out.send(Out::Status { link: Profile::Map, up });
        }
    };
    loop {
        match MapClient::connect(&addr).await {
            Err(e) => {
                tracing::warn!("MAP: {e}");
                set_up(&mut last, false);
                if wait_or_refuse(&mut sends, &mut backoff, &out, &nudge).await { return; }
            }
            Ok(client) => {
                set_up(&mut last, true);
                // Wake-up emitted during the connection: already satisfied, must not shorten the next backoff.
                let _ = nudge.0.notified().now_or_never();
                let started = tokio::time::Instant::now();
                let r = session(&client, &mut seen, interval, &mut sends, &out).await;
                let _ = client.close().await;
                match r {
                    Ok(()) => return, // send channel closed: stop
                    Err(e) => {
                        tracing::warn!("MAP lost: {e}");
                        set_up(&mut last, false);
                        // Short session = repeated failure: the backoff keeps growing.
                        if started.elapsed() >= BACKOFF_MAX {
                            backoff.reset();
                        }
                        if wait_or_refuse(&mut sends, &mut backoff, &out, &nudge).await { return; }
                    }
                }
            }
        }
    }
}

/// Waits for the backoff delay while refusing sends (no session); a wake-up shortens it and resets the
/// backoff. True if the channel is closed.
async fn wait_or_refuse(sends: &mut mpsc::Receiver<(String, String)>, backoff: &mut Backoff, out: &UnboundedSender<Out>, nudge: &Nudge) -> bool {
    let deadline = tokio::time::sleep(backoff.next());
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => return false,
            () = nudge.0.notified() => {
                backoff.reset();
                return false;
            }
            s = sends.recv() => match s {
                None => return true,
                Some((to, text)) => {
                    let _ = out.send(Out::Sent { to, text, ok: false, error: Some(t!("phone not connected (MAP)").into()), timestamp: crate::stamp::now() });
                }
            },
        }
    }
}

async fn session(
    client: &MapClient,
    seen: &mut [Option<HashSet<String>>; 2],
    interval: Duration,
    sends: &mut mpsc::Receiver<(String, String)>,
    out: &UnboundedSender<Out>,
) -> Result<()> {
    loop {
        for (folder, seen) in FOLDERS.into_iter().zip(seen.iter_mut()) {
            // iOS exposes an empty or unstable `sent`: its failure does not interrupt the session.
            let all = match client.list(folder).await {
                Ok(all) => all,
                Err(e) if folder.is_sent() => {
                    tracing::debug!("MAP sent: {e}");
                    continue;
                }
                Err(e) => return Err(e),
            };
            match seen.as_mut() {
                None => {
                    *seen = Some(all.iter().map(|m| m.handle.clone()).collect());
                    for sms in all {
                        let _ = out.send(Out::Sms { sms, folder, new: false });
                    }
                }
                Some(s) => {
                    for sms in map::new_messages(s, all) {
                        let _ = out.send(Out::Sms { sms, folder, new: true });
                    }
                }
            }
        }
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            s = sends.recv() => match s {
                None => return Ok(()),
                Some((to, text)) => {
                    let r = client.send(&to, &text).await;
                    let error = r.as_ref().err().map(ToString::to_string);
                    let _ = out.send(Out::Sent { to, text, ok: r.is_ok(), error, timestamp: crate::stamp::now() });
                    r?;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_commands() {
        assert_eq!(parse_cmd(r#"{"cmd":"answer"}"#), Ok(Cmd::Answer));
        assert_eq!(parse_cmd(r#"{"cmd":"hold_answer"}"#), Ok(Cmd::HoldAnswer));
        assert_eq!(
            parse_cmd(r#"{"cmd":"send","to":"+33712345601","text":"salut"}"#),
            Ok(Cmd::Send { to: "+33712345601".into(), text: "salut".into() })
        );
        assert_eq!(parse_cmd(r#"{"cmd":"dial","number":"+33 6 12-34.56 (78)"}"#), Ok(Cmd::Dial { number: "+33 6 12-34.56 (78)".into() }));
        assert_eq!(dial_number("+33 6 12-34.56 (78)").as_deref(), Ok("+33612345678"));
        assert!(parse_cmd(r#"{"cmd":"mark_read","key":"612345678","until":"20261015T120000"}"#).is_ok());
    }

    #[test]
    fn rejects_bad_dial_and_read() {
        for n in ["", "+", "1", "06;ATH", "0612\r\nAT", "abc", "*#06#", "**21*12345#", "#31#0612"] {
            assert!(parse_cmd(&format!(r#"{{"cmd":"dial","number":"{n}"}}"#)).is_err(), "{n}");
        }
        assert!(parse_cmd(r#"{"cmd":"mark_read","key":"","until":"x"}"#).is_err());
        assert!(parse_cmd(r#"{"cmd":"mark_read","key":"k","until":"2026'; --"}"#).is_err());
        assert!(parse_cmd(r#"{"cmd":"mark_read","key":"k","until":"zzzzzzzzTzzzzzz"}"#).is_err());
        assert!(parse_cmd(r#"{"cmd":"mark_read","key":"k","until":"20261015T120000+0200"}"#).is_ok());
        assert!(parse_cmd(r#"{"cmd":"delete","key":"612345678","item":null}"#).is_ok());
        let one = r#"{"cmd":"delete","key":"612345678","item":{"timestamp":"20261015T120000","text":"a","outgoing":true}}"#;
        assert!(matches!(parse_cmd(one), Ok(Cmd::Delete { item: Some(Item { call: false, .. }), .. })));
        assert!(parse_cmd(r#"{"cmd":"delete","key":"","item":null}"#).is_err());
        assert!(parse_cmd(r#"{"cmd":"delete","key":"k","item":{"timestamp":"x","text":"a","outgoing":true}}"#).is_err());
        assert!(parse_cmd(r#"{"cmd":"delete","key":"k","item":{"timestamp":"20261015T120000","text":"a","outgoing":true,"call":true}}"#).is_err());
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert!(parse_cmd("plop").is_err());
        assert!(parse_cmd(r#"{"cmd":"rm"}"#).is_err());
        assert!(parse_cmd(r#"{"cmd":"send","to":"x;rm","text":"a"}"#).is_err());
        assert!(parse_cmd(r#"{"cmd":"send","to":"+33712345601","text":"  "}"#).is_err());
        let long = "a".repeat(MAX_TEXT + 1);
        assert!(parse_cmd(&format!(r#"{{"cmd":"send","to":"+33712345601","text":"{long}"}}"#)).is_err());
    }

    #[test]
    fn output_shapes() {
        let call = serde_json::to_string(&Out::Call(HfEvent::Clip { number: "+331".into() })).unwrap();
        assert_eq!(call, r#"{"event":"call","type":"clip","number":"+331"}"#);
        let st = serde_json::to_string(&Out::Status { link: Profile::Map, up: true }).unwrap();
        assert_eq!(st, r#"{"event":"status","link":"map","up":true}"#);
        let sms = Sms { handle: "h".into(), text: "x".into(), ..Default::default() };
        let o = Out::Sms { sms, folder: Folder::Inbox, new: true };
        let line = serde_json::to_string(&o).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["event"], "sms");
        assert_eq!(v["handle"], "h");
        assert_eq!(v["folder"], "inbox");
        assert_eq!(v["new"], true);
        // Round trip: the GUI rereads the daemon's lines.
        assert_eq!(serde_json::from_str::<Out>(&line).unwrap(), o);
        let c = Out::Call(HfEvent::Indicator { name: "call".into(), value: 1 });
        assert_eq!(serde_json::from_str::<Out>(&serde_json::to_string(&c).unwrap()).unwrap(), c);
        let s = Out::Sent { to: "+331".into(), text: "x".into(), ok: true, error: None, timestamp: "20261015T100000".into() };
        assert_eq!(serde_json::from_str::<Out>(&serde_json::to_string(&s).unwrap()).unwrap(), s);
    }
}
