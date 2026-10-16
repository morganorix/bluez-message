//! Notifications « Navi » (serveur pleamar) dans un thread dédié : un serveur lent ne fige pas l'UI.
//! Les actions de la notification d'appel reviennent en [`Cmd`] par `on_cmd`.

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

/// Le texte d'un SMS est contrôlé par l'expéditeur : neutralise le balisage autorisé par la spec.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Rafraîchissement de la bulle de sonnerie : un remplacement relance le délai de la carte pleamar.
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

/// Nom du répertoire et numéro, sinon le numéro seul.
fn caller(c: &Call) -> String {
    match (c.name.as_deref(), c.number.as_deref()) {
        (Some(name), Some(n)) if !n.is_empty() => format!("{name}\n{n}"),
        (_, Some(n)) if !n.is_empty() => n.to_owned(),
        _ => t!("hidden number").to_owned(),
    }
}

/// La bulle de sonnerie (Décrocher / Refuser), remplacée sur place (même id) tant qu'elle sonne.
/// Décroché, sortant : la fenêtre `bluez-msg-call` prend le relais, la bulle se ferme.
struct Bubble<F> {
    on_cmd: F,
    /// Id affiché ; 0 = aucun. Remis à 0 par l'écouteur à la fermeture (bouton, fermeture manuelle).
    id: Arc<AtomicU32>,
    /// Numéro affiché pendant la sonnerie ; `None` : pas de bulle.
    number: Option<String>,
}

impl<F: Fn(Cmd) + Clone + Send + 'static> Bubble<F> {
    fn show(&mut self, c: &Call) {
        if c.phase != Phase::Incoming {
            self.close();
            return;
        }
        let number = caller(c);
        self.display(&number);
        self.number = Some(number);
    }

    /// Garde la carte affichée ; une bulle fermée (bouton, ×) n'est pas recréée.
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
        // Même id : pleamar met la carte à jour sans la refaire tomber ; l'écouteur existant reste valable.
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
                        // Carte fermée (bouton ou ×) : libérée avant la commande, l'état qui en découle en crée une neuve.
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
        assert_eq!(caller(&c), t!("hidden number"));
        c.number = Some(String::new());
        assert_eq!(caller(&c), t!("hidden number"));
        c.number = Some("+331".into());
        assert_eq!(caller(&c), "+331");
        c.name = Some("Bob".into());
        assert_eq!(caller(&c), "Bob\n+331");
    }

}
