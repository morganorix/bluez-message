//! Adapter, search and pairing (`org.bluez.Adapter1`, `org.bluez.Agent1`), available with or without
//! a configured device: an actor fed by [`Setup::submit`], publishing its [`Out`] to the daemon.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use zbus::zvariant::{ObjectPath, Value};
use zbus::{Connection, interface, proxy};

use crate::addr::BtAddr;
use crate::bluez::{self, ADAPTER, bounded, is_phone, prop_bool};
use crate::engine::{Cmd, Nearby, Out};
use crate::error::{Error, Result};
use crate::ipc::Clients;
use crate::t;

/// Agent object exported on the system bus.
const AGENT_PATH: &str = "/org/bluez_message/agent";
/// Simple BlueZ call (property, discovery).
const CALL: Duration = Duration::from_secs(3);
/// `Pair`, passkey confirmation included.
const PAIR: Duration = Duration::from_secs(60);
/// Wait for the user's answer to a passkey.
const ANSWER: Duration = Duration::from_secs(30);
/// Profile connection after pairing or on request.
const CONNECT: Duration = Duration::from_secs(30);
/// Search stopped after this delay.
const SEARCH: Duration = Duration::from_secs(60);
/// Adapter state reread every `REFRESH` ticks (1 s) while a client is connected.
const REFRESH: u32 = 5;
/// Pending commands.
const QUEUE: usize = 8;

#[proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
trait Adapter {
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    fn set_discovery_filter(&self, filter: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_powered(&self, v: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn discovering(&self) -> zbus::Result<bool>;
}

#[proxy(interface = "org.bluez.AgentManager1", default_service = "org.bluez", default_path = "/org/bluez")]
trait AgentManager {
    fn register_agent(&self, agent: &ObjectPath<'_>, capability: &str) -> zbus::Result<()>;
    fn unregister_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// Pairing in progress: its device and, while a passkey waits, the channel of the answer.
#[derive(Default)]
struct Pairing {
    target: Option<BtAddr>,
    answer: Option<oneshot::Sender<bool>>,
}

type Shared = Arc<Mutex<Pairing>>;

fn lock(p: &Shared) -> MutexGuard<'_, Pairing> {
    p.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Pairing actor: commands in, events out (published by the daemon).
pub struct Setup {
    tx: mpsc::Sender<(Cmd, Option<mpsc::Sender<Out>>)>,
    task: JoinHandle<()>,
}

impl Setup {
    /// Exports the agent on `conn` (the only system connection of the pairing) and starts the actor;
    /// the agent is registered with BlueZ at each pairing (BlueZ may have restarted).
    ///
    /// # Errors
    /// Agent path refused by the object server.
    pub async fn start(conn: Connection, out: mpsc::UnboundedSender<Out>, clients: Clients) -> Result<Self> {
        let pairing = Shared::default();
        conn.object_server().at(AGENT_PATH, Agent { pairing: pairing.clone(), out: out.clone() }).await?;
        let adapter = AdapterProxy::builder(&conn).path(ADAPTER)?.cache_properties(zbus::proxy::CacheProperties::No).build().await?;
        let (tx, rx) = mpsc::channel(QUEUE);
        let actor = Actor { conn, adapter, out, clients, pairing, agent: false, search: None, adapter_state: None, found: None };
        Ok(Self { tx, task: tokio::spawn(actor.run(rx)) })
    }

    /// Queues a setup command ([`Cmd::is_setup`]); its error goes to `reply` only.
    ///
    /// # Errors
    /// Translated message: queue full or actor stopped.
    pub fn submit(&self, cmd: Cmd, reply: Option<mpsc::Sender<Out>>) -> std::result::Result<(), String> {
        self.tx.try_send((cmd, reply)).map_err(|_| t!("setup queue full").to_owned())
    }

    /// Stops the search started here, cancels the pairing, unregisters the agent (5 s at most).
    pub async fn shutdown(self) {
        drop(self.tx);
        let mut task = self.task;
        if tokio::time::timeout(Duration::from_secs(5), &mut task).await.is_err() {
            task.abort();
        }
    }
}

struct Actor {
    conn: Connection,
    adapter: AdapterProxy<'static>,
    out: mpsc::UnboundedSender<Out>,
    clients: Clients,
    pairing: Shared,
    /// Agent registered with BlueZ at least once.
    agent: bool,
    /// End of the search started by this actor.
    search: Option<Instant>,
    /// Last published states, to publish only the changes.
    adapter_state: Option<Out>,
    found: Option<Out>,
}

impl Actor {
    async fn run(mut self, mut rx: mpsc::Receiver<(Cmd, Option<mpsc::Sender<Out>>)>) {
        let mut tasks = JoinSet::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut ticks = 0u32;
        self.refresh().await;
        loop {
            tokio::select! {
                r = rx.recv() => {
                    let Some((cmd, reply)) = r else { break };
                    if let Err(e) = self.handle(cmd, reply.as_ref(), &mut tasks).await
                        && let Some(reply) = reply
                    {
                        let _ = reply.try_send(Out::Error { msg: e.to_string() });
                    }
                    self.refresh().await;
                }
                _ = tick.tick() => {
                    ticks = ticks.wrapping_add(1);
                    if let Some(end) = self.search {
                        if Instant::now() >= end || self.clients.count() == 0 {
                            self.stop().await;
                            self.refresh().await;
                        } else {
                            self.scan().await;
                        }
                    }
                    if ticks.is_multiple_of(REFRESH) && self.clients.count() > 0 {
                        self.refresh().await;
                    }
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
        tasks.shutdown().await;
        self.close().await;
    }

    async fn handle(&mut self, cmd: Cmd, reply: Option<&mpsc::Sender<Out>>, tasks: &mut JoinSet<()>) -> Result<()> {
        match cmd {
            Cmd::Powered { on } => bounded(CALL, self.adapter.set_powered(on)).await?,
            Cmd::Discover { on: true } => {
                // Classic Bluetooth only: phones (HFP, MAP) are BR/EDR. Filter refused: unfiltered search.
                let filter = HashMap::from([("Transport", Value::from("bredr"))]);
                let _ = bounded(CALL, self.adapter.set_discovery_filter(filter)).await;
                match bounded(CALL, self.adapter.start_discovery()).await {
                    Err(e) if !bluez::is_error(&e, "org.bluez.Error.InProgress") => return Err(e),
                    _ => {}
                }
                self.search = Some(Instant::now() + SEARCH);
                self.found = None;
                self.scan().await;
            }
            Cmd::Discover { on: false } => self.stop().await,
            Cmd::Pair { addr } => {
                {
                    let mut p = lock(&self.pairing);
                    if p.target.is_some() {
                        return Err(Error::PairingBusy);
                    }
                    p.target = Some(addr.clone());
                }
                if let Err(e) = self.register().await {
                    *lock(&self.pairing) = Pairing::default();
                    return Err(e);
                }
                // The search slows down pairing.
                self.stop().await;
                tasks.spawn(pair(self.conn.clone(), addr, self.pairing.clone(), self.out.clone()));
            }
            Cmd::Confirm { ok } => {
                let answer = lock(&self.pairing).answer.take().ok_or(Error::NoConfirmation)?;
                let _ = answer.send(ok);
            }
            Cmd::Connect { addr } => {
                let (conn, reply) = (self.conn.clone(), reply.cloned());
                tasks.spawn(async move {
                    if let Err(e) = connect(&conn, &addr).await
                        && let Some(reply) = reply
                    {
                        let _ = reply.send(Out::Error { msg: e.to_string() }).await;
                    }
                });
            }
            _ => {}
        }
        Ok(())
    }

    /// Registers the agent (already registered: success).
    async fn register(&mut self) -> Result<()> {
        let am = AgentManagerProxy::new(&self.conn).await?;
        let path = ObjectPath::from_static_str_unchecked(AGENT_PATH);
        match bounded(CALL, am.register_agent(&path, "DisplayYesNo")).await {
            Err(e) if !bluez::is_error(&e, "org.bluez.Error.AlreadyExists") => return Err(e),
            _ => {}
        }
        self.agent = true;
        Ok(())
    }

    /// Stops the search started here (the one of another program stays).
    async fn stop(&mut self) {
        if self.search.take().is_some()
            && let Err(e) = bounded(CALL, self.adapter.stop_discovery()).await
        {
            tracing::debug!("StopDiscovery: {e}");
        }
    }

    /// Devices seen, phones first then by name; published if the list changed.
    async fn scan(&mut self) {
        let objs = match bounded(CALL, bluez::managed(&self.conn)).await {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!("search: {e}");
                return;
            }
        };
        let mut found: Vec<Nearby> = bluez::devices(&objs)
            .map(|(addr, d)| {
                let name = bluez::name(d, &addr);
                Nearby::new(addr, &name, prop_bool(d, "Paired"), prop_bool(d, "Connected"), is_phone(d))
            })
            .collect();
        found.sort_by_cached_key(|d| (!d.phone, d.name.to_lowercase()));
        let o = Out::found(found);
        if self.found.as_ref() != Some(&o) {
            self.found = Some(o.clone());
            let _ = self.out.send(o);
        }
    }

    /// Adapter state, published if it changed; unreadable: adapter absent.
    async fn refresh(&mut self) {
        let read = async { Ok::<_, Error>((self.adapter.powered().await?, self.adapter.discovering().await?)) };
        let o = match bounded(CALL, read).await {
            Ok((powered, discovering)) => Out::Adapter { present: true, powered, discovering },
            Err(_) => Out::Adapter { present: false, powered: false, discovering: false },
        };
        if matches!(o, Out::Adapter { powered: false, .. }) {
            self.search = None;
        }
        if self.adapter_state.as_ref() != Some(&o) {
            self.adapter_state = Some(o.clone());
            let _ = self.out.send(o);
        }
    }

    async fn close(&mut self) {
        let target = std::mem::take(&mut *lock(&self.pairing)).target;
        if let Some(addr) = target
            && let Ok(dev) = bluez::device(&self.conn, &addr).await
        {
            let _ = bounded(CALL, dev.cancel_pairing()).await;
        }
        self.stop().await;
        let path = ObjectPath::from_static_str_unchecked(AGENT_PATH);
        if self.agent
            && let Ok(am) = AgentManagerProxy::new(&self.conn).await
        {
            let _ = bounded(CALL, am.unregister_agent(&path)).await;
        }
        let _ = self.conn.object_server().remove::<Agent, _>(&path).await;
    }
}

/// `Pair` (already paired: success), `Trusted`, then `Out::Paired` and best-effort profile connection.
async fn pair(conn: Connection, addr: BtAddr, pairing: Shared, out: mpsc::UnboundedSender<Out>) {
    let r = async {
        let dev = bluez::device(&conn, &addr).await?;
        match bounded(PAIR, dev.pair()).await {
            Err(e) if !bluez::is_error(&e, "org.bluez.Error.AlreadyExists") => return Err(e),
            _ => {}
        }
        // Trusted: the phone reconnects its profiles without authorization.
        bounded(CALL, dev.set_trusted(true)).await?;
        Ok(dev)
    }
    .await;
    *lock(&pairing) = Pairing::default();
    match r {
        Ok(dev) => {
            let _ = out.send(Out::Paired { addr, ok: true, error: None });
            if let Err(e) = bounded(CONNECT, dev.connect()).await {
                tracing::debug!("connection after pairing: {e}");
            }
        }
        Err(e) => {
            tracing::info!("pairing failed: {e}");
            let _ = out.send(Out::Paired { addr, ok: false, error: Some(e.to_string()) });
        }
    }
}

async fn connect(conn: &Connection, addr: &BtAddr) -> Result<()> {
    let dev = bluez::device(conn, addr).await?;
    bounded(CONNECT, dev.connect()).await
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
}

fn rejected() -> AgentError {
    AgentError::Rejected("rejected by bluez-message".into())
}

/// `org.bluez.Agent1`: answers only for the device being paired here.
struct Agent {
    pairing: Shared,
    out: mpsc::UnboundedSender<Out>,
}

impl Agent {
    fn is_target(&self, device: &ObjectPath<'_>) -> bool {
        lock(&self.pairing).target.as_ref().is_some_and(|a| a.path() == device.as_str())
    }
}

// Signatures imposed by `org.bluez.Agent1` (zbus methods are async, unused arguments kept).
#[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
#[interface(name = "org.bluez.Agent1")]
impl Agent {
    /// Passkey shown on the phone: accepted only if the user confirms it within 30 s.
    async fn request_confirmation(&self, device: ObjectPath<'_>, passkey: u32) -> std::result::Result<(), AgentError> {
        let rx = {
            let mut p = lock(&self.pairing);
            let ask = p.target.clone().filter(|a| a.path() == device.as_str()).and_then(|a| Out::confirm(a, passkey));
            let Some(ask) = ask else { return Err(rejected()) };
            let (tx, rx) = oneshot::channel();
            p.answer = Some(tx);
            let _ = self.out.send(ask);
            rx
        };
        let answer = tokio::time::timeout(ANSWER, rx).await;
        lock(&self.pairing).answer = None;
        match answer {
            Ok(Ok(true)) => Ok(()),
            _ => Err(rejected()),
        }
    }

    async fn request_authorization(&self, device: ObjectPath<'_>) -> std::result::Result<(), AgentError> {
        if self.is_target(&device) { Ok(()) } else { Err(rejected()) }
    }

    async fn authorize_service(&self, device: ObjectPath<'_>, _uuid: &str) -> std::result::Result<(), AgentError> {
        if self.is_target(&device) { Ok(()) } else { Err(rejected()) }
    }

    /// PIN codes and typed passkeys are not supported (`DisplayYesNo`).
    async fn request_pin_code(&self, _device: ObjectPath<'_>) -> std::result::Result<String, AgentError> {
        Err(rejected())
    }

    async fn request_passkey(&self, _device: ObjectPath<'_>) -> std::result::Result<u32, AgentError> {
        Err(rejected())
    }

    async fn display_pin_code(&self, _device: ObjectPath<'_>, _pincode: &str) -> std::result::Result<(), AgentError> {
        Err(rejected())
    }

    async fn display_passkey(&self, _device: ObjectPath<'_>, _passkey: u32, _entered: u16) {}

    /// Request cancelled by BlueZ: the pending answer is dropped (the confirmation is rejected).
    async fn cancel(&self) {
        lock(&self.pairing).answer = None;
    }

    async fn release(&self) {
        lock(&self.pairing).answer = None;
    }
}
