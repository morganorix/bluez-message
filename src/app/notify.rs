//! "Navi" notifications (pleamar server) in a dedicated thread: a slow server does not freeze the UI.
//! Raw `org.freedesktop.Notifications` calls on one session connection, whose signals (card actions)
//! come back as a [`Cmd`] through `on_cmd`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use zbus::blocking::Connection;
use zbus::zvariant::Value;

use super::state::{Call, Effect, Phase};
use crate::engine::Cmd;
use crate::t;

const APP: &str = "Navi";
const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// The text of an SMS is controlled by the sender: neutralises the body markup of servers that support it.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Ringing bubble refresh: a replacement restarts the pleamar card's delay.
const TICK: Duration = Duration::from_secs(1);
/// SMS notifications remembered for `CloseAll`; the oldest are forgotten (closed by the server or the user long ago).
const KEPT: usize = 64;

/// Ends once every sender is dropped and the pending effects are applied.
pub fn spawn(effects: Receiver<Effect>, on_cmd: impl Fn(Cmd) + Clone + Send + 'static) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut n = Notifier { link: None, markup: false, shared: Arc::default(), on_cmd, ringing: None };
        loop {
            match effects.recv_timeout(TICK) {
                Ok(Effect::Sms { from, text, reply }) => n.sms(&from, &text, reply),
                Ok(Effect::Unsent { to, error, reply }) => n.sms(&t!("SMS not sent to {0}", to), &error, reply),
                Ok(Effect::Call(c)) => n.ring(&c, None),
                Ok(Effect::Ring(c, reply)) => n.ring(&c, reply),
                Ok(Effect::CallEnd(_)) => n.hang_up(),
                Ok(Effect::Exported(r)) => exported(&r),
                Ok(Effect::CloseAll) => {
                    n.hang_up();
                    n.close_all();
                }
                Err(RecvTimeoutError::Timeout) => n.tick(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        n.hang_up();
    })
}

/// A notification to show (`Notify` arguments); plain text, the summary never holds markup.
struct Card<'a> {
    summary: &'a str,
    body: &'a str,
    icon: &'a str,
    /// Key, label pairs.
    actions: Vec<&'a str>,
    /// Card replaced in place; 0 = new one.
    replaces: u32,
    /// Critical urgency, never expires (ringing bubble).
    critical: bool,
}

impl Card<'_> {
    /// Shows the card on `conn` (`markup`: the server reads it in the body); returns its id and the unique name of
    /// the server.
    fn show(&self, conn: &Connection, markup: bool) -> zbus::Result<(u32, Option<String>)> {
        let body = if markup { escape(self.body) } else { self.body.to_owned() };
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        if self.critical {
            hints.insert("urgency", Value::U8(2));
        }
        let expire = if self.critical { 0i32 } else { -1 };
        let msg = conn.call_method(Some(DEST), PATH, Some(DEST), "Notify", &(APP, self.replaces, self.icon, self.summary, body, &self.actions, hints, expire))?;
        let id: u32 = msg.body().deserialize()?;
        Ok((id, msg.header().sender().map(ToString::to_string)))
    }
}

/// SMS cards shown, oldest first, with the number their "Reply" action opens.
#[derive(Default)]
struct Cards {
    shown: VecDeque<(u32, Option<String>)>,
}

impl Cards {
    /// Remembers card `id`; returns the oldest one forgotten beyond [`KEPT`], to close.
    fn push(&mut self, id: u32, reply: Option<String>) -> Option<u32> {
        let old = if self.shown.len() == KEPT { self.shown.pop_front().map(|(id, _)| id) } else { None };
        self.shown.push_back((id, reply));
        old
    }

    /// Card `id` closed (action, expiry, ×): forgotten; returns the number of its "Reply" action, if any.
    fn closed(&mut self, id: u32) -> Option<String> {
        let i = self.shown.iter().position(|(c, _)| *c == id)?;
        self.shown.remove(i).and_then(|(_, reply)| reply)
    }
}

/// State read by the signal listener.
#[derive(Default)]
struct Shared {
    /// Unique name of the server that showed the last card: the only sender of signals listened to.
    server: Mutex<Option<String>>,
    cards: Mutex<Cards>,
    /// Ringing bubble id; 0 = none. Reset to 0 by the listener on close (button, ×).
    bubble: AtomicU32,
    /// Number "Quick reply" opens, read by the listener when pressed: the number (CLIP) can arrive after the
    /// first display.
    reply: Mutex<Option<String>>,
}

impl Shared {
    /// Signal about card `id` (`action` when a button was pressed, `None` once closed): its commands.
    fn signal(&self, id: u32, action: Option<&str>) -> Vec<Cmd> {
        // Bubble closed (button or ×): released before the command, the resulting state creates a new one.
        if id != 0 && self.bubble.compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let reply = self.reply.lock().unwrap_or_else(PoisonError::into_inner).clone();
            return action.map(|a| bubble_cmds(a, reply)).unwrap_or_default();
        }
        let number = self.cards.lock().unwrap_or_else(PoisonError::into_inner).closed(id);
        match (action, number) {
            (Some("reply"), Some(number)) => vec![Cmd::Open { number: Some(number) }],
            _ => vec![],
        }
    }
}

/// Commands of bubble button `action`; rejected first: the window must not wait for the end of the ringing.
fn bubble_cmds(action: &str, reply: Option<String>) -> Vec<Cmd> {
    match action {
        "answer" => vec![Cmd::Answer],
        "reject" => vec![Cmd::Reject],
        "quick_reply" => std::iter::once(Cmd::Reject).chain(reply.map(|number| Cmd::Open { number: Some(number) })).collect(),
        _ => vec![],
    }
}

/// SMS cards and the ringing bubble (Answer / Decline, and Quick reply when the caller is a mobile), replaced in
/// place (same id) while it rings; answered, outgoing: the `bluez-msg-call` window takes over, the bubble closes.
/// One session connection: a single thread receives the server's signals (broadcast or sent to the sender only).
struct Notifier<F> {
    /// Opened at the first card (the session bus may come up after the daemon); `None` again after a failure.
    link: Option<Connection>,
    /// The server of `link` reads markup in the body.
    markup: bool,
    shared: Arc<Shared>,
    on_cmd: F,
    /// Caller displayed while ringing; `None`: no bubble.
    ringing: Option<String>,
}

impl<F: Fn(Cmd) + Clone + Send + 'static> Notifier<F> {
    /// Session connection, opened with its listener if needed.
    fn conn(&mut self) -> zbus::Result<Connection> {
        if let Some(c) = &self.link {
            return Ok(c.clone());
        }
        let c = Connection::session()?;
        listen(&c, Arc::clone(&self.shared), self.on_cmd.clone())?;
        self.markup = markup(&c);
        self.link = Some(c.clone());
        Ok(c)
    }

    /// Shows `card`; returns its id.
    fn show(&mut self, card: &Card<'_>) -> zbus::Result<u32> {
        let r = self.conn().and_then(|c| card.show(&c, self.markup));
        match r {
            Ok((id, server)) => {
                *self.shared.server.lock().unwrap_or_else(PoisonError::into_inner) = server;
                Ok(id)
            }
            Err(e) => Err(self.failed(e)),
        }
    }

    /// A broken connection is dropped, reopened at the next card (its listener ends with it); a server's
    /// error reply (no notification server, timeout) keeps it: no new connection or listener per card.
    fn failed(&mut self, e: zbus::Error) -> zbus::Error {
        if !matches!(e, zbus::Error::MethodError(..) | zbus::Error::FDO(_)) {
            self.link = None;
        }
        e
    }

    /// Shows the card; with `reply`, a "Reply" action that opens the conversation.
    fn sms(&mut self, from: &str, text: &str, reply: Option<String>) {
        let actions = if reply.is_some() { vec!["reply", t!("Reply")] } else { vec![] };
        let card = Card { summary: from, body: text, icon: "phone", actions, replaces: 0, critical: false };
        match self.show(&card) {
            Ok(id) => {
                let old = self.shared.cards.lock().unwrap_or_else(PoisonError::into_inner).push(id, reply);
                if let Some(old) = old {
                    self.close(old);
                }
            }
            Err(e) => tracing::warn!("SMS notification: {e}"),
        }
    }

    fn close_all(&mut self) {
        let shown = std::mem::take(&mut self.shared.cards.lock().unwrap_or_else(PoisonError::into_inner).shown);
        for (id, _) in shown {
            self.close(id);
        }
    }

    fn ring(&mut self, c: &Call, reply: Option<String>) {
        if c.phase != Phase::Incoming {
            self.hang_up();
            return;
        }
        let number = c.caller("\n");
        *self.shared.reply.lock().unwrap_or_else(PoisonError::into_inner) = reply;
        self.display(&number);
        self.ringing = Some(number);
    }

    /// Keeps the bubble displayed; a closed one (button, ×) is not recreated.
    fn tick(&mut self) {
        if let Some(n) = self.ringing.clone()
            && self.shared.bubble.load(Ordering::Acquire) != 0
        {
            self.display(&n);
        }
    }

    fn hang_up(&mut self) {
        self.ringing = None;
        *self.shared.reply.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let id = self.shared.bubble.swap(0, Ordering::AcqRel);
        if id != 0 {
            self.close(id);
        }
    }

    fn display(&mut self, number: &str) {
        let mut actions = vec!["answer", t!("Answer"), "reject", t!("Decline")];
        if self.shared.reply.lock().unwrap_or_else(PoisonError::into_inner).is_some() {
            actions.extend(["quick_reply", t!("QUICK REPLY")]);
        }
        // Same id: pleamar updates the card without making it drop again.
        let replaces = self.shared.bubble.load(Ordering::Acquire);
        let card = Card { summary: t!("Incoming call"), body: number, icon: "call-start", actions, replaces, critical: true };
        match self.show(&card) {
            // Closed by the listener meanwhile: left at 0, the button's command is under way.
            Ok(id) => {
                let _ = self.shared.bubble.compare_exchange(replaces, id, Ordering::AcqRel, Ordering::Acquire);
            }
            Err(e) => tracing::warn!("call notification: {e}"),
        }
    }

    /// Already closed (expired, dismissed): the server ignores it. Reconnects if needed: a ringing bubble
    /// must not outlive the call.
    fn close(&mut self, id: u32) {
        if let Err(e) = self.conn().and_then(|c| close_on(&c, id)) {
            tracing::debug!("closing notification: {}", self.failed(e));
        }
    }
}

/// Listens to the notification signals on `conn` in a thread that lives as long as the connection.
fn listen(conn: &Connection, shared: Arc<Shared>, on_cmd: impl Fn(Cmd) + Send + 'static) -> zbus::Result<()> {
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(DEST)?.path(PATH)?.build();
    let signals = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(KEPT))?;
    thread::spawn(move || {
        for msg in signals.flatten() {
            let h = msg.header();
            let server = shared.server.lock().unwrap_or_else(PoisonError::into_inner).clone();
            if server.is_none() || h.sender().map(ToString::to_string) != server {
                continue;
            }
            let cmds = match h.member().map(zbus::names::MemberName::as_str) {
                Some("ActionInvoked") => match msg.body().deserialize::<(u32, &str)>() {
                    Ok((id, action)) => shared.signal(id, Some(action)),
                    Err(_) => continue,
                },
                Some("NotificationClosed") => match msg.body().deserialize::<(u32, u32)>() {
                    Ok((id, _reason)) => shared.signal(id, None),
                    Err(_) => continue,
                },
                _ => continue,
            };
            for cmd in cmds {
                on_cmd(cmd);
            }
        }
    });
    Ok(())
}

/// Plain card for the result of a history export, on its own connection (GUI or daemon).
pub fn exported(r: &Result<String, String>) {
    let (summary, body, icon) = match r {
        Ok(path) => (t!("History exported"), path, "document-save"),
        Err(msg) => (t!("History not exported"), msg, "dialog-error"),
    };
    let card = Card { summary, body, icon, actions: vec![], replaces: 0, critical: false };
    if let Err(e) = Connection::session().and_then(|c| card.show(&c, markup(&c))) {
        tracing::warn!("export notification: {e}");
    }
}

/// The server announces `body-markup`; without an answer, plain text: entities would show literally.
fn markup(c: &Connection) -> bool {
    c.call_method(Some(DEST), PATH, Some(DEST), "GetCapabilities", &())
        .and_then(|m| m.body().deserialize::<Vec<String>>())
        .is_ok_and(|caps| caps.iter().any(|c| c == "body-markup"))
}

fn close_on(c: &Connection, id: u32) -> zbus::Result<()> {
    c.call_method(Some(DEST), PATH, Some(DEST), "CloseNotification", &(id,))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup() {
        assert_eq!(escape("<a href=x>&</a>"), "&lt;a href=x&gt;&amp;&lt;/a&gt;");
    }

    #[test]
    fn caller_name() {
        let mut c = Call::default();
        assert_eq!(c.caller("\n"), t!("Hidden number"));
        c.number = Some(String::new());
        assert_eq!(c.caller("\n"), t!("Hidden number"));
        c.number = Some("+331".into());
        assert_eq!(c.caller("\n"), "+331");
        c.name = Some("Bob".into());
        assert_eq!(c.caller("\n"), "Bob\n+331");
    }

    #[test]
    fn cards_map_ids_to_numbers_within_bounds() {
        let mut c = Cards::default();
        assert_eq!(c.push(1, Some("+33600000001".into())), None);
        assert_eq!(c.push(2, None), None);
        assert_eq!(c.closed(2), None);
        // Reply action then close signal: opened once.
        assert_eq!(c.closed(1).as_deref(), Some("+33600000001"));
        assert_eq!(c.closed(1), None);
        for id in 10..10 + u32::try_from(KEPT).unwrap() {
            assert_eq!(c.push(id, Some("+33600000002".into())), None);
        }
        // Beyond KEPT, the oldest is forgotten and handed back to close.
        assert_eq!(c.push(99, None), Some(10));
        assert_eq!(c.closed(10), None);
        assert_eq!(c.shown.len(), KEPT);
    }

    #[test]
    fn bubble_buttons_reject_first() {
        let n = || Some("+33600000001".to_owned());
        assert_eq!(bubble_cmds("answer", n()), vec![Cmd::Answer]);
        assert_eq!(bubble_cmds("reject", n()), vec![Cmd::Reject]);
        assert_eq!(bubble_cmds("quick_reply", n()), vec![Cmd::Reject, Cmd::Open { number: n() }]);
        // Number unknown when pressed: the call is still declined.
        assert_eq!(bubble_cmds("quick_reply", None), vec![Cmd::Reject]);
        assert_eq!(bubble_cmds("unknown", n()), vec![]);
    }

    #[test]
    fn signals_route_bubble_then_cards() {
        let s = Shared::default();
        s.bubble.store(7, Ordering::Release);
        s.cards.lock().unwrap().push(8, Some("+33600000002".into()));
        // Button then close signal: one command only, the bubble is released.
        assert_eq!(s.signal(7, Some("answer")), vec![Cmd::Answer]);
        assert_eq!(s.signal(7, None), vec![]);
        assert_eq!(s.bubble.load(Ordering::Acquire), 0);
        // A bubble closed by × sends nothing.
        s.bubble.store(9, Ordering::Release);
        assert_eq!(s.signal(9, None), vec![]);
        assert_eq!(s.signal(8, Some("reply")), vec![Cmd::Open { number: Some("+33600000002".into()) }]);
        assert_eq!(s.signal(8, Some("reply")), vec![]);
        assert_eq!(s.signal(0, Some("reply")), vec![]);
    }
}
