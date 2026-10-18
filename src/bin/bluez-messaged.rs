//! `bluez-messaged`: the iPhone SMS/calls daemon. Holds the MAP + calls engine, notifies on behalf of Navi
//! (even with the window closed) and serves the state to clients (GUI) on `$XDG_RUNTIME_DIR/bluez-message.sock`.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use bluez_message::addr::BtAddr;
use bluez_message::app::notify;
use bluez_message::app::state::{Effect, Phase, State, phone_reads};
use bluez_message::config;
use bluez_message::engine::{Cmd, Engine, Item, Nudge, Out, Profile};
use bluez_message::history::History;
use bluez_message::map::Sms;
use bluez_message::settings::Settings;
use bluez_message::setup::Setup;
use bluez_message::{bluez, i18n, ipc, numbering, pbap, t};
use clap::Parser;
use futures_util::StreamExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};

/// SMS and calls replayed at startup (the most recent ones).
const HISTORY: usize = 1000;
/// How often the retention period is applied.
const RETENTION_EVERY: Duration = Duration::from_hours(24);
/// Time during which a new outgoing call is refused after a dial.
const DIAL_GUARD: Duration = Duration::from_secs(5);
/// Maximum time of a reconnection requested from Settings.
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// First contacts read, after the profiles start.
const CONTACTS_DELAY: Duration = Duration::from_secs(10);
const CONTACTS_EVERY: Duration = Duration::from_hours(6);
/// Spaced out: each attempt may ask for authorisation again on the iPhone.
const CONTACTS_RETRY: Duration = Duration::from_mins(30);
const WATCH_EVERY: Duration = Duration::from_secs(20);
/// Retry when the BlueZ device is not found at startup.
const WATCH_INIT_RETRY: Duration = Duration::from_secs(300);
/// Faulty checks before a restart.
const WATCH_STRIKES: u32 = 2;
const WATCH_PAUSE: Duration = Duration::from_secs(120);
const WATCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Pause between `Disconnect` and `Connect` of a rebuilt link.
const WATCH_RESET_GAP: Duration = Duration::from_secs(2);
/// Interval of `Connect` attempts when the iPhone is not linked (`auto_connect` setting).
const WATCH_CONNECT_EVERY: Duration = Duration::from_secs(60);
/// Restarts without return to normal before giving up (HFP deliberately absent, iPhone taken elsewhere…).
const WATCH_TRIES: u32 = 3;
const WPCTL_TIMEOUT: Duration = Duration::from_secs(3);
/// Wait for the notification thread at shutdown.
const NOTIFY_FLUSH: Duration = Duration::from_secs(1);

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
        // New iPhone: the same binary restarts (same PID under systemd) and rereads `device`.
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

/// Outcome of [`run`].
enum Exit {
    Done,
    /// Device changed: restart the daemon.
    Restart,
}

async fn run(args: Args) -> Result<Exit, String> {
    let fixed_addr = args.addr.is_some();
    // No device: setup mode (socket, settings and pairing only) until the wizard registers one.
    let addr = config::resolve_addr(args.addr).map_err(|e| e.to_string())?;
    let settings = Settings::load();
    let path = config::socket_path()?;
    let bound = ipc::bind(&path).await.map_err(|e| e.to_string())?;
    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let (dev, mut events) = if let Some(addr) = addr {
        let history = open_history(&addr)?;
        let (engine, events) = Engine::start(&addr, Duration::from_secs(args.interval));
        (Some(Linked { addr, history, engine }), Some(events))
    } else {
        tracing::info!("no device configured: setup mode");
        (None, None)
    };

    let (req_tx, mut requests) = mpsc::channel::<ipc::Request>(64);
    let hub = ipc::spawn_server(bound, req_tx.clone());
    let bus = zbus::Connection::system().await.map_err(|e| e.to_string())?;
    let (setup_tx, mut setup_events) = mpsc::unbounded_channel();
    let setup = Setup::start(bus, setup_tx, hub.clients()).await.map_err(|e| e.to_string())?;
    let (fx_tx, fx_rx) = std_mpsc::channel();
    let notifier = notify::spawn(fx_rx, move |cmd| {
        // Notification action (answer, reply…): lost if the queue is full or the daemon is stopping.
        if let Err(e) = req_tx.try_send((cmd, None)) {
            tracing::warn!("notification action dropped: {e}");
        }
    });
    tracing::info!(socket = %path.display(), "daemon ready");

    let (links_tx, links_rx) = watch::channel(Links::default());
    let mut d = Daemon {
        dev,
        fixed_addr,
        settings,
        state: State::default(),
        hub,
        setup,
        fx_tx,
        notifier,
        links_tx,
        mic: None,
        dialing: None,
        popin: Popin::default(),
        reconnecting: Arc::default(),
        recap: 0,
    };
    d.sync_links();
    d.start().await?;
    let mut daily = tokio::time::interval_at(tokio::time::Instant::now() + RETENTION_EVERY, RETENTION_EVERY);
    // Without a device the senders are dropped: the contacts branch stays disabled.
    let (ct_tx, mut contacts) = mpsc::channel(1);
    if let Some(dev) = &d.dev {
        tokio::spawn(sync_contacts(dev.addr.clone(), ct_tx));
        tokio::spawn(link_watch(dev.addr.clone(), links_rx, dev.engine.nudger()));
    }
    let r = loop {
        tokio::select! {
            o = next(events.as_mut()) => {
                let Some(o) = o else { break Err(t!("engine stopped").to_owned()) };
                d.on_event(o).await;
            }
            Some(o) = setup_events.recv() => d.on_setup(o).await,
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

/// Next engine event; never resolves without a device.
async fn next(events: Option<&mut mpsc::UnboundedReceiver<Out>>) -> Option<Out> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The device part of a command, or the error sent back to the requester.
fn need<T>(dev: Option<T>) -> Result<T, String> {
    dev.ok_or_else(|| t!("no device configured").to_owned())
}

/// SQLite history; unreadable database: the daemon runs anyway (in-memory history) rather than looping under systemd.
fn open_history(addr: &BtAddr) -> Result<History, String> {
    let db = config::data_dir().ok_or(t!("HOME not set"))?.join("bluez-message/history.db");
    History::open(&db, addr)
        .or_else(|e| {
            tracing::error!("history {}: {e}; in-memory history for this session", db.display());
            History::memory(addr)
        })
        .map_err(|e| e.to_string())
}

/// What exists only once a device is configured.
struct Linked {
    addr: BtAddr,
    history: History,
    engine: Engine,
}

/// Daemon state: device (engine, history), setup, call state and clients; one method per event or command.
struct Daemon {
    /// `None`: setup mode, the device commands are refused.
    dev: Option<Linked>,
    /// Address given as an argument: changing device is refused.
    fixed_addr: bool,
    settings: Settings,
    state: State,
    hub: ipc::Hub,
    /// Adapter, search and pairing (both modes).
    setup: Setup,
    /// Effects towards Navi notifications.
    fx_tx: std_mpsc::Sender<Effect>,
    /// Notification thread, joined at shutdown so that `CloseAll` is applied before an `exec`.
    notifier: std::thread::JoinHandle<()>,
    /// State seen by the link watch.
    links_tx: watch::Sender<Links>,
    /// PC microphone muted by the bubble: restored at the end of the call.
    mic: Option<MicHold>,
    /// Last dial, for [`DIAL_GUARD`].
    dialing: Option<Instant>,
    popin: Popin,
    /// Reconnection requested from Settings in progress (only one at a time).
    reconnecting: Arc<AtomicBool>,
    /// Received SMS, unread on the iPhone, inserted by the current listing: summed up in one notification.
    recap: usize,
}

impl Daemon {
    /// Publishes to the clients, the current call state deciding what the snapshot keeps.
    fn publish(&self, o: Out) {
        self.hub.publish(o, self.state.call.is_idle());
    }

    /// Applies to the daemon state, then publishes.
    fn apply_publish(&mut self, o: Out) {
        self.state.apply(o.clone());
        self.publish(o);
    }

    fn addr(&self) -> Option<&BtAddr> {
        self.dev.as_ref().map(|d| &d.addr)
    }

    /// Settings, retention, then the local history: the snapshot replays it to each client.
    async fn start(&mut self) -> Result<(), String> {
        self.apply_publish(settings_out(&self.settings, self.addr()).await);
        if self.dev.is_none() {
            return Ok(());
        }
        // Failure logged: the history keeps its former keys.
        let _ = self.rekey(self.state.home());
        if let Some(n) = self.purge_old() {
            tracing::info!(rows = n, "history: retention applied");
        }
        for o in need(self.dev.as_ref())?.history.load(HISTORY).map_err(|e| t!("history: {0}", e))? {
            self.state.apply(o.clone());
            self.hub.publish(o, true);
        }
        Ok(())
    }

    async fn shutdown(self, socket: &Path) {
        // Pairing cancelled and agent unregistered before the engine stops.
        self.setup.shutdown().await;
        if let Some(dev) = self.dev {
            dev.engine.shutdown().await;
        }
        // Socket removed before the lock is released (held by `hub`).
        let _ = std::fs::remove_file(socket);
        drop(self.hub);
        if let Some(h) = self.mic {
            h.restore().await;
        }
        // Pending effects (`CloseAll`) applied; bounded: a stuck notification server does not block the stop.
        drop(self.fx_tx);
        let notifier = tokio::task::spawn_blocking(move || self.notifier.join());
        if tokio::time::timeout(NOTIFY_FLUSH, notifier).await.is_err() {
            tracing::warn!("notifications: not flushed in time");
        }
    }

    async fn on_event(&mut self, o: Out) {
        let o = match o {
            Out::Flags { items } => return self.on_flags(&items),
            o => o,
        };
        let Some(dev) = self.dev.as_ref() else { return };
        if !admit(&dev.history, &o) {
            return;
        }
        if let Out::Sms { sms, folder, new: false } = &o
            && !folder.is_sent()
            && !sms.read
        {
            self.recap += 1;
        }
        let mut logs = vec![];
        for fx in self.state.apply(o.clone()) {
            if let Effect::CallEnd(e) = &fx
                && !e.number.is_empty()
            {
                if let Err(err) = dev.history.add_call(&e.number, &e.timestamp, e.kind, e.secs) {
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

    /// iPhone read marks (never backwards), then the summary of what the session's first listing brought.
    fn on_flags(&mut self, items: &[Sms]) {
        for (key, until) in phone_reads(items, self.state.home()) {
            if self.state.read_until(&key).is_none_or(|r| until.as_str() > r)
                && let Err(e) = self.mark_read(key, until)
            {
                tracing::debug!("read on the phone: {e}");
            }
        }
        let n = std::mem::take(&mut self.recap);
        if n > 0 && self.settings.notify_sms {
            let _ = self.fx_tx.send(Effect::Sms { from: "iPhone".to_owned(), text: t!("{0} new messages", n) });
        }
    }

    /// Setup events (adapter, search, pairing); a new pairing refreshes the paired devices.
    async fn on_setup(&mut self, o: Out) {
        let paired = matches!(o, Out::Paired { ok: true, .. });
        self.publish(o);
        if paired {
            self.apply_publish(settings_out(&self.settings, self.addr()).await);
        }
    }

    /// Passes to the link watch what it needs to know (call, HFP, setting).
    fn sync_links(&self) {
        let now = Links { hfp: self.state.hfp_up, call: !self.state.call.is_idle(), auto_connect: self.settings.auto_connect };
        self.links_tx.send_if_modified(|l| std::mem::replace(l, now) != now);
    }

    /// `None`: synchronisation failed (refusal, empty contacts), recorded contacts kept.
    fn on_contacts(&mut self, list: Option<&[(String, String)]>) {
        let Some(dev) = self.dev.as_mut() else { return };
        let ok = match list.map(|l| dev.history.replace_contacts(l)) {
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

    /// Command from a client (or a notification); `Some`: the daemon must stop or restart.
    async fn on_request(&mut self, cmd: Cmd, reply: Option<mpsc::Sender<Out>>) -> Option<Exit> {
        let res = match cmd {
            Cmd::Mute => self.mute().await,
            Cmd::MarkRead { key, until } => self.mark_read(key, until),
            cmd @ Cmd::Dial { .. } => self.dial(cmd),
            Cmd::Delete { key, item } => self.delete(key, item),
            Cmd::Settings { settings } => self.set_settings(settings).await,
            Cmd::Reconnect => self.reconnect(reply.clone()),
            Cmd::SetDevice { addr } => match self.set_device(&addr).await {
                Ok(true) => {
                    // No trace of the former device left on the desktop.
                    let _ = self.fx_tx.send(Effect::CloseAll);
                    return Some(Exit::Restart);
                }
                Ok(false) => Ok(()),
                Err(e) => Err(e),
            },
            Cmd::PurgeHidden => self.purge_hidden(reply.as_ref()),
            Cmd::ClearHistory => self.clear_history(),
            cmd if cmd.is_setup() => self.setup.submit(cmd, reply.clone()),
            cmd => need(self.dev.as_ref()).and_then(|d| d.engine.submit(cmd)),
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

    /// Only a known conversation, up to its last item at most: `reads` stays bounded.
    fn mark_read(&mut self, key: String, until: String) -> Result<(), String> {
        let dev = need(self.dev.as_ref())?;
        if self.state.conv(&key).is_none_or(|c| until.get(..15) > c.last_stamp().get(..15)) {
            return Err(t!("mark as read: unknown conversation or future date").to_owned());
        }
        if let Err(e) = dev.history.mark_read(&key, &until) {
            tracing::warn!("history (read): {e}");
        }
        self.apply_publish(Out::Read { key, until });
        Ok(())
    }

    /// `phase` stays `Idle` until the first HFP event: a recent dial also blocks.
    fn dial(&mut self, cmd: Cmd) -> Result<(), String> {
        if !self.state.hfp_up || !self.state.call.is_idle() || self.dialing.is_some_and(|t| t.elapsed() < DIAL_GUARD) {
            return Err(t!("cannot call: phone not linked or a call is already in progress").to_owned());
        }
        need(self.dev.as_ref())?.engine.submit(cmd).inspect(|()| self.dialing = Some(Instant::now()))
    }

    /// Local history only: nothing is erased on the iPhone.
    fn delete(&mut self, key: String, item: Option<Item>) -> Result<(), String> {
        match need(self.dev.as_ref())?.history.delete(&key, item.as_ref()) {
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
        // Other country: conversations regrouped before anything changes; failure: settings refused.
        let old_home = self.state.home();
        let regrouped = self.rekey(numbering::home(new.country.as_deref(), new.lang.as_deref()))?;
        if let Err(e) = new.save() {
            tracing::warn!("settings: {e}");
            if regrouped {
                let _ = self.rekey(old_home);
            }
            return Err(t!("cannot save settings").to_owned());
        }
        let purge = new.retention_days.is_some() && new.retention_days != self.settings.retention_days;
        if new.lang != self.settings.lang {
            i18n::set_lang(new.lang.as_deref());
        }
        self.settings = new;
        self.sync_links();
        self.apply_publish(settings_out(&self.settings, self.addr()).await);
        if regrouped && let Err(e) = self.reload() {
            tracing::warn!("history (reload): {e}");
        }
        if purge {
            self.apply_retention();
        }
        Ok(())
    }

    /// Grouping keys of the history aligned on the country `home` numbers are read with; `true` if they changed.
    fn rekey(&mut self, home: Option<numbering::Id>) -> Result<bool, String> {
        let Some(dev) = self.dev.as_mut() else { return Ok(false) };
        match dev.history.rekey(home) {
            Ok(changed) => {
                if changed {
                    tracing::info!(country = home.map(numbering::code).unwrap_or_default(), "history: numbers regrouped");
                }
                Ok(changed)
            }
            Err(e) => {
                tracing::error!("history (keys): {e}");
                Err(t!("history: {0}", e))
            }
        }
    }

    /// Restarts the link in the background; the result goes to the requester only.
    fn reconnect(&self, reply: Option<mpsc::Sender<Out>>) -> Result<(), String> {
        let addr = need(self.addr())?.clone();
        if !self.state.call.is_idle() {
            return Err(t!("not during a call").to_owned());
        }
        if self.reconnecting.swap(true, Ordering::AcqRel) {
            return Err(t!("reconnection already in progress").to_owned());
        }
        let running = Arc::clone(&self.reconnecting);
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

    /// Registers another paired iPhone; `Ok(true)`: the daemon must restart to follow it.
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
        if self.addr() == Some(new) {
            return Ok(false);
        }
        config::write_device(new).map_err(|e| t!("cannot save the device: {0}", e))?;
        tracing::info!("device changed: restarting");
        Ok(true)
    }

    fn purge_hidden(&mut self, reply: Option<&mpsc::Sender<Out>>) -> Result<(), String> {
        match need(self.dev.as_mut())?.history.purge_hidden() {
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
        match need(self.dev.as_mut())?.history.clear() {
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

    /// Applies the retention period; `Some(rows erased)`, `None` without a period or on error.
    fn purge_old(&mut self) -> Option<usize> {
        let days = self.settings.retention_days?;
        self.dev
            .as_mut()?
            .history
            .purge_before(&bluez_message::stamp::ago(u64::from(days) * 86_400))
            .inspect_err(|e| tracing::warn!("history (retention): {e}"))
            .ok()
    }

    /// Retention applied; if rows are gone, the history is republished.
    fn apply_retention(&mut self) {
        if self.purge_old().is_some_and(|n| n > 0)
            && let Err(e) = self.reload()
        {
            tracing::warn!("history (reload): {e}");
        }
    }

    /// Republishes the history after a purge: clients clear their threads then receive what remains.
    fn reload(&mut self) -> rusqlite::Result<()> {
        let Some(dev) = self.dev.as_ref() else { return Ok(()) };
        let idle = self.state.call.is_idle();
        self.state.apply(Out::Cleared);
        self.hub.publish(Out::Cleared, idle);
        for o in dev.history.load(HISTORY)? {
            self.state.apply(o.clone());
            self.hub.publish(o, idle);
        }
        Ok(())
    }
}

/// Settings published to the clients: languages and paired devices reread each time.
async fn settings_out(settings: &Settings, addr: Option<&BtAddr>) -> Out {
    let devices = bluez::paired().await.unwrap_or_else(|e| {
        tracing::warn!("paired devices: {e}");
        vec![]
    });
    let lang = settings.lang.as_deref();
    let home = numbering::home(settings.country.as_deref(), lang).map(numbering::code);
    let auto = numbering::home(None, lang).map(numbering::code);
    Out::Settings { settings: settings.clone(), langs: i18n::available(), devices, device: addr.cloned(), home, auto }
}

/// What the settings let through to Navi notifications.
fn for_notify(fx: Effect, s: &Settings) -> Option<Effect> {
    match fx {
        Effect::Sms { .. } if !s.notify_sms => None,
        Effect::Sms { from, .. } if s.hide_preview => Some(Effect::Sms { from, text: t!("New message").to_owned() }),
        Effect::Call(_) if !s.notify_calls => None,
        fx => Some(fx),
    }
}

/// Filters and records: an SMS older than the history start, or already recorded, is not published.
fn admit(h: &History, o: &Out) -> bool {
    let r = match o {
        Out::Flags { .. } => Ok(false),
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

/// iPhone contacts through PBAP: at startup, then every 6 h (30 min after a failure).
/// An empty contact list (refusal, transfer without a file) does not replace the one already recorded.
/// `None` sent on every failure, for the PBAP indicator.
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

/// What the link watch knows about the daemon.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Links {
    hfp: bool,
    call: bool,
    auto_connect: bool,
}

/// Link watch counters.
#[derive(Debug, Default)]
struct LinkWatch {
    /// Consecutive faulty checks.
    strikes: u32,
    /// Restarts since the last return to normal.
    tries: u32,
    /// Last restart limited to `Connect`: the next one rebuilds the link.
    connected_once: bool,
    last: Option<Instant>,
}

impl LinkWatch {
    fn healthy(&mut self) {
        (self.strikes, self.connected_once, self.tries) = (0, false, 0);
    }

    /// Faulty check: true if a restart is needed now (threshold reached, attempts left, pause elapsed).
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

/// Link watch, on each change signalled by BlueZ and every [`WATCH_EVERY`]:
/// - iPhone not linked and `auto_connect`: `Connect` every [`WATCH_CONNECT_EVERY`] (out of range: silent failure);
/// - iPhone back (linked, profiles resolved): the MAP session is woken without waiting for its backoff;
/// - linked without its profiles (no HFP or MAP, common after a spontaneous reconnection): `Connect`, then
///   rebuilt link (`Disconnect` + `Connect`) if the profiles still do not come back. Nothing during a call.
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
                // Result (profiles resolved or not) reread right away.
                tick = false;
                continue;
            }
        } else if l.call || (link.resolved && l.hfp) {
            w.healthy();
        } else if tick && w.strike() {
            // Only periodic checks count: a signal often precedes the profiles by a little.
            retry_link(&phone, &mut w, link, l).await;
        }
        tick = wait_link(&mut changes, &mut links).await;
    }
}

/// Waits for a link, setting or call change, at most [`WATCH_EVERY`]; true: delay elapsed.
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
        // Stream ended (device gone): periodic wait only, without rereading it.
        tracing::debug!("Bluetooth watch: BlueZ signal stream ended");
        *changes = None;
    }
    false
}

/// Restarts the incomplete link: `Connect`, or rebuilt link if a `Connect` was not enough.
async fn retry_link(phone: &bluez::Phone, w: &mut LinkWatch, link: bluez::Link, l: Links) {
    if w.tries == WATCH_TRIES {
        tracing::warn!("iPhone link still incomplete: last retry until the iPhone reconnects");
    }
    // Profiles still absent after a `Connect`: the link is rebuilt. With profiles resolved
    // (HFP alone missing), only `Connect`: cutting would break the ongoing A2DP audio.
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

/// Window of the current call (`bluez-msg-call`, installed next to the daemon): one instance at most,
/// it closes itself at the end of the call; a thread waits for it (no zombie process).
#[derive(Default)]
struct Popin(Arc<AtomicBool>);

impl Popin {
    fn ensure(&self) {
        if self.0.swap(true, Ordering::AcqRel) {
            return;
        }
        let running = Arc::clone(&self.0);
        // Dedicated thread: `systemctl` and the wait for the window do not block the daemon loop.
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

/// Display variables missing from the daemon environment (started before the graphical session):
/// taken from the systemd manager environment, imported by the compositor.
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

/// Microphone muted by the bubble: target node (the default source may change during a call) and previous state.
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

/// Call mute: mutes the default PipeWire source (the one feeding the call), not the iPhone.
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
