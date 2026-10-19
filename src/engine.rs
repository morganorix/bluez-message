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
use crate::error::{Error, Field, Queue, Result};
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
    /// Brings the window forward (started if closed), on the conversation with `number` if any: handled by the daemon.
    Open {
        #[serde(default)]
        number: Option<String>,
    },
    /// Sent by the main window once connected: it receives [`Out::Open`] from then on. Handled by the daemon.
    Window,
    /// Powers the Bluetooth adapter on or off: handled by the daemon (setup).
    Powered { on: bool },
    /// Starts or stops the search for nearby devices: handled by the daemon (setup).
    Discover { on: bool },
    /// Pairs a device (then trusts and connects it): handled by the daemon (setup).
    Pair { addr: BtAddr },
    /// Answer to the pending passkey confirmation ([`Out::Confirm`]): handled by the daemon (setup).
    Confirm { ok: bool },
    /// Connects a paired device: handled by the daemon (setup).
    Connect { addr: BtAddr },
    /// Removes a paired device from BlueZ (not the one in use): handled by the daemon.
    Forget { addr: BtAddr },
    /// Searches the SMS of the history (accents and case ignored), newest first, page after `before`:
    /// handled by the daemon, answered to the requester only ([`Out::Hits`]).
    Search {
        query: String,
        #[serde(default)]
        before: Option<Cursor>,
    },
    /// SMS of conversation `key` older than `before`, at most `limit` ([`MAX_OLDER`]): handled by the daemon,
    /// answered to the requester only ([`Out::Older`]).
    Older { key: String, before: Cursor, limit: u32 },
    /// Mute (no notification) or archive conversation `key`: handled by the daemon, published as [`Out::Conv`].
    ConvFlags { key: String, #[serde(flatten)] flags: ConvFlags },
    /// Writes the history (conversation `key`, or all of it) to a JSON file of the download folder:
    /// handled by the daemon, answered to the requester only ([`Out::Exported`]).
    Export {
        #[serde(default)]
        key: Option<String>,
    },
    Quit,
}

impl Cmd {
    /// Command for the adapter and pairing ([`crate::setup::Setup`]), available without a configured device.
    #[must_use]
    pub fn is_setup(&self) -> bool {
        matches!(self, Self::Powered { .. } | Self::Discover { .. } | Self::Pair { .. } | Self::Confirm { .. } | Self::Connect { .. })
    }
}

/// Position in the history, newest first: the following page starts strictly before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    /// MAP timestamp.
    pub ts: String,
    /// Row identifier, breaks ties between items with the same timestamp.
    pub id: i64,
}

impl Cursor {
    /// Just after everything dated `ts`: a page from it includes the items with that timestamp.
    #[must_use]
    pub fn after(ts: &str) -> Self {
        Self { ts: ts.to_owned(), id: i64::MAX }
    }

    fn is_valid(&self) -> bool {
        crate::stamp::is_valid(&self.ts) && self.id >= 0
    }
}

/// Per-conversation flags; both off: nothing is recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvFlags {
    /// No notification for its SMS (calls still ring).
    #[serde(default)]
    pub muted: bool,
    /// Listed apart, until the next incoming SMS.
    #[serde(default)]
    pub archived: bool,
}

/// SMS of the history found by [`Cmd::Search`] or loaded by [`Cmd::Older`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    pub id: i64,
    /// Conversation key (see [`crate::numbering::key`]).
    pub key: String,
    pub number: String,
    /// MAP timestamp.
    pub timestamp: String,
    pub text: String,
    pub outgoing: bool,
}

impl Hit {
    /// Where the page after this item starts.
    #[must_use]
    pub fn cursor(&self) -> Cursor {
        Cursor { ts: self.timestamp.clone(), id: self.id }
    }
}

/// Characters of a [`Cmd::Search`] query.
pub const MAX_QUERY: usize = 100;
/// Results per [`Out::Hits`] page.
pub const HITS_PAGE: usize = 100;
/// Most SMS per [`Cmd::Older`].
pub const MAX_OLDER: u32 = 200;

/// Paired Bluetooth device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub addr: BtAddr,
    pub name: String,
    pub connected: bool,
}

/// Most devices listed by [`Out::Found`].
pub const MAX_FOUND: usize = 32;
/// Characters kept from a device name (set by whoever is nearby).
pub const MAX_NAME: usize = 64;
/// Largest passkey displayed by BlueZ (6 digits).
pub const MAX_PASSKEY: u32 = 999_999;

/// Device seen by the search (paired or not).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nearby {
    pub addr: BtAddr,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
    /// Device class or icon says phone.
    pub phone: bool,
}

impl Nearby {
    /// The name comes from any device in range: cleaned ([`crate::phone::clean`]) and capped at [`MAX_NAME`] characters.
    #[must_use]
    pub fn new(addr: BtAddr, name: &str, paired: bool, connected: bool, phone: bool) -> Self {
        let name = crate::phone::clean(name, false).chars().take(MAX_NAME).collect();
        Self { addr, name, paired, connected, phone }
    }
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
    /// Phone battery level (0..=100), `None` when BlueZ does not expose it.
    Battery { percent: Option<u8> },
    /// The open window must take the focus, and show the conversation with `number` if any (transient, sent to
    /// the windows subscribed with [`Cmd::Window`] only).
    Open {
        #[serde(default)]
        number: Option<String>,
    },
    /// Settings in effect, available languages, paired devices and tracked address; `home`: country
    /// numbers are read with (ISO code, see [`crate::numbering`]), `auto`: the one without a setting.
    Settings {
        settings: Settings,
        langs: Vec<String>,
        devices: Vec<Device>,
        /// `None`: no device configured (setup mode).
        #[serde(default)]
        device: Option<BtAddr>,
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
    /// Bluetooth adapter state (`present`: adapter found on the system bus).
    Adapter { present: bool, powered: bool, discovering: bool },
    /// Devices seen by the search, at most [`MAX_FOUND`] (see [`Out::found`]).
    Found { devices: Vec<Nearby> },
    /// Pairing in progress: the passkey shown by the phone must be confirmed ([`Cmd::Confirm`]).
    Confirm { addr: BtAddr, passkey: u32 },
    /// End of a pairing.
    Paired {
        addr: BtAddr,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Page of [`Cmd::Search`] results for `query` (as sent), to the requester only; `next`: the following page, if any.
    Hits { query: String, items: Vec<Hit>, next: Option<Cursor> },
    /// SMS of conversation `key` asked by [`Cmd::Older`], newest first, to the requester only; `next`: the following page, if any.
    Older { key: String, items: Vec<Hit>, next: Option<Cursor> },
    /// Flags of conversation `key` (replayed in the snapshot; an incoming SMS takes it out of the archive).
    Conv { key: String, #[serde(flatten)] flags: ConvFlags },
    /// File written by [`Cmd::Export`], to the requester only.
    Exported { path: String },
    /// [`Cmd::Export`] refused or failed (translated reason), to the requester only: unlike a broadcast
    /// [`Out::Error`], it always answers that request.
    ExportFailed { msg: String },
    /// Inbox listing with the phone's read flags, for the daemon only: never serialised nor published.
    #[serde(skip)]
    Flags { items: Vec<Sms> },
}

impl Out {
    /// [`Out::Found`] capped at [`MAX_FOUND`] devices.
    #[must_use]
    pub fn found(mut devices: Vec<Nearby>) -> Self {
        devices.truncate(MAX_FOUND);
        Self::Found { devices }
    }

    /// [`Out::Confirm`], `None` if BlueZ gives a passkey beyond 6 digits.
    #[must_use]
    pub fn confirm(addr: BtAddr, passkey: u32) -> Option<Self> {
        (passkey <= MAX_PASSKEY).then_some(Self::Confirm { addr, passkey })
    }
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
/// Invalid recipient, empty or too long text.
pub fn validate_send(to: &str, text: &str) -> Result<()> {
    bmsg::validate_number(to)?;
    if text.trim().is_empty() || text.chars().count() > MAX_TEXT {
        return Err(Error::Invalid(Field::Text));
    }
    Ok(())
}

/// Number to dial: presentation separators removed (` `, `-`, `.`, `(`, `)`), then optional leading `+`
/// and 2 to 30 digits. `*` and `#` rejected: a spoofed sender (`**21*…#`) would turn a
/// "Call back" into an MMI code (call forwarding).
///
/// # Errors
/// [`Error::InvalidNumber`] if the number does not have that form.
pub fn dial_number(n: &str) -> Result<String> {
    let n = crate::phone::strip_separators(n);
    let body = n.strip_prefix('+').unwrap_or(&n);
    if (2..=30).contains(&body.len()) && body.bytes().all(|b| b.is_ascii_digit()) {
        Ok(n)
    } else {
        Err(Error::InvalidNumber(n))
    }
}

fn is_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && !key.chars().any(char::is_control)
}

fn validate_read(key: &str, until: &str) -> Result<()> {
    if !is_key(key) || !crate::stamp::is_valid(until) {
        return Err(Error::Invalid(Field::ReadMarker));
    }
    Ok(())
}

fn validate_search(query: &str, before: Option<&Cursor>) -> Result<()> {
    let n = crate::phone::clean(query, false).trim().chars().count();
    if !(1..=MAX_QUERY).contains(&n) || before.is_some_and(|c| !c.is_valid()) {
        return Err(Error::Invalid(Field::Search));
    }
    Ok(())
}

fn validate_older(key: &str, before: &Cursor, limit: u32) -> Result<()> {
    if !is_key(key) || !before.is_valid() || !(1..=MAX_OLDER).contains(&limit) {
        return Err(Error::Invalid(Field::HistoryRequest));
    }
    Ok(())
}

fn validate_delete(key: &str, item: Option<&Item>) -> Result<()> {
    if !is_key(key) || item.is_some_and(|i| !crate::stamp::is_valid(&i.timestamp) || (i.call && !i.text.is_empty())) {
        return Err(Error::Invalid(Field::Deletion));
    }
    Ok(())
}

/// Decodes and validates a JSON command.
///
/// # Errors
/// [`Error::BadCommand`] (unreadable JSON) or field rejected by validation.
pub fn parse_cmd(line: &str) -> Result<Cmd> {
    let cmd: Cmd = serde_json::from_str(line).map_err(Error::BadCommand)?;
    match &cmd {
        Cmd::Send { to, text } => validate_send(to, text)?,
        Cmd::Dial { number } => drop(dial_number(number)?),
        Cmd::MarkRead { key, until } => validate_read(key, until)?,
        Cmd::Delete { key, item } => validate_delete(key, item.as_ref())?,
        Cmd::Settings { settings } => settings.validate()?,
        Cmd::Search { query, before } => validate_search(query, before.as_ref())?,
        Cmd::Older { key, before, limit } => validate_older(key, before, *limit)?,
        Cmd::ConvFlags { key, .. } | Cmd::Export { key: Some(key) } if !is_key(key) => return Err(Error::Invalid(Field::Conversation)),
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
    /// Field rejected, queue full, daemon-only command or engine stopped.
    pub fn submit(&self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::Send { to, text } => {
                validate_send(&to, &text)?;
                self.send_tx.try_send((to, text)).map_err(|_| Error::Busy(Queue::Send))
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
            | Cmd::ClearHistory
            | Cmd::Open { .. }
            | Cmd::Window
            | Cmd::Powered { .. }
            | Cmd::Discover { .. }
            | Cmd::Pair { .. }
            | Cmd::Confirm { .. }
            | Cmd::Connect { .. }
            | Cmd::Forget { .. }
            | Cmd::Search { .. }
            | Cmd::Older { .. }
            | Cmd::ConvFlags { .. }
            | Cmd::Export { .. } => Err(Error::DaemonOnly),
            Cmd::Quit => Ok(()),
        }
    }

    /// MAP session wake-up handle, for link monitoring.
    #[must_use]
    pub fn nudger(&self) -> Nudge {
        self.nudge.clone()
    }

    fn call(&self, cmd: HfCmd) -> Result<()> {
        self.hf_tx.try_send(cmd).map_err(|_| Error::Busy(Queue::Hfp))
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
            // After the SMS: the read marks apply to conversations already known.
            let flags = (!folder.is_sent()).then(|| all.clone());
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
            if let Some(items) = flags {
                let _ = out.send(Out::Flags { items });
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
        assert_eq!(parse_cmd(r#"{"cmd":"answer"}"#).ok(), Some(Cmd::Answer));
        assert_eq!(parse_cmd(r#"{"cmd":"hold_answer"}"#).ok(), Some(Cmd::HoldAnswer));
        assert_eq!(
            parse_cmd(r#"{"cmd":"send","to":"+33712345601","text":"salut"}"#).ok(),
            Some(Cmd::Send { to: "+33712345601".into(), text: "salut".into() })
        );
        assert_eq!(parse_cmd(r#"{"cmd":"dial","number":"+33 6 12-34.56 (78)"}"#).ok(), Some(Cmd::Dial { number: "+33 6 12-34.56 (78)".into() }));
        assert_eq!(dial_number("+33 6 12-34.56 (78)").ok().as_deref(), Some("+33612345678"));
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
        let bat = serde_json::to_string(&Out::Battery { percent: Some(80) }).unwrap();
        assert_eq!(bat, r#"{"event":"battery","percent":80}"#);
        assert_eq!(serde_json::from_str::<Out>(r#"{"event":"battery","percent":null}"#).unwrap(), Out::Battery { percent: None });
        let open = serde_json::to_string(&Out::Open { number: Some("+331".into()) }).unwrap();
        assert_eq!(open, r#"{"event":"open","number":"+331"}"#);
        assert_eq!(serde_json::from_str::<Cmd>(r#"{"cmd":"open","number":"+331"}"#).unwrap(), Cmd::Open { number: Some("+331".into()) });
        assert_eq!(serde_json::from_str::<Cmd>(r#"{"cmd":"open"}"#).unwrap(), Cmd::Open { number: None });
        assert_eq!(serde_json::from_str::<Out>(r#"{"event":"open"}"#).unwrap(), Out::Open { number: None });
        assert_eq!(serde_json::to_string(&Cmd::Window).unwrap(), r#"{"cmd":"window"}"#);
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

    #[test]
    fn setup_commands() {
        let addr: BtAddr = "00:11:22:33:44:55".parse().unwrap();
        assert_eq!(parse_cmd(r#"{"cmd":"powered","on":true}"#).ok(), Some(Cmd::Powered { on: true }));
        assert_eq!(parse_cmd(r#"{"cmd":"discover","on":false}"#).ok(), Some(Cmd::Discover { on: false }));
        assert_eq!(parse_cmd(r#"{"cmd":"confirm","ok":true}"#).ok(), Some(Cmd::Confirm { ok: true }));
        assert_eq!(parse_cmd(r#"{"cmd":"pair","addr":"00:11:22:33:44:55"}"#).ok(), Some(Cmd::Pair { addr: addr.clone() }));
        assert_eq!(parse_cmd(r#"{"cmd":"forget","addr":"00:11:22:33:44:55"}"#).ok(), Some(Cmd::Forget { addr: addr.clone() }));
        assert_eq!(parse_cmd(r#"{"cmd":"connect","addr":"00:11:22:33:44:55"}"#).ok(), Some(Cmd::Connect { addr }));
        for a in ["", "00:11:22:33:44", "00:11:22:33:44:GG", "../../x", "00:11:22:33:44:55:66"] {
            assert!(parse_cmd(&format!(r#"{{"cmd":"pair","addr":"{a}"}}"#)).is_err(), "{a}");
        }
        assert!(parse_cmd(r#"{"cmd":"powered"}"#).is_err());
    }

    #[test]
    fn export_validated() {
        assert_eq!(parse_cmd(r#"{"cmd":"export"}"#).ok(), Some(Cmd::Export { key: None }));
        assert_eq!(parse_cmd(r#"{"cmd":"export","key":"612345678"}"#).ok(), Some(Cmd::Export { key: Some("612345678".into()) }));
        let long = "1".repeat(65);
        for k in ["", "a\nb", long.as_str()] {
            assert!(parse_cmd(&format!(r#"{{"cmd":"export","key":"{k}"}}"#)).is_err(), "{k:?}");
        }
        // The folder is chosen by the daemon: an extra field is not a path.
        assert!(parse_cmd(r#"{"cmd":"export","path":"/etc/x"}"#).is_ok_and(|c| c == Cmd::Export { key: None }));
        let out = serde_json::to_string(&Out::Exported { path: "/tmp/x.json".into() }).unwrap();
        assert_eq!(serde_json::from_str::<Out>(&out).unwrap(), Out::Exported { path: "/tmp/x.json".into() });
        let out = serde_json::to_string(&Out::ExportFailed { msg: "x".into() }).unwrap();
        assert_eq!(serde_json::from_str::<Out>(&out).unwrap(), Out::ExportFailed { msg: "x".into() });
    }

    #[test]
    fn search_and_older_validated() {
        let s = parse_cmd(r#"{"cmd":"search","query":"Éte"}"#).ok();
        assert_eq!(s, Some(Cmd::Search { query: "Éte".into(), before: None }));
        let page = r#"{"cmd":"search","query":"ok","before":{"ts":"20261015T120000","id":7}}"#;
        assert!(matches!(parse_cmd(page), Ok(Cmd::Search { before: Some(Cursor { id: 7, .. }), .. })));
        let long = "é".repeat(MAX_QUERY + 1);
        for q in ["", "   ", "\u{200B}\u{202E}", long.as_str()] {
            assert!(parse_cmd(&format!(r#"{{"cmd":"search","query":"{q}"}}"#)).is_err(), "{q:?}");
        }
        assert!(parse_cmd(&format!(r#"{{"cmd":"search","query":"{}"}}"#, "é".repeat(MAX_QUERY))).is_ok());
        assert!(parse_cmd(r#"{"cmd":"search","query":"ok","before":{"ts":"x","id":1}}"#).is_err());
        assert!(parse_cmd(r#"{"cmd":"search","query":"ok","before":{"ts":"20261015T120000","id":-1}}"#).is_err());
        let older = |key: &str, ts: &str, limit: u32| format!(r#"{{"cmd":"older","key":"{key}","before":{{"ts":"{ts}","id":9223372036854775807}},"limit":{limit}}}"#);
        assert!(parse_cmd(&older("612345678", "20261015T120000", MAX_OLDER)).is_ok());
        assert!(parse_cmd(&older("612345678", "20261015T120000", MAX_OLDER + 1)).is_err());
        assert!(parse_cmd(&older("612345678", "20261015T120000", 0)).is_err());
        assert!(parse_cmd(&older("", "20261015T120000", 10)).is_err());
        assert!(parse_cmd(&older("k", "2026'; --", 10)).is_err());
        assert_eq!(Cursor::after("20261015T120000").id, i64::MAX);
    }

    #[test]
    fn conv_flags_round_trip() {
        let c = parse_cmd(r#"{"cmd":"conv_flags","key":"600000001","muted":true}"#).ok();
        assert_eq!(c, Some(Cmd::ConvFlags { key: "600000001".into(), flags: ConvFlags { muted: true, archived: false } }));
        assert!(parse_cmd(r#"{"cmd":"conv_flags","key":"","archived":true}"#).is_err());
        assert!(parse_cmd(&format!(r#"{{"cmd":"conv_flags","key":"{}"}}"#, "1".repeat(65))).is_err());
        let o = Out::Conv { key: "600000001".into(), flags: ConvFlags { muted: false, archived: true } };
        let back: Out = serde_json::from_str(&serde_json::to_string(&o).unwrap()).unwrap();
        assert!(matches!(back, Out::Conv { flags: ConvFlags { archived: true, muted: false }, .. }));
    }

    #[test]
    fn setup_events() {
        let addr: BtAddr = "00:11:22:33:44:55".parse().unwrap();
        let name = format!("\u{202E}iPhone\n{}", "x".repeat(100));
        let d = Nearby::new(addr.clone(), &name, false, false, true);
        assert_eq!(d.name.chars().count(), MAX_NAME);
        assert!(d.name.starts_with("iPhonex"), "{}", d.name);
        let Out::Found { devices } = Out::found(vec![d; MAX_FOUND + 3]) else { unreachable!() };
        assert_eq!(devices.len(), MAX_FOUND);
        assert_eq!(Out::confirm(addr.clone(), MAX_PASSKEY + 1), None);
        let c = Out::confirm(addr.clone(), 42).unwrap();
        let line = serde_json::to_string(&c).unwrap();
        assert_eq!(line, r#"{"event":"confirm","addr":"00:11:22:33:44:55","passkey":42}"#);
        assert_eq!(serde_json::from_str::<Out>(&line).unwrap(), c);
        let p = Out::Paired { addr, ok: false, error: Some("e".into()) };
        assert_eq!(serde_json::from_str::<Out>(&serde_json::to_string(&p).unwrap()).unwrap(), p);
        let a = Out::Adapter { present: true, powered: false, discovering: false };
        assert_eq!(serde_json::to_string(&a).unwrap(), r#"{"event":"adapter","present":true,"powered":false,"discovering":false}"#);
        // Internal event: cannot reach a client.
        assert!(serde_json::to_string(&Out::Flags { items: vec![] }).is_err());
        assert!(serde_json::from_str::<Out>(r#"{"event":"flags","items":[]}"#).is_err());
    }
}
