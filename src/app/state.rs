//! État de l'interface, sans UI (testable) : conversations, appel, liens.
//! [`State::apply`] intègre un événement du moteur et renvoie les effets à notifier.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::addr::BtAddr;
use crate::engine::{CallKind, Device, Item, Out, Profile};
use crate::history::SENT_ECHO_SECS;
use crate::hfp::{HfEvent, Indicator};
use crate::map::Sms;
use crate::phone::{Query, fold};
use crate::settings::Settings;
use crate::stamp;
use crate::t;

pub use crate::phone::{matches, number_key, sms_peer};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Msg {
    pub text: String,
    /// Horodatage MAP `AAAAMMJJTHHMMSS` (triable tel quel).
    pub timestamp: String,
    pub outgoing: bool,
    /// Envoyé depuis l'app (transfert MAP abouti). iOS expose `sent` vide : sinon remplacé
    /// par l'entrée du dossier `sent` si elle apparaît.
    pub local: bool,
    /// Ligne d'appel (sens, durée en secondes) plutôt qu'un SMS ; `text` vide.
    pub call: Option<(CallKind, u64)>,
    /// Arrivé non lu (SMS reçu, appel manqué) : compté tant qu'il est après la dernière lecture.
    pub fresh: bool,
    pub delivery: Delivery,
    /// Demande d'envoi (bulle `Pending`) : au-delà de [`PENDING_MAX`], elle passe en échec.
    pub asked: Option<Instant>,
}

/// Une bulle restée en attente au-delà (commande refusée, liaison perdue) passe en échec.
pub const PENDING_MAX: std::time::Duration = std::time::Duration::from_secs(120);

/// Suivi d'un envoi fait depuis l'app.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Delivery {
    #[default]
    Done,
    /// Transfert MAP en cours.
    Pending,
    /// Transfert refusé ou en échec : à renvoyer.
    Failed,
}

impl Msg {
    /// Désignation de l'élément pour une suppression.
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
    /// Clé de regroupement (voir [`number_key`]).
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
    /// Nom du répertoire de l'iPhone pour `number`.
    pub name: Option<String>,
    /// Début de la communication (passage en `Active`).
    pub since: Option<Instant>,
    /// Micro du PC coupé par le démon.
    pub muted: bool,
    /// L'appel principal est lui-même en attente.
    pub hold: bool,
    /// Autre appel mis en attente ; second appel qui sonne (numéro vide si masqué).
    pub held: Option<String>,
    pub waiting: Option<String>,
    call: bool,
    setup: u8,
    /// Appel émis depuis l'iPhone (passé par `Outgoing`).
    dialed: bool,
    /// Début (format MAP), pour l'historique.
    started: String,
}

impl Call {
    /// Aucun appel en cours.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.phase == Phase::Idle
    }

    /// Ce qui s'affiche, sans les indicateurs HFP bruts.
    fn view(&self) -> Call {
        Call { call: false, setup: 0, dialed: false, started: String::new(), ..self.clone() }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Sms { from: String, text: String },
    /// Appel en cours (sonnerie, communication, double appel) : vue à afficher, émise à chaque changement.
    Call(Call),
    /// Fin d'appel : ce que l'historique consigne.
    CallEnd(Ended),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub number: String,
    pub timestamp: String,
    pub kind: CallKind,
    pub secs: u64,
}

/// Réglages tels que le démon les publie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefs {
    pub settings: Settings,
    pub langs: Vec<String>,
    pub devices: Vec<Device>,
    pub device: BtAddr,
}

#[derive(Default)]
pub struct State {
    /// Triées par dernier message, le plus récent en tête.
    pub convs: Vec<Conv>,
    pub map_up: bool,
    pub hfp_up: bool,
    pub slc: bool,
    /// Dernière synchronisation des contacts : `None` avant la première tentative.
    pub pbap: Option<bool>,
    pub call: Call,
    pub error: Option<String>,
    /// Compte rendu d'une commande (purge…), affiché sans alarme.
    pub info: Option<String>,
    /// Réglages publiés par le démon (`None` : pas encore reçus).
    pub settings: Option<Prefs>,
    /// `error` vient d'une liaison (`Out::Error`) : effacée au retour d'un lien.
    link_error: bool,
    handles: HashSet<String>,
    /// Répertoire de l'iPhone : clé de numéro → nom.
    names: HashMap<String, String>,
    /// Répertoire complet (nom, numéro, nom replié), trié par nom : recherche d'un correspondant.
    book: Vec<(String, String, String)>,
    /// Lu jusqu'à (horodatage MAP), par conversation ; consigné par le démon.
    reads: HashMap<String, String>,
}

/// Non lus : arrivés non lus après la dernière lecture consignée.
fn unread_count(c: &Conv, read_until: Option<&String>) -> usize {
    c.msgs.iter().filter(|m| m.fresh && read_until.is_none_or(|u| m.timestamp > *u)).count()
}

impl State {
    /// Contacts du répertoire trouvés par `q` (nom, numéro), au plus `max`, hors ceux qui ont déjà une conversation.
    #[must_use]
    pub fn search(&self, q: &str, max: usize) -> Vec<(&str, &str)> {
        if q.trim().is_empty() {
            return vec![];
        }
        let q = Query::new(q);
        self.book
            .iter()
            .filter(|(_, num, folded)| q.hits(folded, num) && self.conv(&number_key(num)).is_none())
            .take(max)
            .map(|(a, b, _)| (a.as_str(), b.as_str()))
            .collect()
    }

    pub fn apply(&mut self, o: Out) -> Vec<Effect> {
        match o {
            Out::Sms { sms, folder, new } => self.on_sms(sms, folder.is_sent(), new),
            Out::Sent { to, text, ok: true, timestamp, .. } => {
                self.take_unsent(&to, &text, true);
                // Déjà revenu par le dossier `sent` (horloge du téléphone) : pas de doublon.
                let echo = self.conv(&number_key(&to)).is_some_and(|c| {
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
                self.names = names.iter().map(|(num, name)| (number_key(num), name.clone())).collect();
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
                // Bannière seulement pour un envoi suivi ici (pas pour un échec ancien rejoué à la connexion).
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
            Out::Status { link: Profile::Pbap, up } => {
                self.pbap = Some(up);
                vec![]
            }
            Out::Status { up: true, .. } if self.link_error => {
                self.error = None;
                self.link_error = false;
                self.apply(o)
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
            Out::Settings { settings, langs, devices, device } => {
                self.settings = Some(Prefs { settings, langs, devices, device });
                vec![]
            }
            // Appel en cours gardé : seul l'historique est vidé.
            Out::Cleared => {
                self.convs.clear();
                self.reads.clear();
                self.handles.clear();
                vec![]
            }
        }
    }

    /// Bulle affichée dès l'envoi demandé, en attente du résultat du transfert.
    /// Un renvoi remplace la bulle en échec du même texte.
    pub fn pending(&mut self, to: &str, text: &str) {
        self.take_unsent(to, text, false);
        let msg = Msg { text: text.to_owned(), timestamp: stamp::now(), outgoing: true, local: true, delivery: Delivery::Pending, asked: Some(Instant::now()), ..Msg::default() };
        self.push(to, "", msg, false);
    }

    /// Bulles restées en attente trop longtemps, ou toutes (`all` : liaison avec le démon perdue) : en échec.
    /// `true` s'il en reste en attente (animation à entretenir).
    pub fn expire_pending(&mut self, all: bool) -> bool {
        let mut left = false;
        for m in self.convs.iter_mut().flat_map(|c| c.msgs.iter_mut()).filter(|m| m.delivery == Delivery::Pending) {
            if all || m.asked.is_none_or(|t| t.elapsed() > PENDING_MAX) {
                m.delivery = Delivery::Failed;
            } else {
                left = true;
            }
        }
        left
    }

    /// Retire la bulle en échec (et, si `pending`, d'abord celle en attente) de ce texte, remplacée par le
    /// résultat de l'envoi ou par un renvoi ; `true` si une bulle en attente a été retirée.
    fn take_unsent(&mut self, to: &str, text: &str, pending: bool) -> bool {
        let key = number_key(to);
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

    /// Retire un élément, ou la conversation entière (`None`) ; une conversation vidée disparaît.
    pub fn delete(&mut self, key: &str, item: Option<&Item>) {
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
    }

    #[must_use]
    pub fn conv(&self, key: &str) -> Option<&Conv> {
        self.convs.iter().find(|c| c.key == key)
    }

    /// Conversation affichée : lue jusqu'à son dernier élément. `Some(until)` si quelque chose était non lu
    /// (à transmettre au démon, qui le consigne).
    pub fn mark_read(&mut self, key: &str) -> Option<String> {
        let c = self.convs.iter().find(|c| c.key == key && c.unread > 0)?;
        // Hors bulle en attente : le démon ne la connaît pas et refuserait une date au-delà de son dernier élément.
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
            // Si le message envoyé depuis l'app revient par le polling, il remplace l'entrée locale.
            let key = number_key(&number);
            if let Some(c) = self.convs.iter_mut().find(|c| c.key == key)
                && let Some(i) = c.msgs.iter().position(|m| m.local && m.text == msg.text)
            {
                c.msgs.remove(i);
            }
        }
        self.push(&number, &name, msg, !sent && !sms.read);
        if new && !sent {
            let from = self.name_of(&number).or((!sms.sender.is_empty()).then_some(sms.sender)).unwrap_or(number);
            return vec![Effect::Sms { from, text: sms.text }];
        }
        vec![]
    }

    /// Nom du répertoire pour ce numéro.
    #[must_use]
    pub fn name_of(&self, number: &str) -> Option<String> {
        self.names.get(&number_key(number)).cloned()
    }

    fn push(&mut self, number: &str, name: &str, msg: Msg, unread: bool) {
        let key = number_key(number);
        let known = self.names.get(&key);
        let name = known.map_or(name, String::as_str);
        let i = if let Some(i) = self.convs.iter().position(|c| c.key == key) {
            i
        } else {
            self.convs.push(Conv { key, number: number.to_owned(), name: String::new(), msgs: vec![], unread: 0 });
            self.convs.len() - 1
        };
        let c = &mut self.convs[i];
        // Le répertoire l'emporte sur le nom MAP.
        if (c.name.is_empty() || known.is_some()) && !name.is_empty() && name != number {
            name.clone_into(&mut c.name);
        }
        let at = c.msgs.iter().position(|m| m.timestamp > msg.timestamp).unwrap_or(c.msgs.len());
        c.msgs.insert(at, Msg { fresh: unread, ..msg });
        c.unread = unread_count(c, self.reads.get(&c.key));
        self.convs.sort_by(|a, b| b.last_stamp().cmp(a.last_stamp()));
    }

    /// Coupe ou rétablit le micro de l'appel en cours (état seulement ; l'audio est géré par le démon).
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
                c.name = self.names.get(&number_key(&number)).cloned();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Folder;

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
        st.apply(sms("3", "07 12 34 56 01", "20261015T120000", "c", true)); // doublon ignoré
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
        // Même texte renvoyé bien plus tard : consigné.
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
        // Échec rejoué (aucun envoi suivi ici) : bulle en échec, pas de bannière.
        st.apply(Out::Sent { to: "+331".into(), text: "x".into(), ok: false, error: Some("MAP".into()), timestamp: String::new() });
        assert_eq!((st.convs[0].msgs[0].delivery, st.error.is_none()), (Delivery::Failed, true));
        st.pending("+331", "x");
        assert_eq!(st.convs[0].msgs.len(), 1, "the resend replaces the failure");
        st.apply(Out::Sent { to: "+331".into(), text: "x".into(), ok: false, error: Some("MAP".into()), timestamp: String::new() });
        assert!(st.error.as_deref().unwrap().contains("MAP"));
        // Deux envois identiques coup sur coup : deux bulles ; perte de liaison : en échec.
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
        assert_eq!(fx, [Effect::Sms { from: "Alice".into(), text: "yo".into() }]);
    }

    fn ind(name: &str, value: u8) -> Out {
        Out::Call(HfEvent::Indicator { name: name.into(), value })
    }

    fn ended(fx: Vec<Effect>) -> Ended {
        match fx.as_slice() {
            [Effect::CallEnd(e)] => e.clone(),
            _ => panic!("fin d'appel attendue : {fx:?}"),
        }
    }

    fn shown(fx: Vec<Effect>) -> Call {
        match fx.as_slice() {
            [Effect::Call(c)] => c.clone(),
            _ => panic!("vue d'appel attendue : {fx:?}"),
        }
    }

    #[test]
    fn incoming_answered_then_ended() {
        let mut st = State::default();
        assert_eq!(shown(st.apply(Out::Call(HfEvent::IncomingCall))).phase, Phase::Incoming);
        st.apply(ind("callsetup", 1));
        assert_eq!(st.apply(Out::Call(HfEvent::Ring)), []);
        assert_eq!(shown(st.apply(Out::Call(HfEvent::Clip { number: "+331".into() }))).number.as_deref(), Some("+331"));
        assert_eq!(st.apply(Out::Call(HfEvent::Clip { number: "+331".into() })), []);
        let v = shown(st.apply(ind("call", 1)));
        assert_eq!((v.phase, v.number.as_deref(), v.since.is_some()), (Phase::Active, Some("+331"), true));
        assert_eq!(st.apply(ind("callsetup", 0)), []);
        // Démarrage en cours d'appel : numéro appris sans sonnerie.
        let mut late = State::default();
        late.apply(ind("call", 1));
        late.apply(ind("callsetup", 0));
        let v = shown(late.apply(Out::Call(HfEvent::Clip { number: "+332".into() })));
        assert_eq!((v.phase, v.number.as_deref()), (Phase::Active, Some("+332")));
        let e = ended(st.apply(ind("call", 0)));
        assert_eq!((e.number.as_str(), e.kind, e.timestamp.len()), ("+331", CallKind::In, 15));
        assert_eq!(st.call, Call::default());
    }

    #[test]
    fn mute_and_second_call() {
        let mut st = State::default();
        assert_eq!(st.set_muted(true), [], "no call: nothing to mute");
        st.apply(ind("call", 1));
        st.apply(Out::Call(HfEvent::Clip { number: "+331".into() }));
        assert!(shown(st.set_muted(true)).muted);
        assert_eq!(st.set_muted(true), []);
        let lines = |hold, held: Option<&str>, waiting: Option<&str>| {
            Out::Call(HfEvent::Lines { hold, held: held.map(Into::into), waiting: waiting.map(Into::into) })
        };
        assert_eq!(shown(st.apply(lines(false, None, Some("+332")))).waiting.as_deref(), Some("+332"));
        // Attente + réponse : le second devient l'appel principal, le premier passe en attente.
        st.apply(Out::Call(HfEvent::Clip { number: "+332".into() }));
        let v = shown(st.apply(lines(false, Some("+331"), None)));
        assert_eq!((v.number.as_deref(), v.held.as_deref(), v.waiting), (Some("+332"), Some("+331"), None));
        // Le second raccroche : il ne reste que le premier, en attente.
        st.apply(Out::Call(HfEvent::Clip { number: "+331".into() }));
        assert!(shown(st.apply(lines(true, None, None))).hold);
        assert_eq!(ended(st.apply(ind("call", 0))).kind, CallKind::In);
        assert!(!st.call.muted);
    }

    #[test]
    fn incoming_rejected_and_hfp_loss() {
        let mut st = State::default();
        st.apply(Out::Call(HfEvent::IncomingCall));
        st.apply(ind("callsetup", 1));
        let e = ended(st.apply(ind("callsetup", 0)));
        assert_eq!((e.kind, e.secs, e.number.as_str()), (CallKind::Missed, 0, ""));
        st.apply(Out::Call(HfEvent::IncomingCall));
        assert_eq!(ended(st.apply(Out::Status { link: Profile::Hfp, up: false })).kind, CallKind::Missed);
        assert_eq!(st.call.phase, Phase::Idle);
    }

    #[test]
    fn outgoing_call() {
        let mut st = State::default();
        assert!(matches!(&st.apply(ind("callsetup", 2))[..], [Effect::Call(c)] if c.phase == Phase::Outgoing));
        assert!(matches!(&st.apply(ind("call", 1))[..], [Effect::Call(c)] if c.phase == Phase::Active && c.since.is_some()));
        st.apply(ind("callsetup", 0));
        assert_eq!(ended(st.apply(ind("call", 0))).kind, CallKind::Out);
    }

    #[test]
    fn reads_survive_replay() {
        let mut st = State::default();
        st.apply(sms("1", "0612345678", "20261015T100000", "a", false));
        st.apply(sms("2", "0612345678", "20261015T110000", "b", false));
        assert_eq!(st.convs[0].unread, 2);
        let until = st.mark_read("612345678").expect("unread to send");
        assert_eq!((until.as_str(), st.convs[0].unread), ("20261015T110000", 0));
        assert_eq!(st.mark_read("612345678"), None, "already read: nothing to send");
        // Client neuf : instantané rejoué (SMS non lus puis lecture) → rien de non lu.
        let mut fresh = State::default();
        fresh.apply(sms("1", "0612345678", "20261015T100000", "a", false));
        fresh.apply(Out::Read { key: "612345678".into(), until: "20261015T100000".into() });
        fresh.apply(sms("2", "0612345678", "20261015T110000", "b", false));
        assert_eq!(fresh.convs[0].unread, 1, "only the message after the read mark");
        fresh.apply(Out::Read { key: "612345678".into(), until: "20261015T090000".into() });
        assert_eq!(fresh.convs[0].unread, 1, "a read mark never moves back");
    }

    #[test]
    fn muted_event_reaches_clients() {
        let mut st = State::default();
        assert_eq!(st.apply(Out::Muted { on: true }), [], "no call");
        st.apply(Out::Call(HfEvent::IncomingCall));
        st.apply(ind("call", 1));
        assert!(shown(st.apply(Out::Muted { on: true })).muted);
        assert!(!shown(st.apply(Out::Muted { on: false })).muted);
    }

    #[test]
    fn contacts_name_convs_calls_and_log() {
        let mut st = State::default();
        st.apply(sms("1", "+33612345678", "20261015T100000", "a", false));
        st.apply(Out::Contacts { names: vec![("612345678".into(), "Bob".into())] });
        assert_eq!(st.convs[0].name, "Bob");
        let v = shown(st.apply(Out::Call(HfEvent::Clip { number: "06 12 34 56 78".into() })));
        assert_eq!(v.name.as_deref(), Some("Bob"));
        st.apply(Out::CallLog { number: "0612345678".into(), timestamp: "20261015T110000".into(), kind: CallKind::Missed, secs: 0, read: false });
        let c = &st.convs[0];
        assert_eq!((c.msgs.len(), c.unread), (2, 2), "unread text + missed call");
        assert_eq!(c.msgs[1].call, Some((CallKind::Missed, 0)));
        // Nouveau SMS : la notification porte le nom du répertoire.
        let mut s = Sms { handle: "9".into(), address: "+33612345678".into(), timestamp: "20261015T120000".into(), text: "b".into(), ..Default::default() };
        s.sender = String::new();
        assert_eq!(st.apply(Out::Sms { sms: s, folder: Folder::Inbox, new: true }), [Effect::Sms { from: "Bob".into(), text: "b".into() }]);
    }

    #[test]
    fn search_by_name_or_number() {
        let mut st = State::default();
        st.apply(Out::Contacts { names: vec![("+33 6 12 34 56 78".into(), "Émile Zola".into()), ("0700000000".into(), "Anne".into())] });
        assert_eq!(st.search("emi", 10), [("Émile Zola", "+33 6 12 34 56 78")], "accents and case ignored");
        assert_eq!(st.search("ZOLA", 10).len(), 1);
        assert_eq!(st.search("06 12", 10).len(), 1, "forme nationale");
        assert_eq!(st.search("+336123", 10).len(), 1, "forme internationale");
        assert_eq!(st.search("0", 10).len(), 2);
        assert_eq!(st.search("0", 1).len(), 1, "bounded");
        assert!(st.search("  ", 10).is_empty());
        assert_eq!(st.search("", 10), []);
        assert_eq!(st.search("a", 10)[0].0, "Anne", "sorted by name");
    }
}
