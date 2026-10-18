//! Interface state, UI-free (testable): conversations, call, links.
//! [`State::apply`] folds in an engine event and returns the effects to notify.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::addr::BtAddr;
use crate::engine::{CallKind, Cmd, ConvFlags, Cursor, Device, Hit, Item, MAX_QUERY, Nearby, Out, Profile};
use crate::history::SENT_ECHO_SECS;
use crate::hfp::{HfEvent, Indicator};
use crate::map::Sms;
use crate::numbering::{self, Id};
use crate::phone::{Query, fold};
use crate::settings::Settings;
use crate::stamp;
use crate::t;

pub use crate::phone::{matches, sms_peer};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Msg {
    pub text: String,
    /// MAP timestamp `YYYYMMDDTHHMMSS` (sortable as is).
    pub timestamp: String,
    pub outgoing: bool,
    /// Sent from the app (MAP transfer succeeded). iOS exposes an empty `sent`: otherwise replaced
    /// by the entry from the `sent` folder if it shows up.
    pub local: bool,
    /// Call line (direction, duration in seconds) rather than an SMS; empty `text`.
    pub call: Option<(CallKind, u64)>,
    /// Arrived unread (received SMS, missed call): counted while it is after the last read mark.
    pub fresh: bool,
    pub delivery: Delivery,
    /// Send request (`Pending` bubble): beyond [`PENDING_MAX`], it turns into a failure.
    pub asked: Option<Instant>,
}

/// A bubble left pending beyond that (command refused, link lost) turns into a failure.
pub const PENDING_MAX: std::time::Duration = std::time::Duration::from_secs(120);

/// Tracking of a send made from the app.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Delivery {
    #[default]
    Done,
    /// MAP transfer in progress.
    Pending,
    /// Transfer refused or failed: to be resent.
    Failed,
}

impl Msg {
    /// Designation of the item for a deletion.
    #[must_use]
    pub fn item(&self) -> Item {
        Item { timestamp: self.timestamp.clone(), text: self.text.clone(), outgoing: self.outgoing, call: self.call.is_some() }
    }

    fn is(&self, i: &Item) -> bool {
        self.timestamp == i.timestamp && self.text == i.text && self.outgoing == i.outgoing && self.call.is_some() == i.call
    }
}

#[derive(Debug, Clone)]
pub struct Conv {
    /// Grouping key (see [`numbering::key`]).
    pub key: String,
    pub number: String,
    pub name: String,
    pub msgs: Vec<Msg>,
    pub unread: usize,
}

impl Conv {
    #[must_use]
    pub fn last_stamp(&self) -> &str {
        self.msgs.last().map_or("", |m| m.timestamp.as_str())
    }

    /// Last call, if any.
    #[must_use]
    pub fn last_call(&self) -> Option<&Msg> {
        self.msgs.iter().rev().find(|m| m.call.is_some())
    }

    /// Belongs in the SMS list: at least one SMS, or a number able to receive one (not a sure
    /// landline of the `home` country, see [`numbering::is_landline`]).
    #[must_use]
    pub fn textable(&self, home: Option<Id>) -> bool {
        self.msgs.iter().any(|m| m.call.is_none()) || !numbering::is_landline(&self.number, home)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Incoming,
    Outgoing,
    Active,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Call {
    pub phase: Phase,
    pub number: Option<String>,
    /// Name of the iPhone contact for `number`.
    pub name: Option<String>,
    /// Start of the conversation (switch to `Active`).
    pub since: Option<Instant>,
    /// PC microphone muted by the daemon.
    pub muted: bool,
    /// The main call itself is on hold.
    pub hold: bool,
    /// Other call put on hold; second call ringing (empty number if hidden).
    pub held: Option<String>,
    pub waiting: Option<String>,
    /// HFP `call` indicator (the name mirrors the protocol).
    #[allow(clippy::struct_field_names)]
    call: bool,
    setup: u8,
    /// Call placed from the iPhone (went through `Outgoing`).
    dialed: bool,
    /// Start (MAP format), for the history.
    started: String,
}

/// Number as shown: "Hidden number" when absent or empty.
#[must_use]
pub fn shown_number(n: Option<&str>) -> &str {
    match n {
        Some(n) if !n.is_empty() => n,
        _ => t!("Hidden number"),
    }
}

impl Call {
    /// Number able to receive an SMS (separators removed, valid, not a landline): replies to a declined call.
    #[must_use]
    pub fn sms_number(&self, home: Option<Id>) -> Option<String> {
        let n = crate::phone::strip_separators(self.number.as_deref()?);
        crate::bmsg::validate_number(&n).ok()?;
        (!numbering::is_landline(&n, home)).then_some(n)
    }

    /// Name from the phonebook and number joined by `sep`, otherwise the number alone (or "Hidden number").
    #[must_use]
    pub fn caller(&self, sep: &str) -> String {
        match (self.name.as_deref(), self.number.as_deref()) {
            (Some(name), Some(n)) if !n.is_empty() => format!("{name}{sep}{n}"),
            (_, n) => shown_number(n).to_owned(),
        }
    }

    /// No call in progress.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.phase == Phase::Idle
    }

    /// What is displayed, without the raw HFP indicators.
    fn view(&self) -> Call {
        Call { call: false, setup: 0, dialed: false, started: String::new(), ..self.clone() }
    }
}

/// SMS replies of the incoming call bubble: `number` can receive a text (valid, not a landline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decline {
    pub number: String,
    /// Sent by "Decline + SMS" (first quick reply); `None`: only "Decline and write".
    pub text: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    /// `reply`: number the notification's "Reply" action opens ([`Cmd::Open`](crate::engine::Cmd::Open)), `None`
    /// when the sender cannot be answered (alphanumeric, short code) or for a summary.
    Sms { from: String, text: String, reply: Option<String> },
    /// Call in progress (ringing, talking, double call): view to display, emitted on every change.
    Call(Call),
    /// Incoming [`Effect::Call`] as the daemon passes it to the bubble, with what declining can text back.
    Ring(Call, Option<Decline>),
    /// End of call: what the history records.
    CallEnd(Ended),
    /// Closes the SMS notifications still displayed (device change).
    CloseAll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub number: String,
    pub timestamp: String,
    pub kind: CallKind,
    pub secs: u64,
}

/// Settings as the daemon publishes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefs {
    pub settings: Settings,
    pub langs: Vec<String>,
    pub devices: Vec<Device>,
    /// `None`: none configured yet (setup mode).
    pub device: Option<BtAddr>,
    /// Country numbers are read with.
    pub home: Option<Id>,
    /// Country chosen without a setting ("Automatic").
    pub auto: Option<Id>,
}

/// Bluetooth adapter as the daemon publishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adapter {
    pub present: bool,
    pub powered: bool,
    pub discovering: bool,
}

/// Adapter and pairing, read by the setup wizard.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pairing {
    /// `None`: not received yet.
    pub adapter: Option<Adapter>,
    /// Devices seen by the last search.
    pub found: Vec<Nearby>,
    /// Passkey awaiting confirmation.
    pub confirm: Option<(BtAddr, u32)>,
    /// Result of the last pairing: `Err` holds the daemon's reason.
    pub paired: Option<(BtAddr, Result<(), String>)>,
}

impl Pairing {
    /// Command sent: a new pairing forgets the previous result, an answer withdraws the question
    /// (neither can then be shown stale nor sent twice).
    pub fn sent(&mut self, cmd: &Cmd) {
        match cmd {
            Cmd::Pair { .. } => self.paired = None,
            Cmd::Confirm { .. } => self.confirm = None,
            _ => {}
        }
    }
}

/// Revision of a [`State`]'s conversations and contacts, unique in the process: two states never
/// share one, so a cache keyed by it (thread fold, contact search) cannot outlive its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rev(u64);

impl Rev {
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl Default for Rev {
    fn default() -> Self {
        Self::next()
    }
}

/// Search through the whole history (done by the daemon), one page after the other.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hits {
    /// Query sent; a reply for another one is stale.
    pub query: String,
    /// Newest first.
    pub items: Vec<Hit>,
    pub next: Option<Cursor>,
    /// A page is on its way.
    pub busy: bool,
}

/// Message to bring into view (search result), highlighted for [`FOCUS_FOR`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    pub key: String,
    pub timestamp: String,
    pub text: String,
    pub outgoing: bool,
    pub at: Instant,
}

impl Focus {
    /// `m` is the message to bring into view.
    #[must_use]
    pub fn is(&self, m: &Msg) -> bool {
        m.call.is_none() && m.timestamp == self.timestamp && m.outgoing == self.outgoing && m.text == self.text
    }
}

/// Highlight length of a search result reached.
pub const FOCUS_FOR: std::time::Duration = std::time::Duration::from_secs(2);
/// Messages asked per "older" page.
pub const OLDER_PAGE: u32 = 50;
/// Pages loaded at most to reach a search result, then given up.
const JUMP_PAGES: u32 = 50;
/// Longest wait for a page of search results or older messages: past it the reply is taken as lost.
pub const REPLY_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Paging of a conversation's older messages.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Older {
    More(Cursor),
    /// Page before this cursor asked at this instant.
    Asked(Cursor, Instant),
    End,
}

/// Search result being reached through "older" pages.
#[derive(Debug, Clone)]
struct Jump {
    hit: Hit,
    pages: u32,
}

#[derive(Default)]
pub struct State {
    /// Sorted by last message, most recent first.
    pub convs: Vec<Conv>,
    pub map_up: bool,
    pub hfp_up: bool,
    pub slc: bool,
    /// Last contacts synchronisation: `None` before the first attempt.
    pub pbap: Option<bool>,
    /// Phone battery level (0..=100), `None` if unknown.
    pub battery: Option<u8>,
    pub call: Call,
    pub error: Option<String>,
    /// Report of a command (purge…), displayed without alarm.
    pub info: Option<String>,
    /// Settings published by the daemon (`None`: not received yet).
    pub settings: Option<Prefs>,
    pub pairing: Pairing,
    /// `error` comes from a link (`Out::Error`): cleared when a link comes back.
    link_error: bool,
    handles: HashSet<String>,
    /// iPhone contacts: number key → name.
    names: HashMap<String, String>,
    /// Full contacts (name, number, folded name), sorted by name: lookup of a correspondent.
    book: Vec<(String, String, String)>,
    /// Read up to (MAP timestamp), per conversation; recorded by the daemon.
    reads: HashMap<String, String>,
    /// Muted / archived conversations (absent: neither); recorded by the daemon.
    flags: HashMap<String, ConvFlags>,
    /// Changes with every change made through the methods (not through direct access to `convs`).
    rev: Rev,
    pub hits: Hits,
    /// When the page of `hits` on its way was asked.
    hits_asked: Option<Instant>,
    pub focus: Option<Focus>,
    /// Per conversation key; absent: not asked yet.
    older: HashMap<String, Older>,
    jump: Option<Jump>,
    /// Commands to send to the daemon, see [`State::take_cmds`].
    out: Vec<Cmd>,
}

/// Unread: arrived unread after the last recorded read mark.
fn unread_count(c: &Conv, read_until: Option<&String>) -> usize {
    unread_where(c, read_until, |_| true)
}

fn unread_where(c: &Conv, read_until: Option<&String>, keep: impl Fn(&Msg) -> bool) -> usize {
    c.msgs.iter().filter(|m| m.fresh && read_until.is_none_or(|u| m.timestamp > *u) && keep(m)).count()
}

impl State {
    #[must_use]
    pub fn rev(&self) -> Rev {
        self.rev
    }

    /// Country numbers are read with, as published by the daemon.
    #[must_use]
    pub fn home(&self) -> Option<Id> {
        self.settings.as_ref().and_then(|p| p.home)
    }

    /// Grouping key of `number` (see [`numbering::key`]).
    #[must_use]
    pub fn key(&self, number: &str) -> String {
        numbering::key(number, self.home())
    }

    /// Contacts matching `q` (name, number), at most `max`, kept by `keep(number, conversation)`.
    #[must_use]
    pub fn search(&self, q: &str, max: usize, keep: impl Fn(&str, Option<&Conv>) -> bool) -> Vec<(&str, &str)> {
        if q.trim().is_empty() {
            return vec![];
        }
        let q = Query::new(q);
        self.book
            .iter()
            .filter(|(_, num, folded)| q.hits(folded, num) && keep(num, self.conv(&self.key(num))))
            .take(max)
            .map(|(a, b, _)| (a.as_str(), b.as_str()))
            .collect()
    }

    pub fn apply(&mut self, o: Out) -> Vec<Effect> {
        self.rev = Rev::next();
        match o {
            Out::Sms { sms, folder, new } => self.on_sms(sms, folder.is_sent(), new),
            Out::Sent { to, text, ok: true, timestamp, .. } => {
                self.take_unsent(&to, &text, true);
                // Already came back through the `sent` folder (phone clock): no duplicate.
                let echo = self.conv(&self.key(&to)).is_some_and(|c| {
                    let t = stamp::secs(&timestamp);
                    c.msgs.iter().any(|m| {
                        m.outgoing && !m.local && m.call.is_none() && m.text == text
                            && stamp::secs(&m.timestamp).zip(t).is_some_and(|(a, b)| (a - b).abs() <= SENT_ECHO_SECS)
                    })
                });
                if !echo {
                    self.push(&to, "", Msg { text, timestamp, outgoing: true, local: true, ..Msg::default() }, false);
                }
                vec![]
            }
            Out::CallLog { number, timestamp, kind, secs, read } => {
                let msg = Msg { timestamp, outgoing: kind == CallKind::Out, call: Some((kind, secs)), ..Msg::default() };
                self.push(&number, "", msg, kind == CallKind::Missed && !read);
                vec![]
            }
            Out::Contacts { names } => {
                self.names = names.iter().map(|(num, name)| (self.key(num), name.clone())).collect();
                self.book = names.into_iter().map(|(num, name)| (fold(&name), name, num)).map(|(f, name, num)| (name, num, f)).collect();
                self.book.sort_by(|a, b| a.2.cmp(&b.2));
                for c in &mut self.convs {
                    if let Some(n) = self.names.get(&c.key) {
                        c.name = n.clone();
                    }
                }
                self.call.name = self.call.number.as_deref().and_then(|n| self.name_of(n));
                vec![]
            }
            Out::Muted { on } => self.set_muted(on),
            Out::Read { key, until } => {
                let r = self.reads.entry(key.clone()).or_default();
                if until > *r {
                    *r = until;
                }
                if let Some(c) = self.convs.iter_mut().find(|c| c.key == key) {
                    c.unread = unread_count(c, self.reads.get(&key));
                }
                vec![]
            }
            Out::Sent { to, text, error, timestamp, .. } => {
                // Banner only for a send tracked here (not for an old failure replayed on connection).
                if self.take_unsent(&to, &text, true) {
                    self.error = Some(t!("SMS not sent: {0}", error.as_deref().unwrap_or(t!("unknown error"))));
                    self.link_error = false;
                }
                let timestamp = if timestamp.is_empty() { stamp::now() } else { timestamp };
                self.push(&to, "", Msg { text, timestamp, outgoing: true, local: true, delivery: Delivery::Failed, ..Msg::default() }, false);
                vec![]
            }
            Out::Deleted { key, item } => {
                self.delete(&key, item.as_ref());
                vec![]
            }
            Out::Conv { key, flags } => {
                if flags == ConvFlags::default() {
                    self.flags.remove(&key);
                } else {
                    self.flags.insert(key, flags);
                }
                vec![]
            }
            Out::Status { link: Profile::Pbap, up } => {
                self.pbap = Some(up);
                vec![]
            }
            Out::Status { up: true, .. } if self.link_error => {
                self.error = None;
                self.link_error = false;
                self.apply(o)
            }
            Out::Battery { percent } => {
                self.battery = percent.filter(|p| *p <= 100);
                vec![]
            }
            Out::Status { link: Profile::Map, up } => {
                self.map_up = up;
                vec![]
            }
            Out::Status { link: Profile::Hfp, up } => {
                self.hfp_up = up;
                if up {
                    return vec![];
                }
                self.slc = false;
                self.end_call()
            }
            Out::Call(ev) => {
                let before = self.call.view();
                let fx = self.on_call(ev);
                self.call_changed(&before, fx)
            }
            Out::Error { msg } => {
                self.error = Some(msg);
                self.link_error = true;
                vec![]
            }
            Out::Info { msg } => {
                self.info = Some(msg);
                vec![]
            }
            Out::Settings { settings, langs, devices, device, home, auto } => {
                let (home, auto) = (home.as_deref().and_then(numbering::parse_code), auto.as_deref().and_then(numbering::parse_code));
                self.settings = Some(Prefs { settings, langs, devices, device, home, auto });
                vec![]
            }
            // Call in progress kept: only the history is cleared.
            Out::Cleared => {
                self.convs.clear();
                self.reads.clear();
                self.handles.clear();
                self.older.clear();
                (self.hits, self.focus, self.jump) = (Hits::default(), None, None);
                vec![]
            }
            Out::Adapter { present, powered, discovering } => {
                self.pairing.adapter = Some(Adapter { present, powered, discovering });
                vec![]
            }
            Out::Found { devices } => {
                self.pairing.found = devices;
                vec![]
            }
            Out::Confirm { addr, passkey } => {
                self.pairing.confirm = Some((addr, passkey));
                vec![]
            }
            Out::Paired { addr, ok, error } => {
                self.pairing.confirm = None;
                let r = if ok { Ok(()) } else { Err(error.unwrap_or_default()) };
                self.pairing.paired = Some((addr, r));
                vec![]
            }
            Out::Hits { query, items, next } => {
                if query == self.hits.query && self.hits.busy {
                    self.hits.items.extend(items);
                    (self.hits.next, self.hits.busy) = (next, false);
                }
                vec![]
            }
            Out::Older { key, items, next } => {
                self.on_older(&key, items, next);
                vec![]
            }
            Out::Exported { path } => {
                self.info = Some(t!("History exported to {0}", path));
                vec![]
            }
            // Never leaves the daemon.
            Out::Flags { .. } | Out::Open { .. } => vec![],
        }
    }

    /// Commands queued for the daemon (search, older pages), in order.
    pub fn take_cmds(&mut self) -> Vec<Cmd> {
        std::mem::take(&mut self.out)
    }

    /// Searches `query` through the whole history (from 2 characters, cut to [`MAX_QUERY`]); a new
    /// query drops the previous results.
    pub fn search_all(&mut self, query: &str) {
        let query: String = query.trim().chars().take(MAX_QUERY).collect();
        if query == self.hits.query {
            return;
        }
        self.rev = Rev::next();
        let short = query.chars().count() < 2;
        self.hits = Hits { busy: !short, ..Hits::default() };
        self.hits_asked = (!short).then(Instant::now);
        if !short {
            self.out.push(Cmd::Search { query: query.clone(), before: None });
        }
        self.hits.query = query;
    }

    /// Asks for the following page of search results, if any and none is on its way.
    pub fn more_hits(&mut self) {
        if let Some(c) = self.hits.next.clone().filter(|_| !self.hits.busy) {
            (self.hits.busy, self.hits_asked) = (true, Some(Instant::now()));
            self.out.push(Cmd::Search { query: self.hits.query.clone(), before: Some(c) });
        }
    }

    /// Conversation `key` may have older messages in the history (unknown until asked).
    #[must_use]
    pub fn has_older(&self, key: &str) -> bool {
        self.older.get(key).is_none_or(|o| matches!(o, Older::More(_)))
    }

    /// Asks for the page of messages before the oldest one shown in conversation `key`.
    pub fn older(&mut self, key: &str) {
        let before = match self.older.get(key) {
            Some(Older::More(c)) => c.clone(),
            Some(Older::Asked(..) | Older::End) => return,
            None => {
                let oldest = self.conv(key).and_then(|c| c.msgs.iter().find(|m| m.call.is_none()));
                Cursor::after(oldest.map_or("99991231T235959", |m| m.timestamp.as_str()))
            }
        };
        self.older.insert(key.to_owned(), Older::Asked(before.clone(), Instant::now()));
        self.out.push(Cmd::Older { key: key.to_owned(), before, limit: OLDER_PAGE });
    }

    /// Gives up the pages asked [`REPLY_WAIT`] ago or more without a reply (lost on the way), so that
    /// they can be asked again; a search result being reached through them is given up too.
    /// Returns how long until the next page on its way expires, if any.
    pub fn expire_reads(&mut self, now: Instant) -> Option<std::time::Duration> {
        let mut soonest: Option<std::time::Duration> = None;
        let mut expired = |at: Instant| {
            let left = REPLY_WAIT.saturating_sub(now.saturating_duration_since(at));
            if left.is_zero() {
                return true;
            }
            soonest = Some(soonest.map_or(left, |s| s.min(left)));
            false
        };
        let mut lost = false;
        if let Some(at) = self.hits_asked.filter(|_| self.hits.busy)
            && expired(at)
        {
            (self.hits.busy, self.hits_asked, lost) = (false, None, true);
        }
        for o in self.older.values_mut() {
            if let Older::Asked(c, at) = o
                && expired(*at)
            {
                *o = Older::More(c.clone());
                lost = true;
            }
        }
        if lost {
            self.rev = Rev::next();
            // Pending only while its page is on its way.
            if self.jump.as_ref().is_some_and(|j| !matches!(self.older.get(&j.hit.key), Some(Older::Asked(..)))) {
                self.jump = None;
            }
            self.error = Some(t!("no reply from the daemon").to_owned());
        }
        soonest
    }

    /// Opens on search result `hit`: brought into view once present, older pages loaded meanwhile.
    pub fn jump(&mut self, hit: &Hit) {
        self.rev = Rev::next();
        self.focus = None;
        self.jump = Some(Jump { hit: hit.clone(), pages: 0 });
        self.reach();
    }

    /// Next step towards the pending search result: focus if loaded, else one more page or give up.
    fn reach(&mut self) {
        let Some(j) = &self.jump else { return };
        let key = j.hit.key.clone();
        let target = Focus { key: key.clone(), timestamp: j.hit.timestamp.clone(), text: j.hit.text.clone(), outgoing: j.hit.outgoing, at: Instant::now() };
        if self.conv(&key).is_some_and(|c| c.msgs.iter().any(|m| target.is(m))) {
            (self.focus, self.jump) = (Some(target), None);
        } else if matches!(self.older.get(&key), Some(Older::Asked(..))) {
            // Wait for the page on its way.
        } else if j.pages < JUMP_PAGES && self.has_older(&key) {
            if let Some(j) = self.jump.as_mut() {
                j.pages += 1;
            }
            self.older(&key);
        } else {
            self.jump = None;
            self.info = Some(t!("Message not found in the history").to_owned());
        }
    }

    /// Merges a page of older messages, without duplicates (already shown, or same date, direction, text).
    fn on_older(&mut self, key: &str, items: Vec<Hit>, next: Option<Cursor>) {
        self.older.insert(key.to_owned(), next.map_or(Older::End, Older::More));
        for h in items {
            if !self.handles.insert(format!("db{}", h.id)) {
                continue;
            }
            let msg = Msg { text: h.text, timestamp: h.timestamp, outgoing: h.outgoing, ..Msg::default() };
            let same = |m: &Msg| m.call.is_none() && m.timestamp == msg.timestamp && m.outgoing == msg.outgoing && m.text == msg.text;
            if !self.conv(&h.key).is_some_and(|c| c.msgs.iter().any(same)) {
                self.push(&h.number, "", msg, false);
            }
        }
        if self.jump.as_ref().is_some_and(|j| j.hit.key == key) {
            self.reach();
        }
    }

    /// Bubble shown as soon as the send is requested, pending the transfer result.
    /// A resend replaces the failed bubble with the same text.
    pub fn pending(&mut self, to: &str, text: &str) {
        self.rev = Rev::next();
        self.take_unsent(to, text, false);
        let msg = Msg { text: text.to_owned(), timestamp: stamp::now(), outgoing: true, local: true, delivery: Delivery::Pending, asked: Some(Instant::now()), ..Msg::default() };
        self.push(to, "", msg, false);
    }

    /// Bubbles left pending too long, or all of them (`all`: link to the daemon lost): failed.
    /// `true` if some are still pending (animation to keep running).
    pub fn expire_pending(&mut self, all: bool) -> bool {
        let mut left = false;
        for m in self.convs.iter_mut().flat_map(|c| c.msgs.iter_mut()).filter(|m| m.delivery == Delivery::Pending) {
            if all || m.asked.is_none_or(|t| t.elapsed() > PENDING_MAX) {
                m.delivery = Delivery::Failed;
                self.rev = Rev::next();
            } else {
                left = true;
            }
        }
        left
    }

    /// Removes the failed bubble (and, if `pending`, the pending one first) with this text, replaced by the
    /// send result or by a resend; `true` if a pending bubble was removed.
    fn take_unsent(&mut self, to: &str, text: &str, pending: bool) -> bool {
        let key = self.key(to);
        let mut was_pending = false;
        if let Some(c) = self.convs.iter_mut().find(|c| c.key == key) {
            let find = |d: Delivery| c.msgs.iter().position(|m| m.delivery == d && m.text == text);
            let i = if pending { find(Delivery::Pending).inspect(|_| was_pending = true).or_else(|| find(Delivery::Failed)) } else { find(Delivery::Failed) };
            if let Some(i) = i {
                c.msgs.remove(i);
            }
        }
        self.convs.retain(|c| !c.msgs.is_empty());
        was_pending
    }

    /// Removes an item, or the whole conversation (`None`); an emptied conversation disappears.
    pub fn delete(&mut self, key: &str, item: Option<&Item>) {
        self.rev = Rev::next();
        if let Some(c) = self.convs.iter_mut().find(|c| c.key == key)
            && let Some(item) = item
        {
            if let Some(i) = c.msgs.iter().position(|m| m.is(item)) {
                c.msgs.remove(i);
            }
            c.unread = unread_count(c, self.reads.get(key));
            if !c.msgs.is_empty() {
                self.convs.sort_by(|a, b| b.last_stamp().cmp(a.last_stamp()));
                return;
            }
        }
        self.convs.retain(|c| c.key != key);
        // As the daemon: its flags go with the whole conversation, not with its last item.
        if item.is_none() {
            self.flags.remove(key);
        }
    }

    /// Muted / archived state of conversation `key`.
    #[must_use]
    pub fn flags(&self, key: &str) -> ConvFlags {
        self.flags.get(key).copied().unwrap_or_default()
    }

    #[must_use]
    pub fn conv(&self, key: &str) -> Option<&Conv> {
        self.convs.iter().find(|c| c.key == key)
    }

    /// Date up to which conversation `key` is read.
    #[must_use]
    pub fn read_until(&self, key: &str) -> Option<&str> {
        self.reads.get(key).map(String::as_str)
    }

    /// Unread calls of `c` (badge of the Calls tab).
    #[must_use]
    pub fn unread_calls(&self, c: &Conv) -> usize {
        if c.unread == 0 { 0 } else { unread_where(c, self.reads.get(&c.key), |m| m.call.is_some()) }
    }

    /// Conversation displayed: read up to its last item. `Some(until)` if something was unread
    /// (to be passed to the daemon, which records it).
    /// `calls_only`: only the calls are displayed; nothing is marked while an SMS is unread.
    pub fn mark_read(&mut self, key: &str, calls_only: bool) -> Option<String> {
        let c = self.convs.iter().find(|c| c.key == key && c.unread > 0)?;
        let read_until = self.reads.get(key);
        if calls_only && unread_where(c, read_until, |m| m.call.is_none()) > 0 {
            return None;
        }
        // Excluding the pending bubble: the daemon does not know it and would refuse a date past its last item.
        let until = c.msgs.iter().rev().find(|m| m.delivery != Delivery::Pending)?.timestamp.clone();
        self.apply(Out::Read { key: key.to_owned(), until: until.clone() });
        Some(until)
    }

    fn on_sms(&mut self, sms: Sms, sent: bool, new: bool) -> Vec<Effect> {
        if !self.handles.insert(sms.handle.clone()) {
            return vec![];
        }
        let (number, name) = sms_peer(&sms, sent);
        let (number, name) = (number.to_owned(), name.to_owned());
        let msg = Msg { text: sms.text.clone(), timestamp: sms.timestamp.clone(), outgoing: sent, ..Msg::default() };
        if sent {
            // If the message sent from the app comes back through polling, it replaces the local entry.
            let key = self.key(&number);
            if let Some(c) = self.convs.iter_mut().find(|c| c.key == key)
                && let Some(i) = c.msgs.iter().position(|m| m.local && m.text == msg.text)
            {
                c.msgs.remove(i);
            }
        }
        self.push(&number, &name, msg, !sent && !sms.read);
        if new && !sent && !self.flags(&self.key(&number)).muted {
            let reply = Some(crate::phone::strip_separators(&number)).filter(|n| crate::bmsg::validate_number(n).is_ok());
            let from = self.name_of(&number).or((!sms.sender.is_empty()).then_some(sms.sender)).unwrap_or(number);
            return vec![Effect::Sms { from, text: sms.text, reply }];
        }
        vec![]
    }

    /// Contact name for this number.
    #[must_use]
    pub fn name_of(&self, number: &str) -> Option<String> {
        self.names.get(&self.key(number)).cloned()
    }

    fn push(&mut self, number: &str, name: &str, msg: Msg, unread: bool) {
        let key = self.key(number);
        let known = self.names.get(&key);
        let name = known.map_or(name, String::as_str);
        let i = if let Some(i) = self.convs.iter().position(|c| c.key == key) {
            i
        } else {
            self.convs.push(Conv { key, number: number.to_owned(), name: String::new(), msgs: vec![], unread: 0 });
            self.convs.len() - 1
        };
        let c = &mut self.convs[i];
        // The contact name wins over the MAP name.
        if (c.name.is_empty() || known.is_some()) && !name.is_empty() && name != number {
            name.clone_into(&mut c.name);
        }
        // Messages sorted by date: binary search, after those with the same date.
        let at = c.msgs.partition_point(|m| m.timestamp <= msg.timestamp);
        c.msgs.insert(at, Msg { fresh: unread, ..msg });
        c.unread = unread_count(c, self.reads.get(&c.key));
        // Only this conversation is out of place: the stable sort detects the sorted runs, linear cost.
        self.convs.sort_by(|a, b| b.last_stamp().cmp(a.last_stamp()));
    }

    /// Mutes or unmutes the current call's microphone (state only; audio is handled by the daemon).
    pub fn set_muted(&mut self, on: bool) -> Vec<Effect> {
        if self.call.is_idle() {
            return vec![];
        }
        let before = self.call.view();
        self.call.muted = on;
        self.call_changed(&before, vec![])
    }

    fn call_changed(&self, before: &Call, mut fx: Vec<Effect>) -> Vec<Effect> {
        let now = self.call.view();
        if !now.is_idle() && now != *before {
            fx.push(Effect::Call(now));
        }
        fx
    }

    fn on_call(&mut self, ev: HfEvent) -> Vec<Effect> {
        let home = self.home();
        let c = &mut self.call;
        match ev {
            HfEvent::SlcReady => {
                self.slc = true;
                vec![]
            }
            HfEvent::IncomingCall | HfEvent::Ring if c.is_idle() => {
                c.phase = Phase::Incoming;
                c.started = stamp::now();
                vec![]
            }
            HfEvent::Clip { number } => {
                c.name = self.names.get(&numbering::key(&number, home)).cloned();
                c.number = Some(number);
                if c.is_idle() {
                    c.phase = Phase::Incoming;
                    c.started = stamp::now();
                }
                vec![]
            }
            HfEvent::Lines { hold, held, waiting } => {
                (c.hold, c.held, c.waiting) = (hold, held, waiting);
                vec![]
            }
            HfEvent::Indicator { name: Indicator::Call, value } => {
                c.call = value == 1;
                if c.call {
                    if c.phase != Phase::Active {
                        if c.started.is_empty() {
                            c.started = stamp::now();
                        }
                        c.phase = Phase::Active;
                        c.since = Some(Instant::now());
                    }
                    return vec![];
                }
                if c.setup == 0 { self.end_call() } else { vec![] }
            }
            HfEvent::Indicator { name: Indicator::CallSetup, value } => {
                c.setup = value;
                match value {
                    0 if !c.call => self.end_call(),
                    2 | 3 if !c.call => {
                        if c.is_idle() {
                            c.started = stamp::now();
                        }
                        c.phase = Phase::Outgoing;
                        c.dialed = true;
                        vec![]
                    }
                    _ => vec![],
                }
            }
            _ => vec![],
        }
    }

    fn end_call(&mut self) -> Vec<Effect> {
        let c = std::mem::take(&mut self.call);
        if c.is_idle() {
            return vec![];
        }
        let kind = match (c.dialed, c.since) {
            (true, _) => CallKind::Out,
            (false, Some(_)) => CallKind::In,
            (false, None) => CallKind::Missed,
        };
        let secs = c.since.map_or(0, |t| t.elapsed().as_secs());
        let started = if c.started.is_empty() { stamp::now() } else { c.started };
        vec![Effect::CallEnd(Ended { number: c.number.unwrap_or_default(), timestamp: started, kind, secs })]
    }
}

/// Read marks reported by the iPhone (inbox listing): per conversation, the date of the latest received SMS
/// such that it and all the earlier ones are read. A conversation whose oldest listed SMS is unread gives nothing.
#[must_use]
pub fn phone_reads(items: &[Sms], home: Option<Id>) -> Vec<(String, String)> {
    let mut by_key: HashMap<String, Vec<&Sms>> = HashMap::new();
    for sms in items {
        by_key.entry(numbering::key(sms_peer(sms, false).0, home)).or_default().push(sms);
    }
    let mut out: Vec<_> = by_key
        .into_iter()
        .filter_map(|(key, mut list)| {
            list.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
            let until = list.iter().take_while(|s| s.read).last()?.timestamp.clone();
            Some((key, until))
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Folder;

    fn inbox(addr: &str, ts: &str, read: bool) -> Sms {
        Sms { address: addr.into(), timestamp: ts.into(), read, ..Default::default() }
    }

    #[test]
    fn reads_reported_by_the_phone() {
        assert!(phone_reads(&[], None).is_empty());
        let items = [
            inbox("0612345678", "20261015T120000", false),
            inbox("0612345678", "20261015T100000", true),
            inbox("0612345678", "20261015T110000", true),
            inbox("0698765432", "20261015T090000", false),
            inbox("0698765432", "20261015T100000", true),
        ];
        // Stops at the first unread one; an unread oldest SMS gives nothing.
        assert_eq!(phone_reads(&items, None), [("612345678".to_owned(), "20261015T110000".to_owned())]);
    }

    fn hit(id: i64, ts: &str, text: &str) -> Hit {
        Hit { id, key: "600000001".into(), number: "+33600000001".into(), timestamp: ts.into(), text: text.into(), outgoing: false }
    }

    #[test]
    fn search_pages_and_stale_replies() {
        let mut st = State::default();
        st.search_all("e");
        assert!(st.take_cmds().is_empty(), "under 2 characters: nothing asked");
        st.search_all(" été ");
        assert_eq!(st.take_cmds(), [Cmd::Search { query: "été".into(), before: None }]);
        st.search_all("été");
        assert!(st.take_cmds().is_empty(), "same query: not asked again");
        drop(st.apply(Out::Hits { query: "ét".into(), items: vec![hit(9, "20261015T100000", "x")], next: None }));
        assert!(st.hits.items.is_empty() && st.hits.busy, "reply for another query ignored");
        let next = Some(Cursor { ts: "20261015T100000".into(), id: 1 });
        drop(st.apply(Out::Hits { query: "été".into(), items: vec![hit(1, "20261015T100000", "été")], next: next.clone() }));
        assert_eq!(st.hits.items.len(), 1);
        st.more_hits();
        st.more_hits();
        assert_eq!(st.take_cmds(), [Cmd::Search { query: "été".into(), before: next }], "one page at a time");
        drop(st.apply(Out::Hits { query: "été".into(), items: vec![hit(0, "20261014T100000", "été 2")], next: None }));
        assert_eq!(st.hits.items.len(), 2);
        st.more_hits();
        assert!(st.take_cmds().is_empty(), "last page reached");
        st.search_all("x".repeat(MAX_QUERY + 5).as_str());
        assert!(matches!(&st.take_cmds()[..], [Cmd::Search { query, .. }] if query.len() == MAX_QUERY));
    }

    #[test]
    fn older_merges_and_jump_reaches_the_hit() {
        let mut st = State::default();
        let sms = Sms { handle: "db5".into(), address: "+33600000001".into(), timestamp: "20261015T120000".into(), text: "recent".into(), ..Default::default() };
        drop(st.apply(Out::Sms { sms, folder: Folder::Inbox, new: false }));
        assert!(st.has_older("600000001"));
        // Target not loaded: older pages asked one after the other, from the oldest message shown.
        let target = hit(1, "20261001T080000", "ancient");
        st.jump(&target);
        let first = Cmd::Older { key: "600000001".into(), before: Cursor::after("20261015T120000"), limit: OLDER_PAGE };
        assert_eq!(st.take_cmds(), [first]);
        assert!(!st.has_older("600000001"), "page on its way");
        let c2 = Cursor { ts: "20261010T000000".into(), id: 3 };
        // Already shown (same handle, or same date/direction/text): not duplicated.
        let page = vec![hit(5, "20261015T120000", "recent"), hit(4, "20261015T120000", "recent"), hit(3, "20261010T000000", "mid")];
        drop(st.apply(Out::Older { key: "600000001".into(), items: page, next: Some(c2.clone()) }));
        let texts: Vec<_> = st.conv("600000001").unwrap().msgs.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["mid", "recent"]);
        assert_eq!(st.take_cmds(), [Cmd::Older { key: "600000001".into(), before: c2, limit: OLDER_PAGE }]);
        assert!(st.focus.is_none());
        drop(st.apply(Out::Older { key: "600000001".into(), items: vec![target.clone()], next: None }));
        let f = st.focus.clone().unwrap();
        assert!(f.key == "600000001" && f.text == "ancient");
        assert!(!st.has_older("600000001"), "start of the history reached");
        // Already loaded: focused at once, nothing asked.
        st.jump(&target);
        assert!(st.take_cmds().is_empty() && st.focus.is_some());
        // Not found before the start of the history: given up with a notice.
        st.jump(&hit(2, "20261002T000000", "gone"));
        assert!(st.focus.is_none() && st.take_cmds().is_empty() && st.info.is_some());
        // Unknown conversation: newest page asked first.
        let mut st = State::default();
        st.older("700000000");
        assert!(matches!(&st.take_cmds()[..], [Cmd::Older { before, .. }] if *before == Cursor::after("99991231T235959")));
    }

    #[test]
    fn lost_reads_expire() {
        let mut st = State::default();
        st.search_all("été");
        drop(st.take_cmds());
        let now = Instant::now();
        assert!(st.expire_reads(now).is_some_and(|w| w <= REPLY_WAIT), "search on its way");
        assert!(st.hits.busy && st.error.is_none());
        assert!(st.expire_reads(now + REPLY_WAIT).is_none());
        assert!(!st.hits.busy && st.hits.query == "été" && st.error.is_some(), "search given up, kept");
        // Lost older page during a jump: page asked again later, jump given up.
        let mut st = State::default();
        st.jump(&hit(1, "20261001T080000", "ancient"));
        let first = st.take_cmds();
        assert!(!st.has_older("600000001") && st.jump.is_some());
        assert!(st.expire_reads(Instant::now() + REPLY_WAIT).is_none());
        assert!(st.has_older("600000001") && st.jump.is_none() && st.error.is_some());
        st.older("600000001");
        assert_eq!(st.take_cmds(), first, "same page asked again");
    }

    #[test]
    fn pairing_follows_the_daemon() {
        let addr: BtAddr = "00:11:22:33:44:55".parse().unwrap();
        let mut st = State::default();
        drop(st.apply(Out::Adapter { present: true, powered: false, discovering: false }));
        assert_eq!(st.pairing.adapter, Some(Adapter { present: true, powered: false, discovering: false }));
        drop(st.apply(Out::Confirm { addr: addr.clone(), passkey: 42 }));
        assert_eq!(st.pairing.confirm, Some((addr.clone(), 42)));
        // The end of the pairing withdraws the confirmation.
        drop(st.apply(Out::Paired { addr: addr.clone(), ok: false, error: Some("rejected".into()) }));
        assert_eq!(st.pairing.confirm, None);
        assert_eq!(st.pairing.paired, Some((addr.clone(), Err("rejected".into()))));
        // Retrying forgets the failure; answering withdraws the question.
        st.pairing.sent(&Cmd::Pair { addr: addr.clone() });
        assert_eq!(st.pairing.paired, None);
        drop(st.apply(Out::Confirm { addr, passkey: 42 }));
        st.pairing.sent(&Cmd::Confirm { ok: true });
        assert_eq!(st.pairing.confirm, None);
    }

    fn sms(handle: &str, addr: &str, ts: &str, text: &str, sent: bool) -> Out {
        let mut s = Sms { handle: handle.into(), timestamp: ts.into(), text: text.into(), ..Default::default() };
        if sent {
            s.recipient_address = addr.into();
        } else {
            s.address = addr.into();
            s.sender = "Alice".into();
        }
        Out::Sms { sms: s, folder: if sent { Folder::Sent } else { Folder::Inbox }, new: false }
    }

    #[test]
    fn groups_and_sorts_conversations() {
        let mut st = State::default();
        st.apply(sms("1", "+33712345601", "20261015T100000", "a", false));
        st.apply(sms("2", "0612345678", "20261015T110000", "b", false));
        st.apply(sms("3", "07 12 34 56 01", "20261015T120000", "c", true));
        st.apply(sms("3", "07 12 34 56 01", "20261015T120000", "c", true)); // duplicate ignored
        assert_eq!(st.convs.len(), 2);
        assert_eq!(st.convs[0].key, "712345601");
        assert_eq!(st.convs[0].msgs.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!(st.convs[0].name, "Alice");
        assert_eq!(st.convs[0].unread, 1);
    }

    #[test]
    fn local_send_replaced_by_sent_folder() {
        let mut st = State::default();
        st.apply(Out::Sent { to: "+33712345601".into(), text: "salut".into(), ok: true, error: None, timestamp: "20261015T100000".into() });
        assert!(st.convs[0].msgs[0].local);
        st.apply(sms("9", "+33712345601", "99991231T235959", "salut", true));
        assert_eq!(st.convs[0].msgs.len(), 1);
        assert!(!st.convs[0].msgs[0].local);
    }

    fn sent(text: &str, ok: bool, ts: &str) -> Out {
        Out::Sent { to: "+33712345601".into(), text: text.into(), ok, error: (!ok).then(|| "MAP".into()), timestamp: ts.into() }
    }

    #[test]
    fn pending_then_done_or_failed_then_resent() {
        let mut st = State::default();
        st.pending("0712345601", "a");
        assert_eq!(st.convs[0].msgs[0].delivery, Delivery::Pending, "immediate bubble");
        st.apply(sent("a", true, "20261015T100000"));
        assert_eq!(st.convs[0].msgs.iter().map(|m| m.delivery).collect::<Vec<_>>(), [Delivery::Done]);
        st.pending("0712345601", "b");
        st.apply(sent("b", false, "20261015T100100"));
        let failed = st.convs[0].msgs.last().unwrap().clone();
        assert_eq!((failed.delivery, failed.timestamp.as_str()), (Delivery::Failed, "20261015T100100"));
        st.pending("0712345601", "b");
        assert_eq!(st.convs[0].msgs.len(), 2, "the resend replaces the failed bubble");
        st.apply(sent("b", true, "20261015T100200"));
        assert!(st.convs[0].msgs.iter().all(|m| m.delivery == Delivery::Done));
        assert_eq!(st.convs[0].msgs.len(), 2);
    }

    #[test]
    fn sent_folder_before_transfer_result_no_duplicate() {
        let mut st = State::default();
        st.pending("+33712345601", "a");
        st.apply(sms("9", "+33712345601", "20261015T191343", "a", true));
        st.apply(sent("a", true, "20261015T191342"));
        assert_eq!(st.convs[0].msgs.len(), 1);
        assert!(!st.convs[0].msgs[0].local);
        // Same text resent much later: recorded.
        st.apply(sent("a", true, "20261015T193000"));
        assert_eq!(st.convs[0].msgs.len(), 2);
    }

    #[test]
    fn delete_message_or_conversation() {
        let mut st = State::default();
        st.apply(sms("1", "0612345678", "20261015T100000", "a", false));
        st.apply(sms("2", "0612345678", "20261015T110000", "b", false));
        st.apply(sms("3", "0700000000", "20261015T090000", "c", false));
        let item = st.conv("612345678").unwrap().msgs[1].item();
        st.apply(Out::Deleted { key: "612345678".into(), item: Some(item) });
        let c = st.conv("612345678").unwrap();
        assert_eq!((c.msgs.len(), c.unread), (1, 1));
        st.apply(Out::Deleted { key: "612345678".into(), item: None });
        assert!(st.conv("612345678").is_none());
        let item = st.convs[0].msgs[0].item();
        st.delete("700000000", Some(&item));
        assert!(st.convs.is_empty(), "emptied conversation removed");
    }

    #[test]
    fn pbap_status_keeps_link_error() {
        let mut st = State::default();
        assert_eq!(st.pbap, None);
        st.apply(Out::Error { msg: "service unavailable".into() });
        st.apply(Out::Status { link: Profile::Pbap, up: true });
        assert_eq!((st.error.as_deref(), st.pbap), (Some("service unavailable"), Some(true)));
        st.apply(Out::Status { link: Profile::Pbap, up: false });
        assert_eq!(st.pbap, Some(false));
    }

    #[test]
    fn link_up_clears_link_error() {
        let mut st = State::default();
        st.apply(Out::Error { msg: "service unavailable".into() });
        st.apply(Out::Status { link: Profile::Hfp, up: true });
        assert_eq!((st.error.as_deref(), st.hfp_up), (None, true));
        st.pending("+331", "x");
        st.apply(Out::Sent { to: "+331".into(), text: "x".into(), ok: false, error: None, timestamp: String::new() });
        st.apply(Out::Status { link: Profile::Map, up: true });
        assert!(st.error.is_some());
    }

    #[test]
    fn failed_send_sets_error() {
        let mut st = State::default();
        // Failure replayed (no send tracked here): failed bubble, no banner.
        st.apply(Out::Sent { to: "+331".into(), text: "x".into(), ok: false, error: Some("MAP".into()), timestamp: String::new() });
        assert_eq!((st.convs[0].msgs[0].delivery, st.error.is_none()), (Delivery::Failed, true));
        st.pending("+331", "x");
        assert_eq!(st.convs[0].msgs.len(), 1, "the resend replaces the failure");
        st.apply(Out::Sent { to: "+331".into(), text: "x".into(), ok: false, error: Some("MAP".into()), timestamp: String::new() });
        assert!(st.error.as_deref().unwrap().contains("MAP"));
        // Two identical sends back to back: two bubbles; link loss: failed.
        st.pending("+331", "ok");
        st.pending("+331", "ok");
        assert!(!st.expire_pending(true));
        assert_eq!(st.convs[0].msgs.iter().filter(|m| m.delivery == Delivery::Failed).count(), 3);
    }

    #[test]
    fn new_inbox_sms_notifies() {
        let mut st = State::default();
        let Out::Sms { sms, .. } = sms("1", "+331234", "t", "yo", false) else { unreachable!() };
        let fx = st.apply(Out::Sms { sms, folder: Folder::Inbox, new: true });
        assert_eq!(fx, [Effect::Sms { from: "Alice".into(), text: "yo".into(), reply: Some("+331234".into()) }]);
        // Alphanumeric sender: nothing to reply to.
        let Out::Sms { sms: s, .. } = self::sms("2", "INFO", "t", "code", false) else { unreachable!() };
        let fx = st.apply(Out::Sms { sms: s, folder: Folder::Inbox, new: true });
        assert!(matches!(&fx[..], [Effect::Sms { reply: None, .. }]));
    }

    fn ind(name: &str, value: u8) -> Out {
        Out::Call(HfEvent::Indicator { name: name.into(), value })
    }

    fn ended(fx: &[Effect]) -> Ended {
        match fx {
            [Effect::CallEnd(e)] => e.clone(),
            _ => panic!("fin d'appel attendue : {fx:?}"),
        }
    }

    fn shown(fx: &[Effect]) -> Call {
        match fx {
            [Effect::Call(c)] => c.clone(),
            _ => panic!("vue d'appel attendue : {fx:?}"),
        }
    }

    #[test]
    fn incoming_answered_then_ended() {
        let mut st = State::default();
        assert_eq!(shown(&st.apply(Out::Call(HfEvent::IncomingCall))).phase, Phase::Incoming);
        st.apply(ind("callsetup", 1));
        assert_eq!(st.apply(Out::Call(HfEvent::Ring)), []);
        assert_eq!(shown(&st.apply(Out::Call(HfEvent::Clip { number: "+331".into() }))).number.as_deref(), Some("+331"));
        assert_eq!(st.apply(Out::Call(HfEvent::Clip { number: "+331".into() })), []);
        let v = shown(&st.apply(ind("call", 1)));
        assert_eq!((v.phase, v.number.as_deref(), v.since.is_some()), (Phase::Active, Some("+331"), true));
        assert_eq!(st.apply(ind("callsetup", 0)), []);
        // Startup during a call: number learned without ringing.
        let mut late = State::default();
        late.apply(ind("call", 1));
        late.apply(ind("callsetup", 0));
        let v = shown(&late.apply(Out::Call(HfEvent::Clip { number: "+332".into() })));
        assert_eq!((v.phase, v.number.as_deref()), (Phase::Active, Some("+332")));
        let e = ended(&st.apply(ind("call", 0)));
        assert_eq!((e.number.as_str(), e.kind, e.timestamp.len()), ("+331", CallKind::In, 15));
        assert_eq!(st.call, Call::default());
    }

    #[test]
    fn mute_and_second_call() {
        let mut st = State::default();
        assert_eq!(st.set_muted(true), [], "no call: nothing to mute");
        st.apply(ind("call", 1));
        st.apply(Out::Call(HfEvent::Clip { number: "+331".into() }));
        assert!(shown(&st.set_muted(true)).muted);
        assert_eq!(st.set_muted(true), []);
        let lines = |hold, held: Option<&str>, waiting: Option<&str>| {
            Out::Call(HfEvent::Lines { hold, held: held.map(Into::into), waiting: waiting.map(Into::into) })
        };
        assert_eq!(shown(&st.apply(lines(false, None, Some("+332")))).waiting.as_deref(), Some("+332"));
        // Hold + answer: the second becomes the main call, the first goes on hold.
        st.apply(Out::Call(HfEvent::Clip { number: "+332".into() }));
        let v = shown(&st.apply(lines(false, Some("+331"), None)));
        assert_eq!((v.number.as_deref(), v.held.as_deref(), v.waiting), (Some("+332"), Some("+331"), None));
        // The second hangs up: only the first remains, on hold.
        st.apply(Out::Call(HfEvent::Clip { number: "+331".into() }));
        assert!(shown(&st.apply(lines(true, None, None))).hold);
        assert_eq!(ended(&st.apply(ind("call", 0))).kind, CallKind::In);
        assert!(!st.call.muted);
    }

    #[test]
    fn incoming_rejected_and_hfp_loss() {
        let mut st = State::default();
        st.apply(Out::Call(HfEvent::IncomingCall));
        st.apply(ind("callsetup", 1));
        let e = ended(&st.apply(ind("callsetup", 0)));
        assert_eq!((e.kind, e.secs, e.number.as_str()), (CallKind::Missed, 0, ""));
        st.apply(Out::Call(HfEvent::IncomingCall));
        assert_eq!(ended(&st.apply(Out::Status { link: Profile::Hfp, up: false })).kind, CallKind::Missed);
        assert_eq!(st.call.phase, Phase::Idle);
    }

    #[test]
    fn outgoing_call() {
        let mut st = State::default();
        assert!(matches!(&st.apply(ind("callsetup", 2))[..], [Effect::Call(c)] if c.phase == Phase::Outgoing));
        assert!(matches!(&st.apply(ind("call", 1))[..], [Effect::Call(c)] if c.phase == Phase::Active && c.since.is_some()));
        st.apply(ind("callsetup", 0));
        assert_eq!(ended(&st.apply(ind("call", 0))).kind, CallKind::Out);
    }

    #[test]
    fn unanswered_outgoing_call_keeps_its_number() {
        let mut st = State::default();
        st.apply(ind("callsetup", 3));
        assert!(matches!(&st.apply(Out::Call(HfEvent::Clip { number: "+331".into() }))[..], [Effect::Call(c)] if c.phase == Phase::Outgoing));
        let e = ended(&st.apply(ind("callsetup", 0)));
        assert_eq!((e.kind, e.secs, e.number.as_str()), (CallKind::Out, 0, "+331"));
    }

    #[test]
    fn reads_survive_replay() {
        let mut st = State::default();
        st.apply(sms("1", "0612345678", "20261015T100000", "a", false));
        st.apply(sms("2", "0612345678", "20261015T110000", "b", false));
        assert_eq!(st.convs[0].unread, 2);
        let until = st.mark_read("612345678", false).expect("unread to send");
        assert_eq!((until.as_str(), st.convs[0].unread), ("20261015T110000", 0));
        assert_eq!(st.mark_read("612345678", false), None, "already read: nothing to send");
        // New client: snapshot replayed (unread SMS then read mark) → nothing unread.
        let mut fresh = State::default();
        fresh.apply(sms("1", "0612345678", "20261015T100000", "a", false));
        fresh.apply(Out::Read { key: "612345678".into(), until: "20261015T100000".into() });
        fresh.apply(sms("2", "0612345678", "20261015T110000", "b", false));
        assert_eq!(fresh.convs[0].unread, 1, "only the message after the read mark");
        fresh.apply(Out::Read { key: "612345678".into(), until: "20261015T090000".into() });
        assert_eq!(fresh.convs[0].unread, 1, "a read mark never moves back");
    }

    #[test]
    fn calls_tab_reads_and_landlines() {
        let log = |n: &str, ts: &str| Out::CallLog { number: n.into(), timestamp: ts.into(), kind: CallKind::Missed, secs: 0, read: false };
        let mut st = State::default();
        st.apply(sms("1", "0612345678", "20261015T100000", "a", false));
        st.apply(log("0612345678", "20261015T110000"));
        assert_eq!(st.mark_read("612345678", true), None, "unread SMS hidden in the Calls tab");
        assert_eq!(st.convs[0].unread, 2);
        assert_eq!(st.unread_calls(&st.convs[0]), 1, "Calls badge: calls only");
        assert!(st.mark_read("612345678", false).is_some());
        st.apply(log("0612345678", "20261015T120000"));
        assert_eq!(st.mark_read("612345678", true).as_deref(), Some("20261015T120000"), "only calls unread");
        // Landline with calls only: out of the SMS list, unless the country is unknown.
        st.apply(log("03 89 00 00 00", "20261015T130000"));
        let fr = Some(Id::FR);
        let fixed = st.convs.iter().find(|c| c.number.starts_with("03")).unwrap();
        assert!(!fixed.textable(fr) && fixed.textable(None));
        assert_eq!(fixed.last_call().map(|m| m.timestamp.as_str()), Some("20261015T130000"));
        assert!(st.convs.iter().find(|c| c.key == "612345678").unwrap().textable(fr));
    }

    #[test]
    fn muted_event_reaches_clients() {
        let mut st = State::default();
        assert_eq!(st.apply(Out::Muted { on: true }), [], "no call");
        st.apply(Out::Call(HfEvent::IncomingCall));
        st.apply(ind("call", 1));
        assert!(shown(&st.apply(Out::Muted { on: true })).muted);
        assert!(!shown(&st.apply(Out::Muted { on: false })).muted);
    }

    #[test]
    fn battery_bounded() {
        let mut st = State::default();
        assert!(st.apply(Out::Battery { percent: Some(42) }).is_empty());
        assert_eq!(st.battery, Some(42));
        st.apply(Out::Battery { percent: Some(101) });
        assert_eq!(st.battery, None, "out of range");
    }

    #[test]
    fn contacts_name_convs_calls_and_log() {
        let mut st = State::default();
        st.apply(sms("1", "+33612345678", "20261015T100000", "a", false));
        st.apply(Out::Contacts { names: vec![("612345678".into(), "Bob".into())] });
        assert_eq!(st.convs[0].name, "Bob");
        let v = shown(&st.apply(Out::Call(HfEvent::Clip { number: "06 12 34 56 78".into() })));
        assert_eq!(v.name.as_deref(), Some("Bob"));
        st.apply(Out::CallLog { number: "0612345678".into(), timestamp: "20261015T110000".into(), kind: CallKind::Missed, secs: 0, read: false });
        let c = &st.convs[0];
        assert_eq!((c.msgs.len(), c.unread), (2, 2), "unread text + missed call");
        assert_eq!(c.msgs[1].call, Some((CallKind::Missed, 0)));
        // New SMS: the notification carries the contact name.
        let mut s = Sms { handle: "9".into(), address: "+33612345678".into(), timestamp: "20261015T120000".into(), text: "b".into(), ..Default::default() };
        s.sender = String::new();
        assert_eq!(st.apply(Out::Sms { sms: s, folder: Folder::Inbox, new: true }), [Effect::Sms { from: "Bob".into(), text: "b".into(), reply: Some("+33612345678".into()) }]);
    }

    #[test]
    fn muted_conversation_stays_silent() {
        let mut st = State::default();
        st.apply(sms("1", "+33600000001", "20261015T100000", "a", false));
        let key = st.convs[0].key.clone();
        let muted = ConvFlags { muted: true, archived: false };
        st.apply(Out::Conv { key: key.clone(), flags: muted });
        assert_eq!(st.flags(&key), muted);
        let s = Sms { handle: "2".into(), address: "+33600000001".into(), timestamp: "20261015T110000".into(), text: "b".into(), ..Default::default() };
        assert!(st.apply(Out::Sms { sms: s, folder: Folder::Inbox, new: true }).is_empty(), "no notification");
        assert_eq!(st.convs[0].unread, 2, "still counted unread");
        // Whole conversation deleted: its flags go too.
        st.delete(&key, None);
        assert_eq!(st.flags(&key), ConvFlags::default());
        st.apply(Out::Conv { key: key.clone(), flags: muted });
        st.apply(Out::Conv { key: key.clone(), flags: ConvFlags::default() });
        assert_eq!(st.flags(&key), ConvFlags::default());
    }

    #[test]
    fn search_by_name_or_number() {
        let mut st = State::default();
        st.apply(Out::Contacts { names: vec![("+33 6 12 34 56 78".into(), "Émile Zola".into()), ("0700000000".into(), "Anne".into())] });
        // Contacts without a conversation.
        let alone = |_: &str, c: Option<&Conv>| c.is_none();
        assert_eq!(st.search("emi", 10, alone), [("Émile Zola", "+33 6 12 34 56 78")], "accents and case ignored");
        assert_eq!(st.search("ZOLA", 10, alone).len(), 1);
        assert_eq!(st.search("06 12", 10, alone).len(), 1, "forme nationale");
        assert_eq!(st.search("+336123", 10, alone).len(), 1, "forme internationale");
        assert_eq!(st.search("0", 10, alone).len(), 2);
        assert_eq!(st.search("0", 1, alone).len(), 1, "bounded");
        assert!(st.search("  ", 10, alone).is_empty());
        assert_eq!(st.search("", 10, alone), []);
        assert_eq!(st.search("a", 10, alone)[0].0, "Anne", "sorted by name");
        // A conversation hidden by the current tab leaves its contact in the results.
        st.apply(sms("1", "0612345678", "20261015T100000", "a", false));
        assert!(st.search("emi", 10, alone).is_empty(), "conversation listed");
        let no_call = |_: &str, c: Option<&Conv>| c.is_none_or(|c| c.last_call().is_none());
        assert_eq!(st.search("emi", 10, no_call).len(), 1, "conversation hidden");
        assert_eq!(st.search("0", 10, |n, _| n.starts_with("07")), [("Anne", "0700000000")], "filtered by number");
    }
}
