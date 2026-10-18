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
use bluez_message::app::state::{Decline, Effect, Phase, State, phone_reads, sms_peer};
use bluez_message::config;
use bluez_message::engine::{Cmd, ConvFlags, Engine, Item, Nudge, Out, Profile};
use bluez_message::history::History;
use bluez_message::reads::{Job, Reads};
use bluez_message::map::Sms;
use bluez_message::settings::Settings;
use bluez_message::setup::Setup;
use bluez_message::{bluez, i18n, ipc, numbering, pbap, t};
use clap::Parser;
use futures_util::StreamExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Notify, mpsc, watch};

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
/// Full read: catches the edits of an existing contact, which the size probe does not see.
const CONTACTS_EVERY: Duration = Duration::from_hours(1);
/// Size probe of the phonebook between two full reads (additions and removals).
const CONTACTS_PROBE: Duration = Duration::from_mins(5);
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
/// Delay before the first restart of an incomplete link, outside the strikes and the pause: an iPhone that
/// reconnects by itself often leaves its profiles unresolved until a `Connect` (measured: 2 s after it).
const WATCH_GRACE: Duration = Duration::from_secs(8);
/// Battery level reread without signal (`Battery1` appears without `PropertiesChanged`).
const BATTERY_EVERY: Duration = Duration::from_mins(1);
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
        let reads = Reads::spawn(history.reader().map_err(|e| e.to_string())?);
        let (engine, events) = Engine::start(&addr, Duration::from_secs(args.interval));
        (Some(Linked { addr, history, reads, engine }), Some(events))
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
    let (bat_tx, mut battery) = mpsc::channel(1);
    if let Some(dev) = &d.dev {
        tokio::spawn(battery_watch(dev.addr.clone(), bat_tx));
        let reconnected = Arc::new(Notify::new());
        tokio::spawn(sync_contacts(dev.addr.clone(), ct_tx, Arc::clone(&reconnected)));
        tokio::spawn(link_watch(dev.addr.clone(), links_rx, dev.engine.nudger(), reconnected));
    }
    let r = loop {
        tokio::select! {
            o = next(events.as_mut()) => {
                let Some(o) = o else { break Err(t!("engine stopped").to_owned()) };
                d.on_event(o).await;
            }
            Some(o) = setup_events.recv() => d.on_setup(o).await,
            Some(list) = contacts.recv() => d.on_contacts(list.as_deref()),
            Some(percent) = battery.recv() => d.apply_publish(Out::Battery { percent }),
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
    /// Slow reads of `history`, away from the daemon loop.
    reads: Reads,
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
        // Received SMS just recorded: its conversation leaves the archive; a muted one stays out of the summary.
        let mut unarchive = None;
        if let Some((key, flags, recap, fresh)) = received(&self.state, &o) {
            self.recap += usize::from(recap);
            if flags.archived && fresh {
                unarchive = Some((key, ConvFlags { archived: false, ..flags }));
            }
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
            if let Some(fx) = for_notify(fx, &self.settings, self.state.home()) {
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
        if let Some((key, flags)) = unarchive {
            let _ = self.set_conv(key, flags);
        }
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
            let _ = self.fx_tx.send(Effect::Sms { from: "iPhone".to_owned(), text: t!("{0} new messages", n), reply: None });
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
            Cmd::ConvFlags { key, flags } => self.set_conv(key, flags),
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
            Cmd::Search { query, before } => self.read(reply.as_ref(), Job::Search { query, before }),
            Cmd::Older { key, before, limit } => self.read(reply.as_ref(), Job::Older { key, before, limit: usize::try_from(limit).unwrap_or(usize::MAX) }),
            Cmd::ClearHistory => self.clear_history(),
            Cmd::Export { key } => self.export(key, reply.as_ref()),
            Cmd::Open { number } => open_conversation(&number, &self.hub),
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

    /// Only a known conversation can be flagged: `convs` stays bounded.
    fn set_conv(&mut self, key: String, flags: ConvFlags) -> Result<(), String> {
        let dev = need(self.dev.as_ref())?;
        if flags != ConvFlags::default() && self.state.conv(&key).is_none() {
            return Err(t!("invalid conversation").to_owned());
        }
        if let Err(e) = dev.history.set_conv(&key, flags) {
            tracing::warn!("history (conversation): {e}");
            return Err(t!("cannot save the conversation state (history)").to_owned());
        }
        self.apply_publish(Out::Conv { key, flags });
        Ok(())
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

    /// Queues a slow read for the read task, which answers `reply`; from a notification (no reply), nothing to do.
    fn read(&self, reply: Option<&mpsc::Sender<Out>>, job: Job) -> Result<(), String> {
        let dev = need(self.dev.as_ref())?;
        let Some(reply) = reply else { return Ok(()) };
        dev.reads.submit(job, reply.clone()).map_err(|e| e.msg())
    }

    /// Writes the history to the download folder (home folder without one): never a path from the IPC.
    fn export(&self, key: Option<String>, reply: Option<&mpsc::Sender<Out>>) -> Result<(), String> {
        need(self.dev.as_ref())?;
        if key.as_deref().is_some_and(|k| self.state.conv(k).is_none()) {
            return Err(t!("invalid conversation").to_owned());
        }
        let dir = dirs::download_dir().filter(|d| d.is_dir()).or_else(dirs::home_dir).ok_or_else(|| t!("HOME not set").to_owned())?;
        self.read(reply, Job::Export { key, dir })
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

/// What the settings let through to Navi notifications; a ringing call gets its SMS replies ([`Decline`]).
fn for_notify(fx: Effect, s: &Settings, home: Option<numbering::Id>) -> Option<Effect> {
    match fx {
        Effect::Sms { .. } if !s.notify_sms => None,
        Effect::Sms { from, reply, .. } if s.hide_preview => Some(Effect::Sms { from, text: t!("New message").to_owned(), reply }),
        Effect::Call(_) if !s.notify_calls => None,
        Effect::Call(c) if c.phase == Phase::Incoming => {
            let decline = c.sms_number(home).map(|number| Decline { number, text: s.quick_replies.first().cloned() });
            Some(Effect::Ring(c, decline))
        }
        fx => Some(fx),
    }
}

/// Received SMS: its conversation key, its flags, whether it counts in the session summary
/// (listed, not new, unread, not muted), and whether it lifts the archive (new or unread).
fn received(state: &State, o: &Out) -> Option<(String, ConvFlags, bool, bool)> {
    let Out::Sms { sms, folder, new } = o else { return None };
    if folder.is_sent() {
        return None;
    }
    let key = state.key(sms_peer(sms, false).0);
    let flags = state.flags(&key);
    Some((key, flags, !new && !sms.read && !flags.muted, *new || !sms.read))
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

/// iPhone contacts through PBAP: at startup and on each reconnection of the iPhone (`reconnected`),
/// when the phonebook size changes (probe every 5 min), otherwise every hour (30 min after a failure).
/// PBAP has no change notification and the iPhone leaves its version counters at zero: only the
/// size betrays a change, and the whole phonebook is read again.
/// An empty contact list (refusal, transfer without a file) does not replace the one already recorded.
/// `None` sent on every failure, for the PBAP indicator.
async fn sync_contacts(addr: BtAddr, tx: mpsc::Sender<Option<Vec<(String, String)>>>, reconnected: Arc<Notify>) {
    tokio::time::sleep(CONTACTS_DELAY).await;
    loop {
        // No probe after a failure: each session may ask for authorisation again on the iPhone.
        let (list, wait, known) = match pbap::pull(&addr).await {
            Ok(book) if book.contacts.is_empty() => {
                tracing::warn!("empty PBAP phonebook: stored contacts kept");
                (None, CONTACTS_RETRY, None)
            }
            Ok(book) => (Some(book.contacts), CONTACTS_EVERY, book.size),
            Err(e) => {
                tracing::warn!("PBAP phonebook (allow \"Sync Contacts\" on the iPhone?): {e}");
                (None, CONTACTS_RETRY, None)
            }
        };
        if tx.send(list).await.is_err() {
            return;
        }
        let due = tokio::time::Instant::now() + wait;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(due) => break,
                () = reconnected.notified() => {
                    tracing::info!("iPhone reconnected: contacts read again");
                    tokio::time::sleep(CONTACTS_DELAY).await;
                    break;
                }
                () = tokio::time::sleep(CONTACTS_PROBE), if known.is_some() => match pbap::size(&addr).await {
                    Ok(n) if Some(n) != known => {
                        tracing::info!("phonebook size changed: contacts read again");
                        break;
                    }
                    Ok(_) => {}
                    // iPhone away: the next probe or the reconnection takes over.
                    Err(e) => tracing::debug!("PBAP size probe: {e}"),
                },
            }
        }
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
    /// Start of the current incomplete state.
    bad_since: Option<Instant>,
    /// Early restart ([`WATCH_GRACE`]) already made for this incomplete state.
    early: bool,
}

impl LinkWatch {
    fn healthy(&mut self) {
        (self.strikes, self.connected_once, self.tries) = (0, false, 0);
        (self.bad_since, self.early) = (None, false);
    }

    /// Incomplete link: true if the early restart is due now (once per incomplete state).
    fn early_due(&mut self) -> bool {
        let since = *self.bad_since.get_or_insert_with(Instant::now);
        if self.early || since.elapsed() < WATCH_GRACE {
            return false;
        }
        self.early = true;
        true
    }

    /// Wait before the next check: [`WATCH_EVERY`], shortened to the early restart deadline.
    fn next_check(&self) -> Duration {
        match self.bad_since {
            Some(t) if !self.early => WATCH_GRACE.saturating_sub(t.elapsed()).min(WATCH_EVERY),
            _ => WATCH_EVERY,
        }
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

/// Device proxy, retried every [`WATCH_INIT_RETRY`] while the system bus refuses it.
async fn phone(addr: &BtAddr) -> bluez::Phone {
    loop {
        match bluez::Phone::new(addr).await {
            Ok(p) => return p,
            Err(e) => {
                tracing::warn!("Bluetooth watch: {e}");
                tokio::time::sleep(WATCH_INIT_RETRY).await;
            }
        }
    }
}

/// Follows the phone's battery level and sends each change (`None`: unknown).
async fn battery_watch(addr: BtAddr, tx: mpsc::Sender<Option<u8>>) {
    let phone = phone(&addr).await;
    let mut changes = match phone.battery_changes().await {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!("battery watch: {e}; polling only");
            None
        }
    };
    let mut last = None;
    loop {
        let level = phone.battery().await;
        if last != Some(level) {
            last = Some(level);
            tracing::debug!(level, "battery level");
            if tx.send(level).await.is_err() {
                return;
            }
        }
        match changes.as_mut() {
            Some(c) => {
                // Stream ended (connection lost): polling only.
                if let Ok(None) = tokio::time::timeout(BATTERY_EVERY, futures_util::StreamExt::next(c)).await {
                    changes = None;
                }
            }
            None => tokio::time::sleep(BATTERY_EVERY).await,
        }
    }
}

/// Link watch, on each change signalled by BlueZ and every [`WATCH_EVERY`]:
/// - iPhone not linked and `auto_connect`: `Connect` every [`WATCH_CONNECT_EVERY`] (out of range: silent failure);
/// - iPhone back (linked, profiles resolved): the MAP session is woken without waiting for its backoff,
///   and the contacts are read again (`reconnected`);
/// - linked without its profiles (no HFP or MAP, common after a spontaneous reconnection): `Connect`, then
///   first after [`WATCH_GRACE`], then on strikes, rebuilt link (`Disconnect` + `Connect`) if the profiles
///   still do not come back. Nothing during a call.
async fn link_watch(addr: BtAddr, mut links: watch::Receiver<Links>, nudge: Nudge, reconnected: Arc<Notify>) {
    let phone = phone(&addr).await;
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
    // Measurement of the profiles' return: (connected, resolved, HFP) logged on change, timed from `Connected`.
    let mut seen = None;
    let mut since: Option<Instant> = None;
    loop {
        let l = *links.borrow_and_update();
        let link = match phone.link().await {
            Ok(link) => link,
            Err(e) => {
                tracing::debug!("Bluetooth watch: {e}");
                tick = wait_link(&mut changes, &mut links, WATCH_EVERY).await;
                continue;
            }
        };
        if seen != Some((link, l.hfp)) {
            seen = Some((link, l.hfp));
            since = if link.connected { since.or_else(|| Some(Instant::now())) } else { None };
            let ms = since.map(|t| t.elapsed().as_millis());
            tracing::debug!(connected = link.connected, resolved = link.resolved, hfp = l.hfp, ms, "iPhone link state");
        }
        let now_up = link.connected && link.resolved;
        if now_up && up == Some(false) {
            tracing::info!("iPhone connected: MAP session woken up");
            nudge.nudge();
            reconnected.notify_one();
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
        } else if w.early_due() {
            retry_link(&phone, &mut w, link, l).await;
        } else if tick {
            // Only periodic checks count: a signal often precedes the profiles by a little.
            let retry = w.strike();
            let ms = since.map(|t| t.elapsed().as_millis());
            tracing::debug!(strikes = w.strikes, tries = w.tries, retry, ms, "iPhone link incomplete at check");
            if retry {
                retry_link(&phone, &mut w, link, l).await;
            }
        }
        tick = wait_link(&mut changes, &mut links, w.next_check()).await;
    }
}

/// Waits for a link, setting or call change, at most `every`; true: delay elapsed.
async fn wait_link(
    changes: &mut Option<impl futures_util::Stream<Item = ()> + Unpin>,
    links: &mut watch::Receiver<Links>,
    every: Duration,
) -> bool {
    let signal = async {
        match changes.as_mut() {
            Some(c) => c.next().await.is_some(),
            None => std::future::pending().await,
        }
    };
    let ended = tokio::select! {
        () = tokio::time::sleep(every) => return true,
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
            run_ui("bluez-msg-call", &[]);
            running.store(false, Ordering::Release);
        });
    }
}

/// Starts a window binary installed next to the daemon, with the display variables and `env`, and waits for it
/// (blocking: to be called from a dedicated thread).
fn run_ui(bin: &str, env: &[(&str, &str)]) {
    let exe = std::env::current_exe().map(|p| p.with_file_name(bin));
    let child = exe.and_then(|p| std::process::Command::new(p).envs(display_env()).envs(env.iter().copied()).stdin(std::process::Stdio::null()).spawn());
    match child {
        Ok(mut c) => {
            let _ = c.wait();
        }
        Err(e) => tracing::warn!("{bin}: {e}"),
    }
}

/// Shows the conversation with `number`: the open window receives [`Out::Open`], otherwise one is started on it.
/// The number is not logged: a rejection only says it is invalid.
fn open_conversation(number: &str, hub: &ipc::Hub) -> Result<(), String> {
    let number = bluez_message::phone::strip_separators(number);
    bluez_message::bmsg::validate_number(&number).map_err(|_| t!("open: invalid number").to_owned())?;
    if ipc::gui_open() {
        // Transient: never replayed to a window opened later.
        hub.publish(Out::Open { number }, true);
    } else {
        // One launch at a time: repeated `Open` before the window takes its lock start nothing more.
        static LAUNCHING: AtomicBool = AtomicBool::new(false);
        if LAUNCHING.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        // Number in the environment, not in argv (visible to all in `/proc`).
        std::thread::spawn(move || {
            run_ui("bluez-message", &[(ipc::OPEN_ENV, &number)]);
            LAUNCHING.store(false, Ordering::Release);
        });
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_restart_once_after_grace() {
        let mut w = LinkWatch::default();
        assert!(!w.early_due());
        assert!(w.next_check() <= WATCH_GRACE);
        w.bad_since = Instant::now().checked_sub(WATCH_GRACE);
        assert!(w.early_due());
        assert!(!w.early_due());
        assert_eq!(w.next_check(), WATCH_EVERY);
        w.healthy();
        assert!(!w.early_due());
    }

    #[test]
    fn received_sms_follows_conversation_flags() {
        use bluez_message::map::Folder;
        let sms = |handle: &str, read: bool| Sms { handle: handle.into(), address: "+33600000001".into(), timestamp: "20261015T100000".into(), text: "a".into(), read, ..Default::default() };
        let out = |sms, folder, new| Out::Sms { sms, folder, new };
        let mut st = State::default();
        st.apply(out(sms("1", true), Folder::Inbox, true));
        let key = st.convs[0].key.clone();
        let (k, flags, recap, fresh) = received(&st, &out(sms("2", false), Folder::Inbox, false)).unwrap();
        assert_eq!((k.as_str(), flags, recap, fresh), (key.as_str(), ConvFlags::default(), true, true));
        assert!(!received(&st, &out(sms("3", false), Folder::Inbox, true)).unwrap().2, "new: notified, not summed up");
        assert!(received(&st, &out(sms("3", false), Folder::Inbox, true)).unwrap().3, "new: lifts the archive");
        let late = received(&st, &out(sms("4", true), Folder::Inbox, false)).unwrap();
        assert!(!late.2, "already read");
        assert!(!late.3, "already read, listed late: stays archived");
        assert!(received(&st, &out(sms("5", false), Folder::Sent, false)).is_none());
        let both = ConvFlags { muted: true, archived: true };
        st.apply(Out::Conv { key: key.clone(), flags: both });
        assert_eq!(received(&st, &out(sms("6", false), Folder::Inbox, false)), Some((key, both, false, true)), "muted: out of the summary");
    }

    #[test]
    fn ringing_call_gets_sms_replies() {
        let fr = Some(numbering::Id::FR);
        let s = Settings::default();
        let ring = |number: Option<&str>, s: &Settings| {
            let mut c = bluez_message::app::state::Call::default();
            (c.phase, c.number) = (Phase::Incoming, number.map(Into::into));
            match for_notify(Effect::Call(c), s, fr) {
                Some(Effect::Ring(_, d)) => d,
                fx => panic!("{fx:?}"),
            }
        };
        let d = ring(Some("+33 6 12 34 56 78"), &s).expect("mobile");
        assert_eq!(d.number, "+33612345678");
        assert_eq!(d.text, s.quick_replies.first().cloned());
        assert_eq!(ring(Some("+33 1 23 45 67 89"), &s), None, "landline");
        assert_eq!(ring(Some("INFO"), &s), None, "not a number");
        assert_eq!(ring(None, &s), None, "hidden");
        let empty = Settings { quick_replies: Vec::new(), ..Settings::default() };
        assert_eq!(ring(Some("+33612345678"), &empty).and_then(|d| d.text), None, "only Decline and write");
        let mut active = bluez_message::app::state::Call::default();
        active.phase = Phase::Active;
        assert!(matches!(for_notify(Effect::Call(active), &s, fr), Some(Effect::Call(_))));
        let off = Settings { notify_calls: false, ..Settings::default() };
        assert_eq!(for_notify(Effect::Call(bluez_message::app::state::Call::default()), &off, fr), None);
    }
}
