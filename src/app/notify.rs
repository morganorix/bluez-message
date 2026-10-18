//! "Navi" notifications (pleamar server) in a dedicated thread: a slow server does not freeze the UI.
//! The call notification's actions come back as a [`Cmd`] through `on_cmd`.

use std::collections::VecDeque;
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
        let mut bubble = Bubble { on_cmd, id: Arc::new(AtomicU32::new(0)), number: None, decline: Arc::default() };
        let mut shown = VecDeque::with_capacity(KEPT);
        loop {
            match effects.recv_timeout(TICK) {
                Ok(Effect::Sms { from, text, reply }) => match sms(&from, &text, reply, &bubble.on_cmd) {
                    Ok(id) => {
                        // The oldest is closed: its "Reply" listener ends with it.
                        if shown.len() == KEPT
                            && let Some(old) = shown.pop_front()
                            && let Err(e) = close(old)
                        {
                            tracing::debug!("closing notification: {e}");
                        }
                        shown.push_back(id);
                    }
                    Err(e) => tracing::warn!("SMS notification: {e}"),
                },
                Ok(Effect::Call(c)) => bubble.show(&c, None),
                Ok(Effect::Ring(c, d)) => bubble.show(&c, d),
                Ok(Effect::CallEnd(_)) => bubble.close(),
                Ok(Effect::CloseAll) => {
                    bubble.close();
                    for id in shown.drain(..) {
                        // Already closed (expired, dismissed): the server ignores it.
                        if let Err(e) = close(id) {
                            tracing::debug!("closing notification: {e}");
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => bubble.tick(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        bubble.close();
    })
}

/// SMS notification; with `reply`, a "Reply" action that opens the conversation, listened to in a thread that
/// ends when the card closes (action, expiry, ×).
fn sms(from: &str, text: &str, reply: Option<String>, on_cmd: &(impl Fn(Cmd) + Clone + Send + 'static)) -> notify_rust::error::Result<u32> {
    let mut n = Notification::new();
    n.appname(APP).summary(&escape(from)).body(&escape(text)).icon("phone");
    if reply.is_some() {
        n.action("reply", t!("Reply"));
    }
    let h = n.show()?;
    let id = h.id();
    if let Some(number) = reply {
        let on_cmd = on_cmd.clone();
        thread::spawn(move || {
            h.wait_for_action(|a| {
                if a == "reply" {
                    on_cmd(Cmd::Open { number: Some(number) });
                }
            });
        });
    }
    Ok(id)
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

fn close(id: u32) -> zbus::Result<()> {
    let c = zbus::blocking::Connection::session()?;
    c.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "CloseNotification",
        &(id,),
    )?;
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

}
