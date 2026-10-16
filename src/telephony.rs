//! Appels via le service `org.pipewire.Telephony` (bus session) : PipeWire tient le profil HFP
//! (rôle `hfp_hf`) et l'audio SCO ; ce module ne fait que suivre les appels et les piloter.
//! Les états sont traduits en [`HfEvent`] pour garder la machine d'appel de l'interface.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::message::Type;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, Message, MessageStream, fdo};

use crate::addr::BtAddr;
use crate::bluez::prop_str;
use crate::error::{Error, Result};
use crate::hfp::{HfCmd, HfEvent, Indicator, mask};
use crate::phone::clean;
use crate::t;

const SERVICE: &str = "org.pipewire.Telephony";
const ROOT: &str = "/org/pipewire/Telephony";
const AG_IFACE: &str = "org.pipewire.Telephony.AudioGateway1";
const CALL_IFACE: &str = "org.pipewire.Telephony.Call1";
const OM_IFACE: &str = "org.freedesktop.DBus.ObjectManager";
const RETRY: Duration = Duration::from_secs(5);
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CALLS: usize = 8;
const MAX_NUMBER: usize = 64;

type Props = HashMap<String, OwnedValue>;
type Objects = HashMap<OwnedObjectPath, HashMap<String, Props>>;

/// AG dont `Address` correspond à `addr` (casse ignorée : l'adresse lue est normalisée).
fn pick_ag(objs: &Objects, addr: &BtAddr) -> Option<OwnedObjectPath> {
    objs.iter()
        .find(|(_, ifs)| ifs.get(AG_IFACE).and_then(|p| prop_str(p, "Address")).is_some_and(|a| a.parse::<BtAddr>().is_ok_and(|a| a == *addr)))
        .map(|(path, _)| path.clone())
}

/// État `State` d'un appel Telephony ; état inconnu gardé tel quel (borné).
#[derive(Debug, Clone, PartialEq, Eq)]
enum CallState {
    Incoming,
    Dialing,
    Alerting,
    Active,
    Held,
    Waiting,
    Disconnected,
    Other(String),
}

impl CallState {
    fn parse(s: &str) -> Self {
        match s {
            "incoming" => Self::Incoming,
            "dialing" => Self::Dialing,
            "alerting" => Self::Alerting,
            "active" => Self::Active,
            "held" => Self::Held,
            "waiting" => Self::Waiting,
            "disconnected" => Self::Disconnected,
            _ => Self::Other(s.chars().take(MAX_NUMBER).collect()),
        }
    }

    /// Priorité pour l'appel affiché ; `None` : non affiché (`waiting`, inconnu).
    fn rank(&self) -> Option<u8> {
        match self {
            Self::Active => Some(0),
            Self::Held => Some(1),
            Self::Dialing => Some(2),
            Self::Alerting => Some(3),
            Self::Incoming => Some(4),
            _ => None,
        }
    }

    /// En communication (décroché, éventuellement mis en attente).
    fn is_up(&self) -> bool {
        matches!(self, Self::Active | Self::Held)
    }
}

fn ind(name: Indicator, value: u8) -> HfEvent {
    HfEvent::Indicator { name, value }
}

/// Événements d'un état d'appel ; `None` pour un état sans effet (`waiting`, inconnu).
fn call_events(state: &CallState, number: &str) -> Option<Vec<HfEvent>> {
    Some(match state {
        CallState::Incoming if number.is_empty() => vec![HfEvent::IncomingCall],
        CallState::Incoming => vec![HfEvent::IncomingCall, HfEvent::Clip { number: number.into() }],
        CallState::Dialing => vec![ind(Indicator::CallSetup, 2)],
        CallState::Alerting => vec![ind(Indicator::CallSetup, 3)],
        // Ordre HFP : call=1 avant callsetup=0, sinon l'état client clôt l'appel et perd le numéro ;
        // le numéro suit, pour un client qui découvre l'appel déjà en cours.
        CallState::Active | CallState::Held => vec![ind(Indicator::Call, 1), ind(Indicator::CallSetup, 0), HfEvent::Clip { number: number.into() }],
        CallState::Disconnected => end_events(),
        CallState::Waiting | CallState::Other(_) => return None,
    })
}

fn end_events() -> Vec<HfEvent> {
    vec![ind(Indicator::CallSetup, 0), ind(Indicator::Call, 0)]
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Call {
    state: CallState,
    number: String,
}

/// État suivi : AG retenu, ses appels, et l'appel dernièrement signalé (seuls ses changements émettent).
#[derive(Debug, Default)]
struct Tracker {
    ag: Option<OwnedObjectPath>,
    calls: HashMap<OwnedObjectPath, Call>,
    shown: Option<Call>,
    /// Derniers appels secondaires signalés (mis en attente, en attente de réponse).
    lines: Lines,
}

/// Appels secondaires : l'appel affiché est en attente, numéro mis en attente, numéro qui attend une réponse.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Lines {
    hold: bool,
    held: Option<String>,
    waiting: Option<String>,
}

impl Lines {
    fn event(&self) -> HfEvent {
        HfEvent::Lines { hold: self.hold, held: self.held.clone(), waiting: self.waiting.clone() }
    }
}

impl Tracker {
    fn owns_call(&self, path: &str) -> bool {
        self.ag.as_ref().is_some_and(|ag| path.strip_prefix(ag.as_str()).is_some_and(|r| r.starts_with('/')))
    }

    /// Événements du passage de l'appel affiché précédent à l'appel prioritaire courant.
    fn emit(&mut self) -> Vec<HfEvent> {
        let cur = self
            .calls
            .values()
            .filter_map(|c| c.state.rank().map(|r| (r, c)))
            .min_by(|(ra, a), (rb, b)| ra.cmp(rb).then_with(|| a.number.cmp(&b.number)))
            .map(|(_, c)| c.clone());
        // Secondaires : un appel mis en attente derrière l'appel affiché, un second appel qui sonne.
        let other = |state: CallState| self.calls.values().filter(|c| c.state == state).map(|c| c.number.clone()).min();
        let lines = match &cur {
            Some(c) if c.state == CallState::Held => Lines { hold: true, held: None, waiting: other(CallState::Waiting) },
            Some(_) => Lines { hold: false, held: other(CallState::Held), waiting: other(CallState::Waiting) },
            None => Lines::default(),
        };
        let mut evs = vec![];
        if cur != self.shown {
            let up = |c: &Option<Call>| c.as_ref().is_some_and(|c| c.state.is_up());
            evs = match &cur {
                // Plus d'appel en communication, un autre sonne ou compose : clore d'abord l'ancien.
                Some(c) if up(&self.shown) && !up(&cur) => {
                    let mut e = end_events();
                    e.extend(call_events(&c.state, &c.number).unwrap_or_default());
                    e
                }
                Some(c) => call_events(&c.state, &c.number).unwrap_or_default(),
                None => end_events(),
            };
            self.shown = cur;
        }
        if lines != self.lines {
            evs.push(lines.event());
            self.lines = lines;
        }
        evs
    }

    fn set_ag(&mut self, path: OwnedObjectPath) -> Vec<HfEvent> {
        self.ag = Some(path);
        self.calls.clear();
        self.shown = None;
        self.lines = Lines::default();
        vec![HfEvent::Connected, HfEvent::SlcReady]
    }

    /// Perte de l'AG ou du service : fin d'appel éventuelle puis `Disconnected`.
    fn lose_ag(&mut self) -> Vec<HfEvent> {
        if self.ag.take().is_none() {
            return vec![];
        }
        self.calls.clear();
        let mut evs = self.emit();
        evs.push(HfEvent::Disconnected);
        evs
    }

    fn upsert_call(&mut self, path: OwnedObjectPath, p: &Props) -> Vec<HfEvent> {
        if !self.owns_call(path.as_str()) {
            return vec![];
        }
        let state = prop_str(p, "State");
        // Appel inconnu : créé seulement sur un état réel, sinon une entrée `Other("")` resterait sans retrait.
        if !self.calls.contains_key(&path) && (matches!(state, None | Some("disconnected")) || self.calls.len() >= MAX_CALLS) {
            return vec![];
        }
        let call = self.calls.entry(path.clone()).or_insert_with(|| Call { state: CallState::Other(String::new()), number: String::new() });
        if let Some(n) = prop_str(p, "LineIdentification") {
            call.number = clean(n, false).chars().take(MAX_NUMBER).collect();
        }
        if let Some(s) = state {
            call.state = CallState::parse(s);
        }
        tracing::debug!(path = path.as_str(), state = ?call.state, number = mask(&call.number), "call");
        if call.state == CallState::Disconnected {
            self.calls.remove(&path);
        }
        self.emit()
    }

    fn remove_call(&mut self, path: &OwnedObjectPath) -> Vec<HfEvent> {
        if self.calls.remove(path).is_none() {
            return vec![];
        }
        self.emit()
    }

    fn on_signal(&mut self, m: &Message, addr: &BtAddr) -> Vec<HfEvent> {
        let h = m.header();
        let (Some(iface), Some(member), Some(path)) = (h.interface(), h.member(), h.path()) else { return vec![] };
        match (iface.as_str(), member.as_str()) {
            (OM_IFACE, "InterfacesAdded") => {
                let Ok((obj, ifs)) = m.body().deserialize::<(OwnedObjectPath, HashMap<String, Props>)>() else { return vec![] };
                if let Some(p) = ifs.get(CALL_IFACE) {
                    return self.upsert_call(obj, p);
                }
                if self.ag.is_none() && pick_ag(&HashMap::from([(obj.clone(), ifs)]), addr).is_some() {
                    return self.set_ag(obj);
                }
                vec![]
            }
            (OM_IFACE, "InterfacesRemoved") => {
                let Ok((obj, ifs)) = m.body().deserialize::<(OwnedObjectPath, Vec<String>)>() else { return vec![] };
                if self.ag.as_ref() == Some(&obj) && ifs.iter().any(|i| i == AG_IFACE) {
                    self.lose_ag()
                } else if ifs.iter().any(|i| i == CALL_IFACE) {
                    self.remove_call(&obj)
                } else {
                    vec![]
                }
            }
            ("org.freedesktop.DBus.Properties", "PropertiesChanged") => {
                let Ok((changed_iface, props, _)) = m.body().deserialize::<(String, Props, Vec<String>)>() else { return vec![] };
                if changed_iface != CALL_IFACE {
                    return vec![];
                }
                self.upsert_call(OwnedObjectPath::from(path.to_owned()), &props)
            }
            _ => vec![],
        }
    }

    /// Réaligne sur l'état du service, en n'émettant que les différences ; `Err` si le service est absent.
    async fn sync(&mut self, conn: &Connection, addr: &BtAddr) -> Result<Vec<HfEvent>> {
        let found = pick_ag(&managed(conn, ROOT).await?, addr);
        let mut evs = vec![];
        if found != self.ag {
            evs.extend(self.lose_ag());
            if let Some(ag) = found {
                evs.extend(self.set_ag(ag));
            }
        }
        let Some(ag) = self.ag.clone() else { return Ok(evs) };
        match managed(conn, ag.as_str()).await {
            Ok(objs) => {
                self.calls.retain(|p, _| objs.get(p).is_some_and(|ifs| ifs.contains_key(CALL_IFACE)));
                for (path, ifs) in objs {
                    if let Some(p) = ifs.get(CALL_IFACE) {
                        evs.extend(self.upsert_call(path, p));
                    }
                }
                evs.extend(self.emit());
            }
            Err(e) => tracing::warn!("calls of {} unreadable: {e}", ag.as_str()),
        }
        Ok(evs)
    }

    /// Premier appel trouvé dans l'ordre de priorité `states` (pas l'ordre de la table).
    fn find(&self, states: &[CallState]) -> Option<OwnedObjectPath> {
        states.iter().find_map(|s| self.calls.iter().filter(|(_, c)| c.state == *s).map(|(p, _)| p).min_by_key(|p| p.as_str()).cloned())
    }

    async fn exec(&self, conn: &Connection, cmd: HfCmd) -> Result<()> {
        let none = |what: &str| Err(Error::Hfp(what.into()));
        let ag = || self.ag.clone().ok_or_else(|| Error::Hfp(t!("no phone").into()));
        let (path, iface, method) = match cmd {
            HfCmd::Answer => match self.find(&[CallState::Incoming]) {
                Some(p) => (p, CALL_IFACE, "Answer"),
                None => return none(t!("no incoming call")),
            },
            HfCmd::Reject => match self.find(&[CallState::Waiting, CallState::Incoming]) {
                Some(p) => (p, CALL_IFACE, "Hangup"),
                None => return none(t!("no call to reject")),
            },
            HfCmd::Hangup => match self.find(&[CallState::Active, CallState::Dialing, CallState::Alerting, CallState::Held, CallState::Incoming]) {
                Some(p) => (p, CALL_IFACE, "Hangup"),
                None => (ag()?, AG_IFACE, "HangupAll"),
            },
            HfCmd::HoldAnswer => (ag()?, AG_IFACE, "HoldAndAnswer"),
            HfCmd::Swap => (ag()?, AG_IFACE, "SwapCalls"),
            HfCmd::Dial(n) => return call_with(conn, ag()?.as_str(), AG_IFACE, "Dial", &(n.as_str(),)).await.map(drop),
        };
        call(conn, path.as_str(), iface, method).await?;
        Ok(())
    }
}

/// Appel de méthode borné dans le temps : un service figé ne bloque pas la boucle.
async fn call(conn: &Connection, path: &str, iface: &str, method: &str) -> Result<Message> {
    call_with(conn, path, iface, method, &()).await
}

async fn call_with<B>(conn: &Connection, path: &str, iface: &str, method: &str, body: &B) -> Result<Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    tokio::time::timeout(CALL_TIMEOUT, conn.call_method(Some(SERVICE), path, Some(iface), method, body))
        .await
        .map_err(|_| Error::Hfp(t!("{0}: timed out", method)))?
        .map_err(Error::from)
}

async fn managed(conn: &Connection, path: &str) -> Result<Objects> {
    Ok(call(conn, path, OM_IFACE, "GetManagedObjects").await?.body().deserialize::<Objects>()?)
}

/// Suit les appels de `addr` jusqu'à la fermeture de `cmds` (`Ok`). `on_event` reçoit `Connected`/`SlcReady`
/// à l'apparition du téléphone, puis les événements d'appel ; `on_warn` signale une fois l'absence du service.
/// Sur `Err`, la fin d'appel et `Disconnected` ont déjà été émis.
///
/// # Errors
/// Bus session injoignable ou abonnement refusé (fin d'appel et `Disconnected` déjà émis).
pub async fn run(
    addr: &BtAddr,
    cmds: &mut mpsc::Receiver<HfCmd>,
    mut on_event: impl FnMut(&HfEvent),
    mut on_warn: impl FnMut(String),
) -> Result<()> {
    let conn = Connection::session().await?;
    // Abonnements avant la synchro : aucun signal perdu entre les deux. Le filtre `sender` d'une règle
    // ne vaut que pour le nom unique : l'émetteur est contrôlé contre le propriétaire courant.
    let dbus = fdo::DBusProxy::new(&conn).await?;
    let mut owners = dbus.receive_name_owner_changed_with_args(&[(0, SERVICE)]).await?;
    let rule = MatchRule::builder().msg_type(Type::Signal).path_namespace(ROOT)?.build();
    let mut signals = MessageStream::for_match_rule(rule, &conn, Some(64)).await?;
    let mut retry = tokio::time::interval(RETRY);
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut t = Tracker::default();
    let mut owner: Option<String> = None;
    let (mut need_sync, mut present, mut warned) = (true, false, false);
    let r = loop {
        if need_sync {
            need_sync = false;
            owner = dbus.get_name_owner(zbus::names::WellKnownName::from_static_str_unchecked(SERVICE).into()).await.ok().map(|o| o.to_string());
            let synced = match owner {
                Some(_) => t.sync(&conn, addr).await,
                None => Err(Error::Hfp(t!("service missing").into())),
            };
            match synced {
                Ok(evs) => {
                    present = true;
                    warned = false;
                    evs.iter().for_each(&mut on_event);
                }
                Err(e) => {
                    present = false;
                    retry.reset();
                    tracing::debug!("Telephony: {e}");
                    if !warned {
                        warned = true;
                        on_warn(t!("org.pipewire.Telephony service missing: enable bluez5.roles hfp_hf and bluez5.telephony-dbus-service in WirePlumber").to_owned());
                    }
                }
            }
        }
        tokio::select! {
            m = signals.next() => match m {
                Some(Ok(m)) if m.header().sender().map(zbus::names::UniqueName::as_str) == owner.as_deref() => {
                    t.on_signal(&m, addr).iter().for_each(&mut on_event);
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => tracing::debug!("D-Bus signal: {e}"),
                None => break Err(Error::Hfp(t!("D-Bus stream closed").into())),
            },
            o = owners.next() => {
                if o.is_none() {
                    break Err(Error::Hfp(t!("D-Bus stream closed").into()));
                }
                t.lose_ag().iter().for_each(&mut on_event);
                need_sync = true;
            }
            c = cmds.recv() => match c {
                None => break Ok(()),
                Some(c) => if let Err(e) = t.exec(&conn, c).await {
                    tracing::warn!("call command: {e}");
                    on_warn(t!("call command: {0}", e));
                },
            },
            _ = retry.tick(), if !present => need_sync = true,
        }
    };
    if r.is_err() {
        t.lose_ag().iter().for_each(&mut on_event);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn path(s: &str) -> OwnedObjectPath {
        OwnedObjectPath::try_from(s).unwrap()
    }

    fn props(kv: &[(&str, &str)]) -> Props {
        kv.iter().map(|(k, v)| ((*k).to_owned(), OwnedValue::try_from(Value::from(*v)).unwrap())).collect()
    }

    fn ind(name: &str, value: u8) -> HfEvent {
        super::ind(name.into(), value)
    }

    #[test]
    fn picks_ag_by_address() {
        let objs: Objects = HashMap::from([
            (path("/org/pipewire/Telephony/ag1"), HashMap::from([(AG_IFACE.to_owned(), props(&[("Address", "11:22:33:44:55:66")]))])),
            (path("/org/pipewire/Telephony/ag2"), HashMap::from([(AG_IFACE.to_owned(), props(&[("Address", "AA:BB:CC:DD:00:70")]))])),
        ]);
        let addr = |a: &str| a.parse::<BtAddr>().unwrap();
        assert_eq!(pick_ag(&objs, &addr("aa:bb:cc:dd:00:70")), Some(path("/org/pipewire/Telephony/ag2")));
        assert_eq!(pick_ag(&objs, &addr("00:00:00:00:00:00")), None);
    }

    #[test]
    fn maps_call_states() {
        let ev = |s: &str, n: &str| call_events(&CallState::parse(s), n);
        assert_eq!(ev("incoming", "+331"), Some(vec![HfEvent::IncomingCall, HfEvent::Clip { number: "+331".into() }]));
        assert_eq!(ev("incoming", ""), Some(vec![HfEvent::IncomingCall]));
        assert_eq!(ev("alerting", ""), Some(vec![ind("callsetup", 3)]));
        assert_eq!(ev("active", "+331"), Some(vec![ind("call", 1), ind("callsetup", 0), HfEvent::Clip { number: "+331".into() }]));
        assert_eq!(ev("active", ""), Some(vec![ind("call", 1), ind("callsetup", 0), HfEvent::Clip { number: String::new() }]));
        assert_eq!(ev("waiting", "+331"), None);
        assert_eq!(ev("ringing?", "+331"), None);
    }

    #[test]
    fn call_lifecycle() {
        let mut t = Tracker::default();
        assert_eq!(t.set_ag(path("/org/pipewire/Telephony/ag1")), [HfEvent::Connected, HfEvent::SlcReady]);
        // Appel d'un autre AG : ignoré.
        assert!(t.upsert_call(path("/org/pipewire/Telephony/ag10/call1"), &props(&[("State", "incoming")])).is_empty());
        let c = path("/org/pipewire/Telephony/ag1/call1");
        let evs = t.upsert_call(c.clone(), &props(&[("LineIdentification", "+331"), ("State", "incoming")]));
        assert_eq!(evs, [HfEvent::IncomingCall, HfEvent::Clip { number: "+331".into() }]);
        assert_eq!(t.upsert_call(c.clone(), &props(&[("State", "active")])), [ind("call", 1), ind("callsetup", 0), HfEvent::Clip { number: "+331".into() }]);
        assert_eq!(t.upsert_call(c.clone(), &props(&[("State", "disconnected")])), end_events());
        // Retrait après `disconnected` : pas de second événement de fin.
        assert!(t.remove_call(&c).is_empty());
        assert_eq!(t.lose_ag(), [HfEvent::Disconnected]);
        assert!(t.lose_ag().is_empty());
    }

    #[test]
    fn shows_highest_priority_call() {
        let mut t = Tracker::default();
        t.set_ag(path("/org/pipewire/Telephony/ag1"));
        let (a, b) = (path("/org/pipewire/Telephony/ag1/call1"), path("/org/pipewire/Telephony/ag1/call2"));
        t.upsert_call(a.clone(), &props(&[("State", "active")]));
        // Second appel entrant : l'appel actif reste affiché, rien n'est émis.
        assert!(t.upsert_call(b.clone(), &props(&[("LineIdentification", "+332"), ("State", "incoming")])).is_empty());
        // Propriété inchangée : pas de réémission.
        assert!(t.upsert_call(a.clone(), &props(&[("State", "active")])).is_empty());
        let mut want = end_events();
        want.extend([HfEvent::IncomingCall, HfEvent::Clip { number: "+332".into() }]);
        assert_eq!(t.remove_call(&a), want);
        // Fin inconnue : ignorée.
        assert!(t.upsert_call(path("/org/pipewire/Telephony/ag1/call9"), &props(&[("State", "disconnected")])).is_empty());
        // Propriété sans état sur un appel inconnu : pas d'entrée fantôme.
        assert!(t.upsert_call(path("/org/pipewire/Telephony/ag1/call8"), &props(&[("LineIdentification", "+338")])).is_empty());
        assert!(!t.calls.contains_key(&path("/org/pipewire/Telephony/ag1/call8")));
    }

    #[test]
    fn losing_ag_ends_call() {
        let mut t = Tracker::default();
        t.set_ag(path("/org/pipewire/Telephony/ag1"));
        t.upsert_call(path("/org/pipewire/Telephony/ag1/call1"), &props(&[("State", "active")]));
        let mut want = end_events();
        want.push(HfEvent::Disconnected);
        assert_eq!(t.lose_ag(), want);
    }

    #[test]
    fn second_call_lines() {
        let mut t = Tracker::default();
        t.set_ag(path("/org/pipewire/Telephony/ag1"));
        let (a, b) = (path("/org/pipewire/Telephony/ag1/call1"), path("/org/pipewire/Telephony/ag1/call2"));
        t.upsert_call(a.clone(), &props(&[("LineIdentification", "+331"), ("State", "active")]));
        let lines = |hold, held: Option<&str>, waiting: Option<&str>| HfEvent::Lines { hold, held: held.map(Into::into), waiting: waiting.map(Into::into) };
        assert_eq!(t.upsert_call(b.clone(), &props(&[("LineIdentification", "+332"), ("State", "waiting")])), [lines(false, None, Some("+332"))]);
        assert_eq!(t.find(&[CallState::Waiting, CallState::Incoming]), Some(b.clone()));
        // Attente + répondre : a passe en attente, b en communication.
        t.upsert_call(a.clone(), &props(&[("State", "held")]));
        let evs = t.upsert_call(b.clone(), &props(&[("State", "active")]));
        assert_eq!(evs.last(), Some(&lines(false, Some("+331"), None)));
        assert!(evs.contains(&HfEvent::Clip { number: "+332".into() }));
        // Raccrocher vise l'appel actif, pas celui en attente.
        assert_eq!(t.find(&[CallState::Active, CallState::Dialing, CallState::Alerting, CallState::Held, CallState::Incoming]), Some(b.clone()));
        let evs = t.remove_call(&b);
        assert_eq!(evs.last(), Some(&lines(true, None, None)));
        assert_eq!(t.remove_call(&a).last(), Some(&lines(false, None, None)));
    }
}
