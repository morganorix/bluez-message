//! Unix socket of the `bluez-messaged` daemon: [`Out`] events go out as JSON lines to each client (GUI),
//! [`Cmd`] commands come back as JSON lines. On connection, a client first receives a snapshot
//! (links, known SMS, sends, current call) to rebuild its state without notifications.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io;
use std::fs::{File, TryLockError};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, broadcast, mpsc};

use crate::engine::{CallKind, Cmd, Item, Out, Profile, parse_cmd};
use crate::hfp::HfEvent;
use crate::history::SENT_ECHO_SECS;
use crate::map::Folder;
use crate::numbering;
use crate::phone::sms_peer;
use crate::stamp;
use crate::t;

/// Maximum length of a client line (bytes); beyond it the connection is closed.
pub const MAX_LINE: usize = 16 * 1024;
/// Maximum length of a daemon line seen by the GUI (the contacts fit on one line).
const MAX_DAEMON_LINE: usize = 16 * 1024 * 1024;
const MAX_SMS: usize = 1000;
const MAX_SENT: usize = 200;
const MAX_CALL: usize = 64;
/// Call history replayed to the clients.
const MAX_LOG: usize = 500;
const RETRY: Duration = Duration::from_secs(2);
const MAX_CLIENTS: usize = 16;
/// A client that stops reading is disconnected after this delay.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const BROADCAST: usize = 4096;
/// Conversations whose read mark is replayed; beyond it the oldest mark is dropped.
const MAX_READS: usize = 4096;

/// What a connecting client must replay. Entries are shared (`Arc`): the replay taken under
/// the hub lock copies pointers, not the phonebook.
#[derive(Debug, Default)]
pub struct Snapshot {
    status: BTreeMap<Profile, bool>,
    slc: bool,
    sms: VecDeque<Arc<Out>>,
    keys: HashSet<(Folder, String)>,
    sent: VecDeque<Arc<Out>>,
    /// Events of the current call; cleared when the call ends.
    call: Vec<Arc<Out>>,
    log: VecDeque<Arc<Out>>,
    contacts: Option<Arc<Out>>,
    battery: Option<Arc<Out>>,
    settings: Option<Arc<Out>>,
    adapter: Option<Arc<Out>>,
    found: Option<Arc<Out>>,
    /// Passkey awaiting an answer; cleared by the end of the pairing.
    confirm: Option<Arc<Out>>,
    /// Read up to, per conversation.
    reads: BTreeMap<String, String>,
}

fn push_capped(q: &mut VecDeque<Arc<Out>>, o: Out, cap: usize) -> Option<Arc<Out>> {
    q.push_back(Arc::new(o));
    if q.len() > cap { q.pop_front() } else { None }
}

impl Snapshot {
    /// `call_idle`: no call in progress after applying `o` (the daemon's state machine).
    pub fn record(&mut self, o: &Out, call_idle: bool) {
        match o {
            Out::Status { link, up } => {
                self.status.insert(*link, *up);
                if *link == Profile::Hfp && !up {
                    self.slc = false;
                    self.call.clear();
                }
            }
            Out::Sms { sms, folder, .. } => {
                let o = Out::Sms { sms: sms.clone(), folder: *folder, new: false };
                if !self.keys.insert((*folder, sms.handle.clone())) {
                    // Already known: the most recent version is kept (read status, etc.).
                    let same = |x: &Out| matches!(x, Out::Sms { sms: s, folder: f, .. } if f == folder && s.handle == sms.handle);
                    if let Some(slot) = self.sms.iter_mut().find(|x| same(x)) {
                        *slot = Arc::new(o);
                    }
                } else if let Some(old) = push_capped(&mut self.sms, o, MAX_SMS)
                    && let Out::Sms { sms, folder, .. } = &*old
                {
                    self.keys.remove(&(*folder, sms.handle.clone()));
                }
            }
            // Failures included: the "not sent" bubble is still there when the app reopens.
            Out::Sent { .. } => {
                push_capped(&mut self.sent, o.clone(), MAX_SENT);
            }
            Out::Deleted { key, item } => self.forget(key, item.as_ref()),
            Out::CallLog { .. } => {
                push_capped(&mut self.log, o.clone(), MAX_LOG);
            }
            Out::Contacts { .. } => self.contacts = Some(Arc::new(o.clone())),
            Out::Battery { .. } => self.battery = Some(Arc::new(o.clone())),
            Out::Settings { .. } => self.settings = Some(Arc::new(o.clone())),
            Out::Adapter { .. } => self.adapter = Some(Arc::new(o.clone())),
            Out::Found { .. } => self.found = Some(Arc::new(o.clone())),
            Out::Confirm { .. } => self.confirm = Some(Arc::new(o.clone())),
            Out::Paired { .. } => self.confirm = None,
            Out::Cleared => {
                self.sms.clear();
                self.keys.clear();
                self.sent.clear();
                self.log.clear();
                self.reads.clear();
            }
            Out::Read { key, until } => {
                if self.reads.len() >= MAX_READS
                    && !self.reads.contains_key(key)
                    && let Some(oldest) = self.reads.iter().min_by(|a, b| a.1.cmp(b.1)).map(|(k, _)| k.clone())
                {
                    self.reads.remove(&oldest);
                }
                let r = self.reads.entry(key.clone()).or_default();
                if until > r {
                    r.clone_from(until);
                }
            }
            Out::Muted { .. } if !call_idle => {
                self.call.retain(|c| !matches!(**c, Out::Muted { .. }));
                self.call.push(Arc::new(o.clone()));
            }
            Out::Call(HfEvent::SlcReady) => self.slc = true,
            Out::Call(_) if call_idle => self.call.clear(),
            Out::Call(_) if self.call.len() < MAX_CALL => self.call.push(Arc::new(o.clone())),
            _ => {}
        }
    }

    /// Removes what was deleted from the history (item or whole conversation).
    fn forget(&mut self, key: &str, item: Option<&Item>) {
        let home = match self.settings.as_deref() {
            Some(Out::Settings { home, .. }) => home.as_deref().and_then(numbering::parse_code),
            _ => None,
        };
        let number_key = |n: &str| numbering::key(n, home);
        let hit = |o: &Arc<Out>| match &**o {
            Out::Sms { sms, folder, .. } => {
                let sent = folder.is_sent();
                number_key(sms_peer(sms, sent).0) == key
                    && item.is_none_or(|i| !i.call && i.outgoing == sent && i.timestamp == sms.timestamp && i.text == sms.text)
            }
            // The kept send carries the PC time; the deleted item, often the phone's (`sent` return).
            Out::Sent { to, text, timestamp, .. } => {
                number_key(to) == key
                    && item.is_none_or(|i| {
                        !i.call && i.outgoing && i.text == *text
                            && (i.timestamp == *timestamp
                                || stamp::secs(&i.timestamp).zip(stamp::secs(timestamp)).is_some_and(|(a, b)| (a - b).abs() <= SENT_ECHO_SECS))
                    })
            }
            Out::CallLog { number, timestamp, kind, .. } => {
                number_key(number) == key && item.is_none_or(|i| i.call && i.timestamp == *timestamp && i.outgoing == (*kind == CallKind::Out))
            }
            _ => false,
        };
        let keys = &mut self.keys;
        self.sms.retain(|o| {
            let gone = hit(o);
            if gone && let Out::Sms { sms, folder, .. } = &**o {
                keys.remove(&(*folder, sms.handle.clone()));
            }
            !gone
        });
        self.sent.retain(|o| !hit(o));
        self.log.retain(|o| !hit(o));
    }

    /// Bounded by the caps above; cheap to take under the hub lock (shared entries).
    #[must_use]
    pub fn replay(&self) -> Vec<Arc<Out>> {
        // Settings first: they tell the client which device (or setup mode) the rest belongs to.
        let mut v: Vec<Arc<Out>> = [&self.settings, &self.adapter, &self.found, &self.confirm].into_iter().flatten().cloned().collect();
        v.extend(self.status.iter().map(|(link, up)| Arc::new(Out::Status { link: *link, up: *up })));
        if self.slc {
            v.push(Arc::new(Out::Call(HfEvent::SlcReady)));
        }
        v.extend(self.battery.iter().cloned());
        v.extend(self.contacts.iter().cloned());
        v.extend(self.sms.iter().cloned());
        v.extend(self.log.iter().cloned());
        v.extend(self.sent.iter().cloned());
        v.extend(self.reads.iter().map(|(key, until)| Arc::new(Out::Read { key: key.clone(), until: until.clone() })));
        v.extend(self.call.iter().cloned());
        v
    }
}

/// Command from a client, with the channel to send back an error that concerns only that client.
pub type Request = (Cmd, Option<mpsc::Sender<Out>>);

type Shared = Arc<Mutex<(Snapshot, broadcast::Sender<Out>)>>;

/// Daemon side: publishes events to the clients. Holds the instance lock.
pub struct Hub {
    inner: Shared,
    _lock: File,
}

impl Hub {
    /// Snapshot and broadcast under the same lock: a connecting client neither loses nor duplicates anything.
    pub fn publish(&self, o: Out, call_idle: bool) {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.0.record(&o, call_idle);
        let _ = g.1.send(o);
    }

    /// Counter of connected clients, usable outside the hub (the search stops when none is left).
    #[must_use]
    pub fn clients(&self) -> Clients {
        Clients(self.inner.lock().unwrap_or_else(PoisonError::into_inner).1.downgrade())
    }
}

/// Connected clients of a [`Hub`]; weak: does not keep the broadcast open.
#[derive(Clone)]
pub struct Clients(broadcast::WeakSender<Out>);

impl Clients {
    #[must_use]
    pub fn count(&self) -> usize {
        self.0.upgrade().map_or(0, |s| s.receiver_count())
    }
}

/// Bound socket and instance lock (`<socket>.lock`, to be kept for the whole life of the daemon).
#[derive(Debug)]
pub struct Bound {
    listener: UnixListener,
    lock: File,
}

/// Instance lock file (daemon, GUI): created 0600, a symbolic link is refused.
///
/// # Errors
/// File cannot be opened or created.
pub fn lock_file(path: &Path) -> io::Result<File> {
    File::options().create(true).truncate(false).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)
}

/// Variable through which the daemon gives the GUI the conversation to open: unlike an argument, the environment
/// of a process is not readable by other users (`/proc/<pid>/environ`).
pub const OPEN_ENV: &str = "BLUEZ_MESSAGE_OPEN";

/// A window holds its instance lock ([`crate::config::gui_lock_path`]). Probing takes the lock for an instant
/// when it is free: a window started at that very moment exits on it (see the GUI's `--open` forwarding).
#[must_use]
pub fn gui_open() -> bool {
    let Ok(path) = crate::config::gui_lock_path() else { return false };
    lock_file(&path).is_ok_and(|f| matches!(f.try_lock(), Err(TryLockError::WouldBlock)))
}

/// Main windows subscribed with [`crate::engine::Cmd::Window`], and the last [`Out::Open`] held for a
/// window holding its lock but not subscribed yet (connecting, or the daemon restarted under it).
#[derive(Default)]
pub struct Windows {
    open: Vec<mpsc::Sender<Out>>,
    held: Option<(Option<String>, Instant)>,
}

impl Windows {
    /// Age past which a held `Open` is dropped instead of handed to the next window.
    pub const HOLD: Duration = Duration::from_secs(10);

    /// Passes `Open { number }` to the subscribed windows; `false`: none took it (gone or not reading),
    /// the caller holds or launches.
    pub fn deliver(&mut self, number: Option<&str>) -> bool {
        self.open.retain(|w| !w.is_closed());
        let mut taken = false;
        for w in &self.open {
            taken |= w.try_send(Out::Open { number: number.map(str::to_owned) }).is_ok();
        }
        taken
    }

    /// Keeps `Open { number }` for the next window, replacing an older one.
    pub fn hold(&mut self, number: Option<String>, now: Instant) {
        self.held = Some((number, now));
    }

    /// Subscribes a window and hands it the held `Open`, once, if it is younger than [`Self::HOLD`].
    pub fn add(&mut self, w: mpsc::Sender<Out>, now: Instant) {
        if let Some((number, at)) = self.held.take()
            && now.saturating_duration_since(at) < Self::HOLD
        {
            let _ = w.try_send(Out::Open { number });
        }
        // A window subscribing twice is kept once.
        self.open.retain(|o| !o.is_closed() && !o.same_channel(&w));
        self.open.push(w);
    }
}

/// Exclusive non-blocking lock: two daemons started together cannot steal each other's socket.
fn lock(path: &Path) -> io::Result<File> {
    let f = lock_file(path)?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(TryLockError::WouldBlock) => Err(io::Error::new(io::ErrorKind::AddrInUse, t!("daemon already running (lock {0})", path.display()))),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// The socket is only safe in a directory of one's own, closed to others: otherwise a third party could connect to it
/// before the `chmod`, or replace the socket and the lock.
fn private_parent(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let m = std::fs::metadata(dir)?;
    // SAFETY: geteuid has no precondition and cannot fail.
    let me = unsafe { libc::geteuid() };
    if m.uid() != me || m.mode() & 0o077 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, t!("{0} must belong to you and be closed to others (0700)", dir.display())));
    }
    Ok(())
}

/// Takes the lock, refuses if a daemon already answers, removes an orphan socket, permissions 0600.
///
/// # Errors
/// Lock taken, daemon already active, or socket cannot be created.
pub async fn bind(path: &Path) -> io::Result<Bound> {
    private_parent(path)?;
    let lock = lock(&path.with_extension("lock"))?;
    if UnixStream::connect(path).await.is_ok() {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, t!("daemon already running ({0})", path.display())));
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => return Err(io::Error::new(io::ErrorKind::AlreadyExists, t!("{0} is not a socket", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(Bound { listener, lock })
}

/// Accepts clients in the background; their commands arrive in `requests`.
pub fn spawn_server(bound: Bound, requests: mpsc::Sender<Request>) -> Hub {
    let Bound { listener, lock } = bound;
    let (tx, _) = broadcast::channel(BROADCAST);
    let inner: Shared = Arc::new(Mutex::new((Snapshot::default(), tx)));
    let shared = Arc::clone(&inner);
    let slots = Arc::new(Semaphore::new(MAX_CLIENTS));
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((s, _)) => {
                    // Beyond MAX_CLIENTS, the connection is closed immediately.
                    let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                        tracing::warn!("too many clients: connection refused");
                        continue;
                    };
                    let (shared, requests) = (Arc::clone(&shared), requests.clone());
                    tokio::spawn(async move {
                        serve_client(s, shared, requests).await;
                        drop(permit);
                    });
                }
                Err(e) => {
                    tracing::warn!("socket: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });
    Hub { inner, _lock: lock }
}

async fn write(w: &mut OwnedWriteHalf, o: &Out) -> io::Result<()> {
    let mut line = serde_json::to_string(o).map_err(io::Error::other)?;
    line.push('\n');
    tokio::time::timeout(WRITE_TIMEOUT, w.write_all(line.as_bytes()))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "client stuck"))?
}

async fn serve_client(stream: UnixStream, shared: Shared, requests: mpsc::Sender<Request>) {
    let (replay, mut events) = {
        let g = shared.lock().unwrap_or_else(PoisonError::into_inner);
        (g.0.replay(), g.1.subscribe())
    };
    let (r, mut w) = stream.into_split();
    // The reader holds the only reply sender: its end (EOF, line too long) closes writing.
    let (reply_tx, mut replies) = mpsc::channel::<Out>(8);
    let reader = tokio::spawn(read_commands(BufReader::new(r), reply_tx, requests));
    let _abort = AbortOnDrop(reader);
    for o in &replay {
        if write(&mut w, o).await.is_err() {
            return;
        }
    }
    loop {
        let o = tokio::select! {
            e = events.recv() => match e {
                Ok(o) => o,
                // Client too slow: it reconnects and starts again from the snapshot.
                Err(_) => return,
            },
            r = replies.recv() => match r {
                Some(o) => o,
                None => return,
            },
        };
        if write(&mut w, &o).await.is_err() {
            return;
        }
    }
}

/// Stops reading commands when writing to the client stops.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn read_commands(mut r: BufReader<tokio::net::unix::OwnedReadHalf>, reply: mpsc::Sender<Out>, requests: mpsc::Sender<Request>) {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut r).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) if buf.last() != Some(&b'\n') && buf.len() > MAX_LINE => {
                let _ = reply.send(Out::Error { msg: t!("line longer than {0} bytes rejected", MAX_LINE) }).await;
                return;
            }
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        if line.trim().is_empty() {
            continue;
        }
        match parse_cmd(&line) {
            // A client cannot stop the daemon: `quit` only closes its own connection.
            Ok(Cmd::Quit) => return,
            Ok(cmd) => {
                if requests.send((cmd, Some(reply.clone()))).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = reply.send(Out::Error { msg: e.to_string() }).await;
            }
        }
    }
}

/// GUI side: state of the link with the daemon.
#[derive(Debug)]
pub enum Link {
    /// Connected: the snapshot follows, the local state must be reset.
    Up,
    Down(String),
    Event(Box<Out>),
}

/// Connects to the daemon and stays there (retry every 2 s) until `cmds` is closed.
pub async fn client(path: PathBuf, mut cmds: mpsc::UnboundedReceiver<Cmd>, mut on: impl FnMut(Link)) {
    let mut reported = false;
    loop {
        match UnixStream::connect(&path).await {
            Ok(s) => {
                reported = false;
                on(Link::Up);
                match session(s, &mut cmds, &mut on).await {
                    Ok(()) => return,
                    Err(msg) => on(Link::Down(msg)),
                }
            }
            Err(e) if !reported => {
                reported = true;
                on(Link::Down(t!("bluez-messaged daemon unreachable: {0}", e)));
                tokio::spawn(start_daemon());
            }
            Err(_) => {}
        }
        tokio::select! {
            () = tokio::time::sleep(RETRY) => {}
            c = cmds.recv() => match c {
                None => return,
                Some(_) => on(Link::Down(t!("bluez-messaged daemon unreachable: command ignored").into())),
            },
        }
    }
}

/// Starts the daemon service (not enabled, or launched before its target was reached); without waiting:
/// the client's periodic reconnection will find it. Service missing (manual install): no effect.
async fn start_daemon() {
    let r = tokio::process::Command::new("systemctl")
        .args(["--user", "start", "--no-block", "bluez-messaged.service"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(Duration::from_secs(5), r).await {
        Err(_) => tracing::debug!("systemctl start bluez-messaged: timed out"),
        Ok(Ok(st)) if st.success() => tracing::info!("bluez-messaged service start requested"),
        Ok(Ok(st)) => tracing::debug!("systemctl start bluez-messaged: {st}"),
        Ok(Err(e)) => tracing::debug!("systemctl: {e}"),
    }
}

/// `Ok`: `cmds` closed; `Err`: link lost.
async fn session(s: UnixStream, cmds: &mut mpsc::UnboundedReceiver<Cmd>, on: &mut impl FnMut(Link)) -> Result<(), String> {
    let (r, mut w) = s.into_split();
    let mut r = BufReader::new(r);
    // Kept from one round to the next: a read interrupted by `select!` keeps its bytes in `buf`.
    let mut buf = Vec::new();
    loop {
        let mut limited = (&mut r).take((MAX_DAEMON_LINE + 1 - buf.len()) as u64);
        tokio::select! {
            n = limited.read_until(b'\n', &mut buf) => match n {
                Ok(0) => return Err(t!("bluez-messaged daemon stopped").into()),
                Ok(_) if buf.last() == Some(&b'\n') => {
                    match serde_json::from_slice::<Out>(&buf) {
                        Ok(o) => on(Link::Event(Box::new(o))),
                        Err(e) => tracing::debug!("unreadable daemon line: {e}"),
                    }
                    buf.clear();
                }
                Ok(_) if buf.len() > MAX_DAEMON_LINE => return Err(t!("line longer than {0} bytes rejected", MAX_DAEMON_LINE)),
                Ok(_) => {}
                Err(e) => return Err(t!("bluez-messaged daemon: {0}", e)),
            },
            c = cmds.recv() => {
                let Some(c) = c else { return Ok(()) };
                let mut line = serde_json::to_string(&c).map_err(|e| e.to_string())?;
                line.push('\n');
                w.write_all(line.as_bytes()).await.map_err(|e| t!("bluez-messaged daemon: {0}", e))?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Sms;

    fn sms(h: &str, new: bool) -> Out {
        Out::Sms { sms: Sms { handle: h.into(), text: "t".into(), ..Default::default() }, folder: Folder::Inbox, new }
    }

    fn ind(name: &str, value: u8) -> Out {
        Out::Call(HfEvent::Indicator { name: name.into(), value })
    }

    fn replayed(s: &Snapshot) -> Vec<Out> {
        s.replay().iter().map(|o| (**o).clone()).collect()
    }

    #[test]
    fn snapshot_replays_state_without_new_flags() {
        let mut s = Snapshot::default();
        s.record(&Out::Status { link: Profile::Hfp, up: true }, true);
        s.record(&Out::Call(HfEvent::SlcReady), true);
        s.record(&sms("1", true), true);
        s.record(&sms("1", true), true); // duplicate: replaces without duplicating
        s.record(&Out::Call(HfEvent::IncomingCall), false);
        s.record(&Out::Error { msg: "x".into() }, false);
        let r = replayed(&s);
        assert_eq!(
            r,
            [Out::Status { link: Profile::Hfp, up: true }, Out::Call(HfEvent::SlcReady), sms("1", false), Out::Call(HfEvent::IncomingCall)]
        );
        // End of call: nothing of the call is replayed any more.
        s.record(&ind("callsetup", 0), true);
        assert_eq!(replayed(&s).len(), 3);
        // HFP link lost: SLC forgotten.
        s.record(&Out::Status { link: Profile::Hfp, up: false }, true);
        assert!(!replayed(&s).contains(&Out::Call(HfEvent::SlcReady)));
    }

    #[test]
    fn snapshot_forgets_deleted_and_keeps_failures() {
        let mut s = Snapshot::default();
        let mut a = Sms { handle: "1".into(), address: "+33612345678".into(), timestamp: "20261015T100000".into(), text: "a".into(), ..Default::default() };
        s.record(&Out::Sms { sms: a.clone(), folder: Folder::Inbox, new: true }, true);
        a.handle = "2".into();
        a.text = "b".into();
        s.record(&Out::Sms { sms: a, folder: Folder::Inbox, new: true }, true);
        let fail = Out::Sent { to: "0612345678".into(), text: "c".into(), ok: false, error: None, timestamp: "20261015T110000".into() };
        s.record(&fail, true);
        s.record(&Out::CallLog { number: "0612345678".into(), timestamp: "20261015T120000".into(), kind: CallKind::Out, secs: 3, read: false }, true);
        assert_eq!(replayed(&s).len(), 4, "send failure replayed");
        let item = Item { timestamp: "20261015T100000".into(), text: "a".into(), outgoing: false, call: false };
        s.record(&Out::Deleted { key: "612345678".into(), item: Some(item) }, true);
        assert_eq!(replayed(&s).len(), 3);
        assert!(!s.keys.contains(&(Folder::Inbox, "1".into())));
        s.record(&Out::Deleted { key: "612345678".into(), item: None }, true);
        assert!(replayed(&s).is_empty());
    }

    #[test]
    fn snapshot_forgets_by_country_key() {
        let mut s = Snapshot::default();
        let settings = Out::Settings {
            settings: crate::settings::Settings::default(),
            langs: vec![],
            devices: vec![],
            device: Some("00:11:22:33:44:55".parse().unwrap()),
            home: Some("fr".into()),
            auto: Some("fr".into()),
        };
        s.record(&settings, true);
        s.record(&Out::CallLog { number: "03 89 00 00 00".into(), timestamp: "20261015T120000".into(), kind: CallKind::Out, secs: 3, read: false }, true);
        s.record(&Out::Deleted { key: "+33389000000".into(), item: None }, true);
        assert_eq!(replayed(&s), [settings], "conversation keyed in E.164 forgotten");
    }

    #[test]
    fn snapshot_replays_setup_after_settings() {
        let mut s = Snapshot::default();
        let addr: crate::addr::BtAddr = "00:11:22:33:44:55".parse().unwrap();
        let settings = Out::Settings { settings: crate::settings::Settings::default(), langs: vec![], devices: vec![], device: None, home: None, auto: None };
        let adapter = Out::Adapter { present: true, powered: true, discovering: false };
        let confirm = Out::Confirm { addr: addr.clone(), passkey: 123_456 };
        s.record(&Out::Status { link: Profile::Map, up: false }, true);
        s.record(&adapter, true);
        s.record(&confirm, true);
        s.record(&settings, true);
        assert_eq!(replayed(&s), [settings.clone(), adapter.clone(), confirm, Out::Status { link: Profile::Map, up: false }]);
        // Pairing over: the confirmation is no longer replayed.
        s.record(&Out::Paired { addr, ok: true, error: None }, true);
        assert_eq!(replayed(&s), [settings, adapter, Out::Status { link: Profile::Map, up: false }]);
    }

    #[test]
    fn snapshot_caps_read_marks_dropping_oldest() {
        let mut s = Snapshot::default();
        let read = |k: usize, until: &str| Out::Read { key: format!("+3361234{k:04}"), until: until.into() };
        s.record(&read(0, "20260101T000000"), true);
        for k in 1..MAX_READS {
            s.record(&read(k, "20260601T000000"), true);
        }
        s.record(&read(MAX_READS, "20260701T000000"), true);
        assert_eq!(s.reads.len(), MAX_READS);
        assert!(!s.reads.contains_key("+33612340000"), "oldest mark dropped");
        // Updating a known conversation never evicts.
        s.record(&read(1, "20260801T000000"), true);
        assert_eq!(s.reads.len(), MAX_READS);
    }

    #[test]
    fn snapshot_replays_last_battery() {
        let mut s = Snapshot::default();
        s.record(&Out::Battery { percent: Some(80) }, true);
        s.record(&Out::Battery { percent: Some(79) }, true);
        assert_eq!(replayed(&s), [Out::Battery { percent: Some(79) }]);
        s.record(&Out::Battery { percent: None }, true);
        assert_eq!(replayed(&s), [Out::Battery { percent: None }]);
    }

    #[test]
    fn snapshot_skips_open() {
        let mut s = Snapshot::default();
        s.record(&Out::Open { number: Some("+331".into()) }, true);
        assert!(replayed(&s).is_empty());
    }

    fn open(number: Option<&str>) -> Out {
        Out::Open { number: number.map(str::to_owned) }
    }

    #[test]
    fn held_open_goes_once_to_the_next_window() {
        let mut w = Windows::default();
        let t0 = Instant::now();
        assert!(!w.deliver(Some("+33600000001")));
        w.hold(Some("+33600000001".into()), t0);
        let (tx, mut rx) = mpsc::channel(8);
        w.add(tx, t0 + Duration::from_secs(1));
        assert_eq!(rx.try_recv().ok(), Some(open(Some("+33600000001"))));
        // Handed once: a second window gets nothing, the subscribed ones get the next `Open`.
        let (tx2, mut rx2) = mpsc::channel(8);
        w.add(tx2, t0 + Duration::from_secs(2));
        assert_eq!(rx2.try_recv().ok(), None);
        assert!(w.deliver(None));
        assert_eq!(rx.try_recv().ok(), Some(open(None)));
        assert_eq!(rx2.try_recv().ok(), Some(open(None)));
    }

    #[test]
    fn held_open_expires_and_closed_windows_leave() {
        let mut w = Windows::default();
        let t0 = Instant::now();
        w.hold(None, t0);
        let (tx, mut rx) = mpsc::channel(8);
        w.add(tx, t0 + Windows::HOLD);
        assert_eq!(rx.try_recv().ok(), None);
        drop(rx);
        assert!(!w.deliver(None));
    }

    #[test]
    fn window_kept_once_and_full_channel_not_counted() {
        let mut w = Windows::default();
        let t0 = Instant::now();
        let (tx, mut rx) = mpsc::channel(1);
        w.add(tx.clone(), t0);
        w.add(tx, t0);
        assert!(w.deliver(None));
        assert_eq!(rx.try_recv().ok(), Some(open(None)));
        assert_eq!(rx.try_recv().ok(), None, "subscribed once");
        assert!(w.deliver(None));
        assert!(!w.deliver(None), "channel full: the caller holds");
    }

    #[test]
    fn snapshot_caps_sms() {
        let mut s = Snapshot::default();
        for i in 0..MAX_SMS + 5 {
            s.record(&sms(&i.to_string(), false), true);
        }
        assert_eq!(s.sms.len(), MAX_SMS);
        assert_eq!(s.keys.len(), MAX_SMS);
        assert!(!s.keys.contains(&(Folder::Inbox, "0".into())));
    }

    async fn next(lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>) -> Option<Out> {
        let l = tokio::time::timeout(Duration::from_secs(2), lines.next_line()).await.ok()?.ok()??;
        Some(serde_json::from_str(&l).unwrap())
    }

    /// Directory like `$XDG_RUNTIME_DIR`: 0700 (the `tempdir` default follows the umask).
    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[tokio::test]
    async fn server_replays_broadcasts_and_limits_lines() {
        let dir = private_dir();
        let path = dir.path().join("t.sock");
        let (req_tx, mut req_rx) = mpsc::channel(8);
        let bound = bind(&path).await.unwrap();
        let hub = spawn_server(bound, req_tx);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // Second daemon refused.
        assert_eq!(bind(&path).await.unwrap_err().kind(), io::ErrorKind::AddrInUse);
        hub.publish(sms("1", true), true);

        let (r, mut w) = UnixStream::connect(&path).await.unwrap().into_split();
        let mut lines = BufReader::new(r).lines();
        assert_eq!(next(&mut lines).await, Some(sms("1", false)));
        hub.publish(Out::Status { link: Profile::Map, up: true }, true);
        assert_eq!(next(&mut lines).await, Some(Out::Status { link: Profile::Map, up: true }));

        w.write_all(b"{\"cmd\":\"answer\"}\n").await.unwrap();
        let (cmd, reply) = req_rx.recv().await.unwrap();
        assert_eq!(cmd, Cmd::Answer);
        reply.unwrap().send(Out::Error { msg: "busy".into() }).await.unwrap();
        assert_eq!(next(&mut lines).await, Some(Out::Error { msg: "busy".into() }));

        w.write_all(b"not json\n").await.unwrap();
        assert!(matches!(next(&mut lines).await, Some(Out::Error { .. })));

        w.write_all(&vec![b'a'; MAX_LINE + 10]).await.unwrap();
        assert!(matches!(next(&mut lines).await, Some(Out::Error { .. })));
        assert_eq!(next(&mut lines).await, None, "connection closed after an over-long line");
    }

    #[tokio::test]
    async fn bind_refuses_open_parent() {
        let dir = private_dir();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(bind(&dir.path().join("t.sock")).await.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn bind_refuses_non_socket() {
        let dir = private_dir();
        let path = dir.path().join("f");
        std::fs::write(&path, b"x").unwrap();
        assert_eq!(bind(&path).await.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    }
}
