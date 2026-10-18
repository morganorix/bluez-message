//! "Navi" notifications (pleamar server) in a dedicated thread: a slow server does not freeze the UI.
//! The call notification's actions come back as a [`Cmd`] through `on_cmd`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crate::engine::Cmd;
use notify_rust::{Notification, Timeout, Urgency};

use super::state::{Call, Decline, Effect, Phase};
use crate::t;

const APP: &str = "Navi";
const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// The text of an SMS is controlled by the sender: neutralises the markup allowed by the spec.
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
        let mut sms = SmsCards { link: None, cards: Arc::default(), on_cmd: on_cmd.clone() };
        let mut bubble = Bubble { on_cmd, id: Arc::new(AtomicU32::new(0)), number: None, decline: Arc::default() };
        loop {
            match effects.recv_timeout(TICK) {
                Ok(Effect::Sms { from, text, reply }) => {
                    if let Err(e) = sms.show(&from, &text, reply) {
                        tracing::warn!("SMS notification: {e}");
                    }
                }
                Ok(Effect::Unsent { to, error, reply }) => {
                    if let Err(e) = sms.show(&t!("SMS not sent to {0}", to), &error, reply) {
                        tracing::warn!("SMS notification: {e}");
                    }
                }
                Ok(Effect::Call(c)) => bubble.show(&c, None),
                Ok(Effect::Ring(c, d)) => bubble.show(&c, d),
                Ok(Effect::CallEnd(_)) => bubble.close(),
                Ok(Effect::Exported(r)) => exported(&r),
                Ok(Effect::CloseAll) => {
                    bubble.close();
                    sms.close_all();
                }
                Err(RecvTimeoutError::Timeout) => bubble.tick(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        bubble.close();
    })
}

/// SMS cards shown, oldest first, with the number their "Reply" action opens.
#[derive(Default)]
struct Cards {
    shown: VecDeque<(u32, Option<String>)>,
    /// Unique name of the server that showed the last card: the only sender of signals listened to.
    server: Option<String>,
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

/// SMS notifications, shown and listened to on one session connection: a single thread receives the server's
/// signals (broadcast or sent to the sender only), whatever the number of cards.
struct SmsCards<F> {
    /// Opened at the first SMS (the session bus may come up after the daemon); `None` again if it fails.
    link: Option<zbus::blocking::Connection>,
    cards: Arc<Mutex<Cards>>,
    on_cmd: F,
}

impl<F: Fn(Cmd) + Clone + Send + 'static> SmsCards<F> {
    /// Shows the card; with `reply`, a "Reply" action that opens the conversation.
    fn show(&mut self, from: &str, text: &str, reply: Option<String>) -> zbus::Result<()> {
        let conn = if let Some(c) = &self.link {
            c.clone()
        } else {
            let c = zbus::blocking::Connection::session()?;
            listen(&c, Arc::clone(&self.cards), self.on_cmd.clone())?;
            self.link = Some(c.clone());
            c
        };
        let actions: Vec<&str> = if reply.is_some() { vec!["reply", t!("Reply")] } else { vec![] };
        let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
        let msg = conn.call_method(Some(DEST), PATH, Some(DEST), "Notify", &(APP, 0u32, "phone", escape(from), escape(text), actions, hints, -1i32))?;
        let id: u32 = msg.body().deserialize()?;
        let old = {
            let mut cards = self.cards.lock().unwrap_or_else(PoisonError::into_inner);
            cards.server = msg.header().sender().map(ToString::to_string);
            cards.push(id, reply)
        };
        if let Some(old) = old
            && let Err(e) = close_on(&conn, old)
        {
            tracing::debug!("closing notification: {e}");
        }
        Ok(())
    }

    fn close_all(&mut self) {
        let shown = std::mem::take(&mut self.cards.lock().unwrap_or_else(PoisonError::into_inner).shown);
        let Some(conn) = &self.link else { return };
        for (id, _) in shown {
            // Already closed (expired, dismissed): the server ignores it.
            if let Err(e) = close_on(conn, id) {
                tracing::debug!("closing notification: {e}");
            }
        }
    }
}

/// Listens to the notification signals on `conn` in a thread that lives as long as the connection.
fn listen(conn: &zbus::blocking::Connection, cards: Arc<Mutex<Cards>>, on_cmd: impl Fn(Cmd) + Send + 'static) -> zbus::Result<()> {
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(DEST)?.path(PATH)?.build();
    let signals = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(KEPT))?;
    thread::spawn(move || {
        for msg in signals.flatten() {
            let h = msg.header();
            let mut cards = cards.lock().unwrap_or_else(PoisonError::into_inner);
            if cards.server.is_none() || h.sender().map(ToString::to_string) != cards.server {
                continue;
            }
            let open = match h.member().map(zbus::names::MemberName::as_str) {
                Some("ActionInvoked") => match msg.body().deserialize::<(u32, &str)>() {
                    Ok((id, "reply")) => cards.closed(id),
                    _ => None,
                },
                Some("NotificationClosed") => {
                    if let Ok((id, _reason)) = msg.body().deserialize::<(u32, u32)>() {
                        cards.closed(id);
                    }
                    None
                }
                _ => None,
            };
            drop(cards);
            if let Some(number) = open {
                on_cmd(Cmd::Open { number: Some(number) });
            }
        }
    });
    Ok(())
}

/// The ringing bubble (Answer / Decline, and SMS replies when the caller is a mobile), replaced in place (same id) while it rings.
/// Answered, outgoing: the `bluez-msg-call` window takes over, the bubble closes.
struct Bubble<F> {
    on_cmd: F,
    /// Displayed id; 0 = none. Reset to 0 by the listener on close (button, manual close).
    id: Arc<AtomicU32>,
    /// Number displayed while ringing; `None`: no bubble.
    number: Option<String>,
    /// Read by the listener when a button is pressed: the number (CLIP) can arrive after the first display.
    decline: Arc<Mutex<Option<Decline>>>,
}

impl<F: Fn(Cmd) + Clone + Send + 'static> Bubble<F> {
    fn show(&mut self, c: &Call, decline: Option<Decline>) {
        if c.phase != Phase::Incoming {
            self.close();
            return;
        }
        let number = c.caller("\n");
        *self.decline.lock().unwrap_or_else(PoisonError::into_inner) = decline;
        self.display(&number);
        self.number = Some(number);
    }

    /// Keeps the card displayed; a closed bubble (button, ×) is not recreated.
    fn tick(&mut self) {
        if let Some(n) = &self.number
            && self.id.load(Ordering::Acquire) != 0
        {
            self.display(n);
        }
    }

    fn close(&mut self) {
        self.number = None;
        *self.decline.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let id = self.id.swap(0, Ordering::AcqRel);
        if id != 0
            && let Err(e) = close(id)
        {
            tracing::warn!("closing notification: {e}");
        }
    }

    fn display(&self, number: &str) {
        let mut n = Notification::new();
        n.appname(APP)
            .summary(t!("Incoming call"))
            .body(&escape(number))
            .icon("call-start")
            .urgency(Urgency::Critical)
            .timeout(Timeout::Never)
            .action("answer", t!("Answer"))
            .action("reject", t!("Decline"));
        if let Some(d) = &*self.decline.lock().unwrap_or_else(PoisonError::into_inner) {
            if d.text.is_some() {
                n.action("decline_sms", t!("Decline + SMS"));
            }
            n.action("decline_write", t!("Decline and write"));
        }
        // Same id: pleamar updates the card without making it drop again; the existing listener stays valid.
        let current = self.id.load(Ordering::Acquire);
        if current != 0 {
            n.id(current);
        }
        match n.show() {
            Ok(h) if h.id() != current => {
                let id = h.id();
                self.id.store(id, Ordering::Release);
                let (on_cmd, slot, decline) = (self.on_cmd.clone(), Arc::clone(&self.id), Arc::clone(&self.decline));
                thread::spawn(move || {
                    h.wait_for_action(|a| {
                        // Card closed (button or ×): released before the command, the resulting state creates a new one.
                        let _ = slot.compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire);
                        let then = match a {
                            "answer" => return on_cmd(Cmd::Answer),
                            "reject" => None,
                            "decline_sms" | "decline_write" => {
                                let d = decline.lock().unwrap_or_else(PoisonError::into_inner).clone();
                                d.and_then(|Decline { number, text }| match (a, text) {
                                    ("decline_sms", Some(text)) => Some(Cmd::Send { to: number, text }),
                                    ("decline_write", _) => Some(Cmd::Open { number: Some(number) }),
                                    _ => None,
                                })
                            }
                            _ => return,
                        };
                        // Rejected first: the SMS or the window must not wait for the end of the ringing.
                        on_cmd(Cmd::Reject);
                        if let Some(cmd) = then {
                            on_cmd(cmd);
                        }
                    });
                });
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("call notification: {e}"),
        }
    }
}

/// Plain card for the result of a history export.
pub fn exported(r: &Result<String, String>) {
    let (summary, body, icon) = match r {
        Ok(path) => (t!("History exported"), path, "document-save"),
        Err(msg) => (t!("History not exported"), msg, "dialog-error"),
    };
    if let Err(e) = Notification::new().appname(APP).summary(summary).body(&escape(body)).icon(icon).show() {
        tracing::warn!("export notification: {e}");
    }
}

fn close(id: u32) -> zbus::Result<()> {
    close_on(&zbus::blocking::Connection::session()?, id)
}

fn close_on(c: &zbus::blocking::Connection, id: u32) -> zbus::Result<()> {
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

}
