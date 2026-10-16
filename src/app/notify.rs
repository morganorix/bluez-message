//! "Navi" notifications (pleamar server) in a dedicated thread: a slow server does not freeze the UI.
//! The call notification's actions come back as a [`Cmd`] through `on_cmd`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crate::engine::Cmd;
use notify_rust::{Notification, Timeout, Urgency};

use super::state::{Call, Effect, Phase};
use crate::t;

const APP: &str = "Navi";

/// The text of an SMS is controlled by the sender: neutralises the markup allowed by the spec.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Ringing bubble refresh: a replacement restarts the pleamar card's delay.
const TICK: Duration = Duration::from_secs(1);

pub fn spawn(effects: Receiver<Effect>, on_cmd: impl Fn(Cmd) + Clone + Send + 'static) {
    thread::spawn(move || {
        let mut bubble = Bubble { on_cmd, id: Arc::new(AtomicU32::new(0)), number: None };
        loop {
            match effects.recv_timeout(TICK) {
                Ok(Effect::Sms { from, text }) => {
                    if let Err(e) = Notification::new().appname(APP).summary(&escape(&from)).body(&escape(&text)).icon("phone").show() {
                        tracing::warn!("SMS notification: {e}");
                    }
                }
                Ok(Effect::Call(c)) => bubble.show(&c),
                Ok(Effect::CallEnd(_)) => bubble.close(),
                Err(RecvTimeoutError::Timeout) => bubble.tick(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        bubble.close();
    });
}

/// The ringing bubble (Answer / Decline), replaced in place (same id) while it rings.
/// Answered, outgoing: the `bluez-msg-call` window takes over, the bubble closes.
struct Bubble<F> {
    on_cmd: F,
    /// Displayed id; 0 = none. Reset to 0 by the listener on close (button, manual close).
    id: Arc<AtomicU32>,
    /// Number displayed while ringing; `None`: no bubble.
    number: Option<String>,
}

impl<F: Fn(Cmd) + Clone + Send + 'static> Bubble<F> {
    fn show(&mut self, c: &Call) {
        if c.phase != Phase::Incoming {
            self.close();
            return;
        }
        let number = c.caller("\n");
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
        // Same id: pleamar updates the card without making it drop again; the existing listener stays valid.
        let current = self.id.load(Ordering::Acquire);
        if current != 0 {
            n.id(current);
        }
        match n.show() {
            Ok(h) if h.id() != current => {
                let id = h.id();
                self.id.store(id, Ordering::Release);
                let (on_cmd, slot) = (self.on_cmd.clone(), Arc::clone(&self.id));
                thread::spawn(move || {
                    h.wait_for_action(|a| {
                        // Card closed (button or ×): released before the command, the resulting state creates a new one.
                        let _ = slot.compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire);
                        let cmd = match a {
                            "answer" => Cmd::Answer,
                            "reject" => Cmd::Reject,
                            _ => return,
                        };
                        on_cmd(cmd);
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
