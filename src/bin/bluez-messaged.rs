//! `bluez-messaged` : démon SMS/appels de l'iPhone. Tient le moteur MAP + appels, notifie au nom de Navi
//! (même fenêtre fermée) et sert l'état aux clients (GUI) sur `$XDG_RUNTIME_DIR/bluez-message.sock`.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use bluez_message::addr::BtAddr;
use bluez_message::app::notify;
use bluez_message::app::state::{Effect, Phase, State};
use bluez_message::config;
use bluez_message::engine::{Cmd, Engine, Item, Nudge, Out, Profile};
use bluez_message::history::History;
use bluez_message::settings::Settings;
use bluez_message::{bluez, i18n, ipc, pbap, t};
use clap::Parser;
use futures_util::StreamExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};

/// SMS et appels rejoués au démarrage (les plus récents).
const HISTORY: usize = 1000;
/// Fréquence d'application de la durée de conservation.
const RETENTION_EVERY: Duration = Duration::from_hours(24);
/// Délai pendant lequel un nouvel appel sortant est refusé après une numérotation.
const DIAL_GUARD: Duration = Duration::from_secs(5);
/// Délai maximal d'une reconnexion demandée depuis les Paramètres.
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Première lecture du répertoire, après le démarrage des profils.
const CONTACTS_DELAY: Duration = Duration::from_secs(10);
const CONTACTS_EVERY: Duration = Duration::from_hours(6);
/// Espacé : chaque essai peut redemander l'autorisation sur l'iPhone.
const CONTACTS_RETRY: Duration = Duration::from_mins(30);
const WATCH_EVERY: Duration = Duration::from_secs(20);
/// Nouvel essai quand l'appareil BlueZ est introuvable au démarrage.
const WATCH_INIT_RETRY: Duration = Duration::from_secs(300);
/// Constats en défaut avant une relance.
const WATCH_STRIKES: u32 = 2;
const WATCH_PAUSE: Duration = Duration::from_secs(120);
const WATCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Pause entre `Disconnect` et `Connect` d'une liaison refaite.
const WATCH_RESET_GAP: Duration = Duration::from_secs(2);
/// Intervalle des `Connect` quand l'iPhone n'est pas relié (réglage `auto_connect`).
const WATCH_CONNECT_EVERY: Duration = Duration::from_secs(60);
/// Relances sans retour à la normale avant abandon (HFP volontairement absent, iPhone pris ailleurs…).
const WATCH_TRIES: u32 = 3;
const WPCTL_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Parser)]
#[command(about = "iPhone SMS/call relay daemon (Navi notifications, socket for the GUI)")]
struct Args {
    /// iPhone Bluetooth address; default: contents of ~/.config/bluez-message/device.
    addr: Option<BtAddr>,
    /// MAP polling period in seconds.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(2..=300))]
    interval: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    bluez_message::log::init();
    let args = Args::parse();
    match run(args).await {
        Ok(Exit::Done) => ExitCode::SUCCESS,
        // Nouvel iPhone : le même binaire repart (même PID sous systemd) et relit `device`.
        Ok(Exit::Restart) => {
            use std::os::unix::process::CommandExt;
            let e = std::env::current_exe().map(|exe| std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec());
            eprintln!("bluez-messaged: restart: {}", e.map_or_else(|e| e.to_string(), |e| e.to_string()));
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("bluez-messaged: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Issue de [`run`].
enum Exit {
    Done,
    /// Appareil changé : relancer le démon.
    Restart,
}

async fn run(args: Args) -> Result<Exit, String> {
    let fixed_addr = args.addr.is_some();
    let addr = config::resolve_addr(args.addr)?;
    let settings = Settings::load();
    let path = config::socket_path()?;
    let bound = ipc::bind(&path).await.map_err(|e| e.to_string())?;
    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let history = open_history()?;
    let (engine, mut events) = Engine::start(&addr, Duration::from_secs(args.interval));

    let (req_tx, mut requests) = mpsc::channel::<ipc::Request>(64);
    let hub = ipc::spawn_server(bound, req_tx.clone());
    let (fx_tx, fx_rx) = std_mpsc::channel();
    notify::spawn(fx_rx, move |cmd| {
        let _ = req_tx.try_send((cmd, None));
    });
    tracing::info!(socket = %path.display(), "daemon ready");

    let (links_tx, links_rx) = watch::channel(Links::default());
    let mut d = Daemon {
        addr,
        fixed_addr,
        settings,
        history,
        state: State::default(),
        hub,
        engine,
        fx_tx,
        links_tx,
        mic: None,
        dialing: None,
        popin: Popin::default(),
        reconnecting: Arc::default(),
    };
    d.sync_links();
    d.start().await?;
    let mut daily = tokio::time::interval_at(tokio::time::Instant::now() + RETENTION_EVERY, RETENTION_EVERY);
    let (ct_tx, mut contacts) = mpsc::channel(1);
    tokio::spawn(sync_contacts(d.addr.clone(), ct_tx));
    tokio::spawn(link_watch(d.addr.clone(), links_rx, d.engine.nudger()));
    let r = loop {
        tokio::select! {
            o = events.recv() => {
                let Some(o) = o else { break Err(t!("engine stopped").to_owned()) };
                d.on_event(o).await;
            }
            Some(list) = contacts.recv() => d.on_contacts(list.as_deref()),
            r = requests.recv() => {
                let Some((cmd, reply)) = r else { break Err(t!("command channel closed").to_owned()) };
                if let Some(exit) = d.on_request(cmd, reply).await {
                    break Ok(exit);
                }
            }
            _ = daily.tick() => d.apply_retention(),
            _ = term.recv() => break Ok(Exit::Done),
            _ = int.recv() => break Ok(Exit::Done),
        }
    };
    d.shutdown(&path).await;
    r
}

/// Historique SQLite ; base illisible : le démon tourne quand même (historique en mémoire) plutôt que de boucler sous systemd.
fn open_history() -> Result<History, String> {
    let db = config::data_dir().ok_or(t!("HOME not set"))?.join("bluez-message/history.db");
    History::open(&db)
        .or_else(|e| {
            tracing::error!("history {}: {e}; in-memory history for this session", db.display());
            History::memory()
        })
        .map_err(|e| e.to_string())
}

/// État du démon : moteur, historique, état d'appel et clients ; une méthode par événement ou commande.
struct Daemon {
    addr: BtAddr,
    /// Adresse donnée en argument : le changement d'appareil est refusé.
    fixed_addr: bool,
    settings: Settings,
    history: History,
    state: State,
    hub: ipc::Hub,
    engine: Engine,
    /// Effets vers les notifications Navi.
    fx_tx: std_mpsc::Sender<Effect>,
    /// État vu par la surveillance de liaison.
    links_tx: watch::Sender<Links>,
    /// Micro du PC coupé par la bulle : restauré en fin d'appel.
    mic: Option<MicHold>,
    /// Dernière numérotation, pour [`DIAL_GUARD`].
    dialing: Option<Instant>,
    popin: Popin,
    /// Reconnexion demandée depuis les Paramètres en cours (une seule à la fois).
    reconnecting: Arc<AtomicBool>,
}

impl Daemon {
    /// Publie aux clients, l'état d'appel courant décidant de ce que l'instantané garde.
    fn publish(&self, o: Out) {
        self.hub.publish(o, self.state.call.is_idle());
    }

    /// Applique à l'état du démon, puis publie.
    fn apply_publish(&mut self, o: Out) {
        self.state.apply(o.clone());
        self.publish(o);
    }

    /// Réglages, rétention, puis l'historique local : l'instantané le rejoue à chaque client.
    async fn start(&mut self) -> Result<(), String> {
        self.hub.publish(settings_out(&self.settings, &self.addr).await, true);
        if let Some(n) = self.purge_old() {
            tracing::info!(rows = n, "history: retention applied");
        }
        for o in self.history.load(HISTORY).map_err(|e| t!("history: {0}", e))? {
            self.state.apply(o.clone());
            self.hub.publish(o, true);
        }
        Ok(())
    }

    async fn shutdown(self, socket: &Path) {
        self.engine.shutdown().await;
        // Socket retiré avant la libération du verrou (détenu par `hub`).
        let _ = std::fs::remove_file(socket);
        drop(self.hub);
        if let Some(h) = self.mic {
            h.restore().await;
        }
    }

    async fn on_event(&mut self, o: Out) {
        if !admit(&self.history, &o) {
            return;
        }
        let mut logs = vec![];
        for fx in self.state.apply(o.clone()) {
            if let Effect::CallEnd(e) = &fx
                && !e.number.is_empty()
            {
                if let Err(err) = self.history.add_call(&e.number, &e.timestamp, e.kind, e.secs) {
                    tracing::warn!("call history: {err}");
                }
                logs.push(Out::CallLog { number: e.number.clone(), timestamp: e.timestamp.clone(), kind: e.kind, secs: e.secs, read: false });
            }
            if let Effect::Call(c) = &fx
                && matches!(c.phase, Phase::Active | Phase::Outgoing)
            {
                self.popin.ensure();
            }
            if let Some(fx) = for_notify(fx, &self.settings) {
                let _ = self.fx_tx.send(fx);
            }
        }
        if self.state.call.is_idle()
            && let Some(h) = self.mic.take()
        {
            h.restore().await;
        }
        self.publish(o);
        for log in logs {
            self.state.apply(log.clone());
            self.hub.publish(log, true);
        }
        self.sync_links();
    }

    /// Transmet à la surveillance de liaison ce qu'elle doit savoir (appel, HFP, réglage).
    fn sync_links(&self) {
        let now = Links { hfp: self.state.hfp_up, call: !self.state.call.is_idle(), auto_connect: self.settings.auto_connect };
        self.links_tx.send_if_modified(|l| std::mem::replace(l, now) != now);
    }

    /// `None` : synchronisation échouée (refus, répertoire vide), contacts consignés gardés.
    fn on_contacts(&mut self, list: Option<&[(String, String)]>) {
        let ok = match list.map(|l| self.history.replace_contacts(l)) {
            Some(Ok(names)) => {
                tracing::info!(contacts = names.len(), "iPhone contacts synced");
                self.apply_publish(Out::Contacts { names });
                true
            }
            Some(Err(e)) => {
                tracing::warn!("contacts: {e}");
                false
            }
            None => false,
        };
        self.apply_publish(Out::Status { link: Profile::Pbap, up: ok });
    }

    /// Commande d'un client (ou d'une notification) ; `Some` : le démon doit s'arrêter ou repartir.
    async fn on_request(&mut self, cmd: Cmd, reply: Option<mpsc::Sender<Out>>) -> Option<Exit> {
        let res = match cmd {
            Cmd::Mute => self.mute().await,
            Cmd::MarkRead { key, until } => self.mark_read(key, until),
            cmd @ Cmd::Dial { .. } => self.dial(cmd),
            Cmd::Delete { key, item } => self.delete(key, item),
            Cmd::Settings { settings } => self.set_settings(settings).await,
            Cmd::Reconnect => self.reconnect(reply.clone()),
            Cmd::SetDevice { addr } => match self.set_device(&addr).await {
                Ok(true) => return Some(Exit::Restart),
                Ok(false) => Ok(()),
                Err(e) => Err(e),
            },
            Cmd::PurgeHidden => self.purge_hidden(reply.as_ref()),
            Cmd::ClearHistory => self.clear_history(),
            cmd => self.engine.submit(cmd),
        };
        if let Err(msg) = res {
            tracing::warn!("command rejected: {msg}");
            if let Some(reply) = reply {
                let _ = reply.try_send(Out::Error { msg });
            }
        }
        None
    }

    async fn mute(&mut self) -> Result<(), String> {
        for f in toggle_mute(&mut self.state, &mut self.mic).await? {
            let _ = self.fx_tx.send(f);
        }
        self.publish(Out::Muted { on: self.state.call.muted });
        Ok(())
    }

    /// Seulement une conversation connue, jusqu'à son dernier élément au plus : `reads` reste borné.
    fn mark_read(&mut self, key: String, until: String) -> Result<(), String> {
        if !self.state.conv(&key).is_some_and(|c| until.get(..15) <= c.last_stamp().get(..15)) {
            return Err(t!("mark as read: unknown conversation or future date").to_owned());
        }
        if let Err(e) = self.history.mark_read(&key, &until) {
            tracing::warn!("history (read): {e}");
        }
        self.apply_publish(Out::Read { key, until });
        Ok(())
    }

    /// `phase` reste `Idle` jusqu'au premier événement HFP : une numérotation récente bloque aussi.
    fn dial(&mut self, cmd: Cmd) -> Result<(), String> {
        if !self.state.hfp_up || !self.state.call.is_idle() || self.dialing.is_some_and(|t| t.elapsed() < DIAL_GUARD) {
            return Err(t!("cannot call: phone not linked or a call is already in progress").to_owned());
        }
        self.engine.submit(cmd).inspect(|()| self.dialing = Some(Instant::now()))
    }

    /// Historique local seulement : rien n'est effacé sur l'iPhone.
    fn delete(&mut self, key: String, item: Option<Item>) -> Result<(), String> {
        match self.history.delete(&key, item.as_ref()) {
            Ok(_) => {
                self.apply_publish(Out::Deleted { key, item });
                Ok(())
            }
            Err(e) => {
                tracing::warn!("history (delete): {e}");
                Err(t!("cannot delete (history)").to_owned())
            }
        }
    }

    async fn set_settings(&mut self, new: Settings) -> Result<(), String> {
        if let Err(e) = new.save() {
            tracing::warn!("settings: {e}");
            return Err(t!("cannot save settings").to_owned());
        }
        let purge = new.retention_days.is_some() && new.retention_days != self.settings.retention_days;
        if new.lang != self.settings.lang {
            i18n::set_lang(new.lang.as_deref());
        }
        self.settings = new;
        self.sync_links();
        self.publish(settings_out(&self.settings, &self.addr).await);
        if purge {
            self.apply_retention();
        }
        Ok(())
    }

    /// Relance la liaison en tâche de fond ; le résultat va au seul demandeur.
    fn reconnect(&self, reply: Option<mpsc::Sender<Out>>) -> Result<(), String> {
        if !self.state.call.is_idle() {
            return Err(t!("not during a call").to_owned());
        }
        if self.reconnecting.swap(true, Ordering::AcqRel) {
            return Err(t!("reconnection already in progress").to_owned());
        }
        let (addr, running) = (self.addr.clone(), Arc::clone(&self.reconnecting));
        tokio::spawn(async move {
            let r = match bluez::Phone::new(&addr).await {
                Ok(p) => p.reconnect(RECONNECT_TIMEOUT).await,
                Err(e) => Err(e.to_string()),
            };
            running.store(false, Ordering::Release);
            let o = match r {
                Ok(()) => Out::Info { msg: t!("iPhone reconnected").to_owned() },
                Err(e) => Out::Error { msg: t!("reconnection failed: {0}", e) },
            };
            if let Some(reply) = reply {
                let _ = reply.send(o).await;
            }
        });
        Ok(())
    }

    /// Enregistre un autre iPhone appairé ; `Ok(true)` : le démon doit repartir pour le suivre.
    async fn set_device(&self, new: &BtAddr) -> Result<bool, String> {
        if self.fixed_addr {
            return Err(t!("address set on the command line").to_owned());
        }
        if !self.state.call.is_idle() {
            return Err(t!("not during a call").to_owned());
        }
        let known = bluez::paired().await.is_ok_and(|d| d.iter().any(|d| d.addr == *new));
        if !known {
            return Err(t!("unknown device (not paired)").to_owned());
        }
        if *new == self.addr {
            return Ok(false);
        }
        config::write_device(new).map_err(|e| t!("cannot save the device: {0}", e))?;
        tracing::info!("device changed: restarting");
        Ok(true)
    }

    fn purge_hidden(&mut self, reply: Option<&mpsc::Sender<Out>>) -> Result<(), String> {
        match self.history.purge_hidden() {
            Ok(n) => {
                tracing::info!(rows = n, "history: deleted items purged");
                if let Some(reply) = reply {
                    let _ = reply.try_send(Out::Info { msg: t!("{0} deleted item(s) purged", n) });
                }
                Ok(())
            }
            Err(e) => {
                tracing::warn!("history (purge): {e}");
                Err(t!("cannot purge the history").to_owned())
            }
        }
    }

    fn clear_history(&mut self) -> Result<(), String> {
        match self.history.clear() {
            Ok(()) => {
                tracing::info!("history cleared");
                self.apply_publish(Out::Cleared);
                Ok(())
            }
            Err(e) => {
                tracing::warn!("history (clear): {e}");
                Err(t!("cannot clear the history").to_owned())
            }
        }
    }

    /// Applique la durée de conservation ; `Some(lignes effacées)`, `None` sans durée ou en cas d'erreur.
    fn purge_old(&mut self) -> Option<usize> {
        let days = self.settings.retention_days?;
        self.history
            .purge_before(&bluez_message::stamp::ago(u64::from(days) * 86_400))
            .inspect_err(|e| tracing::warn!("history (retention): {e}"))
            .ok()
    }

    /// Rétention appliquée ; si des lignes sont parties, l'historique est republié.
    fn apply_retention(&mut self) {
        if self.purge_old().is_some_and(|n| n > 0)
            && let Err(e) = self.reload()
        {
            tracing::warn!("history (reload): {e}");
        }
    }

    /// Republie l'historique après une purge : les clients vident leurs fils puis reçoivent ce qui reste.
    fn reload(&mut self) -> rusqlite::Result<()> {
        let idle = self.state.call.is_idle();
        self.state.apply(Out::Cleared);
        self.hub.publish(Out::Cleared, idle);
        for o in self.history.load(HISTORY)? {
            self.state.apply(o.clone());
            self.hub.publish(o, idle);
        }
        Ok(())
    }
}

/// Réglages publiés aux clients : langues et appareils appairés relus à chaque fois.
async fn settings_out(settings: &Settings, addr: &BtAddr) -> Out {
    let devices = bluez::paired().await.unwrap_or_else(|e| {
        tracing::warn!("paired devices: {e}");
        vec![]
    });
    Out::Settings { settings: settings.clone(), langs: i18n::available(), devices, device: addr.clone() }
}

/// Ce que les réglages laissent passer vers les notifications Navi.
fn for_notify(fx: Effect, s: &Settings) -> Option<Effect> {
    match fx {
        Effect::Sms { .. } if !s.notify_sms => None,
        Effect::Sms { from, .. } if s.hide_preview => Some(Effect::Sms { from, text: t!("New message").to_owned() }),
        Effect::Call(_) if !s.notify_calls => None,
        fx => Some(fx),
    }
}

/// Filtre et consigne : un SMS antérieur à la mise en service de l'historique, ou déjà consigné, n'est pas publié.
fn admit(h: &History, o: &Out) -> bool {
    let r = match o {
        Out::Sms { sms, new: false, .. } if sms.timestamp.as_str() < h.since() => Ok(false),
        Out::Sms { sms, folder, .. } => h.add_sms(sms, folder.is_sent()),
        Out::Sent { to, text, ok: true, timestamp, .. } => h.add_local_sent(to, timestamp, text).map(|_| true),
        _ => Ok(true),
    };
    r.unwrap_or_else(|e| {
        tracing::warn!("history: {e}");
        true
    })
}

/// Répertoire de l'iPhone par PBAP : au démarrage, puis toutes les 6 h (30 min après un échec).
/// Un répertoire vide (refus, transfert sans fichier) ne remplace pas celui déjà consigné.
/// `None` envoyé à chaque échec, pour l'indicateur PBAP.
async fn sync_contacts(addr: BtAddr, tx: mpsc::Sender<Option<Vec<(String, String)>>>) {
    tokio::time::sleep(CONTACTS_DELAY).await;
    loop {
        let (list, wait) = match pbap::pull(&addr).await {
            Ok(list) if list.is_empty() => {
                tracing::warn!("empty PBAP phonebook: stored contacts kept");
                (None, CONTACTS_RETRY)
            }
            Ok(list) => (Some(list), CONTACTS_EVERY),
            Err(e) => {
                tracing::warn!("PBAP phonebook (allow \"Sync Contacts\" on the iPhone?): {e}");
                (None, CONTACTS_RETRY)
            }
        };
        if tx.send(list).await.is_err() {
            return;
        }
        tokio::time::sleep(wait).await;
    }
}

/// Ce que la surveillance de la liaison sait du démon.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Links {
    hfp: bool,
    call: bool,
    auto_connect: bool,
}

/// Compteurs de la surveillance de liaison.
#[derive(Debug, Default)]
struct LinkWatch {
    /// Constats en défaut consécutifs.
    strikes: u32,
    /// Relances depuis le dernier retour à la normale.
    tries: u32,
    /// Dernière relance limitée à `Connect` : la suivante refait la liaison.
    connected_once: bool,
    last: Option<Instant>,
}

impl LinkWatch {
    fn healthy(&mut self) {
        (self.strikes, self.connected_once, self.tries) = (0, false, 0);
    }

    /// Constat en défaut : vrai s'il faut relancer maintenant (seuil atteint, essais restants, pause écoulée).
    fn strike(&mut self) -> bool {
        self.strikes += 1;
        if self.strikes < WATCH_STRIKES || self.tries >= WATCH_TRIES || self.last.is_some_and(|t| t.elapsed() < WATCH_PAUSE) {
            return false;
        }
        self.tries += 1;
        true
    }

    fn retried(&mut self, reset: bool) {
        (self.strikes, self.connected_once, self.last) = (0, !reset, Some(Instant::now()));
    }
}

/// Surveillance de la liaison, à chaque changement signalé par BlueZ et toutes les [`WATCH_EVERY`] :
/// - iPhone non relié et `auto_connect` : `Connect` toutes les [`WATCH_CONNECT_EVERY`] (hors de portée : échec silencieux) ;
/// - iPhone revenu (relié, profils résolus) : la session MAP est réveillée sans attendre son backoff ;
/// - relié sans ses profils (pas de HFP ni de MAP, fréquent après une reconnexion spontanée) : `Connect`, puis
///   liaison refaite (`Disconnect` + `Connect`) si les profils ne reviennent toujours pas. Rien pendant un appel.
async fn link_watch(addr: BtAddr, mut links: watch::Receiver<Links>, nudge: Nudge) {
    let phone = loop {
        match bluez::Phone::new(&addr).await {
            Ok(p) => break p,
            Err(e) => {
                tracing::warn!("Bluetooth watch: {e}");
                tokio::time::sleep(WATCH_INIT_RETRY).await;
            }
        }
    };
    let mut changes = match phone.link_changes().await {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!("Bluetooth watch: {e}; polling only");
            None
        }
    };
    let mut w = LinkWatch::default();
    let mut up = None;
    let mut last_connect: Option<Instant> = None;
    let mut tick = false;
    loop {
        let l = *links.borrow_and_update();
        let link = match phone.link().await {
            Ok(link) => link,
            Err(e) => {
                tracing::debug!("Bluetooth watch: {e}");
                tick = wait_link(&mut changes, &mut links).await;
                continue;
            }
        };
        let now_up = link.connected && link.resolved;
        if now_up && up == Some(false) {
            tracing::info!("iPhone connected: MAP session woken up");
            nudge.nudge();
        }
        up = Some(now_up);
        if !link.connected {
            w.healthy();
            if l.auto_connect && !l.call && last_connect.is_none_or(|t| t.elapsed() >= WATCH_CONNECT_EVERY) {
                last_connect = Some(Instant::now());
                match tokio::time::timeout(WATCH_TIMEOUT, phone.connect()).await {
                    Ok(Ok(())) => tracing::info!("iPhone connected automatically"),
                    Ok(Err(e)) => tracing::debug!("automatic connection: {e}"),
                    Err(_) => tracing::debug!("automatic connection: timed out"),
                }
                // Résultat (profils résolus ou non) relu tout de suite.
                tick = false;
                continue;
            }
        } else if l.call || (link.resolved && l.hfp) {
            w.healthy();
        } else if tick && w.strike() {
            // Seuls les constats périodiques comptent : un signal précède souvent de peu les profils.
            retry_link(&phone, &mut w, link, l).await;
        }
        tick = wait_link(&mut changes, &mut links).await;
    }
}

/// Attend un changement de liaison, de réglage ou d'appel, au plus [`WATCH_EVERY`] ; vrai : délai écoulé.
async fn wait_link(changes: &mut Option<impl futures_util::Stream<Item = ()> + Unpin>, links: &mut watch::Receiver<Links>) -> bool {
    let signal = async {
        match changes.as_mut() {
            Some(c) => c.next().await.is_some(),
            None => std::future::pending().await,
        }
    };
    let ended = tokio::select! {
        () = tokio::time::sleep(WATCH_EVERY) => return true,
        alive = signal => !alive,
        Ok(()) = links.changed() => false,
    };
    if ended {
        // Flux terminé (appareil disparu) : attente périodique seule, sans le relire.
        tracing::debug!("Bluetooth watch: BlueZ signal stream ended");
        *changes = None;
    }
    false
}

/// Relance la liaison incomplète : `Connect`, ou liaison refaite si un `Connect` n'a pas suffi.
async fn retry_link(phone: &bluez::Phone, w: &mut LinkWatch, link: bluez::Link, l: Links) {
    if w.tries == WATCH_TRIES {
        tracing::warn!("iPhone link still incomplete: last retry until the iPhone reconnects");
    }
    // Profils toujours absents après un `Connect` : la liaison est refaite. Avec les profils résolus
    // (HFP seul absent), seulement `Connect` : couper casserait l'audio A2DP en cours.
    let reset = w.connected_once && !link.resolved;
    tracing::info!(resolved = link.resolved, hfp = l.hfp, reset, "iPhone link incomplete: retrying");
    let r = tokio::time::timeout(WATCH_TIMEOUT, async {
        if reset {
            phone.disconnect().await?;
            tokio::time::sleep(WATCH_RESET_GAP).await;
        }
        phone.connect().await
    })
    .await;
    match r {
        Ok(Ok(())) => tracing::info!("iPhone link restarted"),
        Ok(Err(e)) => tracing::warn!("iPhone link restart: {e}"),
        Err(_) => tracing::warn!("iPhone link restart: timed out"),
    }
    w.retried(reset);
}

/// Fenêtre de l'appel en cours (`bluez-msg-call`, installée à côté du démon) : une instance au plus,
/// elle se ferme d'elle-même en fin d'appel ; un fil l'attend (pas de processus zombie).
#[derive(Default)]
struct Popin(Arc<AtomicBool>);

impl Popin {
    fn ensure(&self) {
        if self.0.swap(true, Ordering::AcqRel) {
            return;
        }
        let running = Arc::clone(&self.0);
        // Fil dédié : `systemctl` et l'attente de la fenêtre ne bloquent pas la boucle du démon.
        std::thread::spawn(move || {
            let exe = std::env::current_exe().map(|p| p.with_file_name("bluez-msg-call"));
            let child = exe.and_then(|p| std::process::Command::new(p).envs(display_env()).stdin(std::process::Stdio::null()).spawn());
            match child {
                Ok(mut c) => {
                    let _ = c.wait();
                }
                Err(e) => tracing::warn!("call window: {e}"),
            }
            running.store(false, Ordering::Release);
        });
    }
}

/// Variables d'affichage absentes de l'environnement du démon (lancé avant la session graphique) :
/// reprises de l'environnement du gestionnaire systemd, importé par le compositeur.
fn display_env() -> Vec<(String, String)> {
    const KEYS: [&str; 4] = ["WAYLAND_DISPLAY", "DISPLAY", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE"];
    if std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some() {
        return vec![];
    }
    let out = match std::process::Command::new("systemctl").args(["--user", "show-environment"]).stdin(std::process::Stdio::null()).output() {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            tracing::warn!("systemctl show-environment: {}", o.status);
            return vec![];
        }
        Err(e) => {
            tracing::warn!("systemctl show-environment: {e}");
            return vec![];
        }
    };
    String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| KEYS.contains(k))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Micro coupé par la bulle : nœud visé (la source par défaut peut changer en cours d'appel) et état d'avant.
struct MicHold {
    node: String,
    was_muted: bool,
}

impl MicHold {
    async fn restore(self) {
        if let Err(e) = wpctl(&["set-mute", &self.node, if self.was_muted { "1" } else { "0" }]).await {
            tracing::warn!("microphone restore: {e}");
        }
    }
}

/// Muet de l'appel : coupe la source PipeWire par défaut (celle qui alimente l'appel), pas l'iPhone.
async fn toggle_mute(state: &mut State, mic: &mut Option<MicHold>) -> Result<Vec<Effect>, String> {
    if state.call.is_idle() {
        return Err(t!("microphone: no call in progress").to_owned());
    }
    let on = !state.call.muted;
    if on {
        if mic.is_none() {
            let id = wpctl(&["inspect", "@DEFAULT_AUDIO_SOURCE@"]).await?;
            let node = id
                .strip_prefix("id ")
                .and_then(|r| r.split(',').next())
                .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .ok_or(t!("microphone: default source not found"))?
                .to_owned();
            let was_muted = wpctl(&["get-volume", &node]).await?.contains("[MUTED]");
            *mic = Some(MicHold { node, was_muted });
        }
        let node = mic.as_ref().map(|h| h.node.clone()).unwrap_or_default();
        wpctl(&["set-mute", &node, "1"]).await?;
    } else if let Some(h) = mic.take() {
        wpctl(&["set-mute", &h.node, "0"]).await?;
    }
    Ok(state.set_muted(on))
}

async fn wpctl(args: &[&str]) -> Result<String, String> {
    let run = tokio::process::Command::new("wpctl").args(args).kill_on_drop(true).output();
    let out = tokio::time::timeout(WPCTL_TIMEOUT, run)
        .await
        .map_err(|_| "wpctl: timed out".to_owned())?
        .map_err(|e| format!("wpctl: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("wpctl {}: {}", args[0], out.status))
    }
}
