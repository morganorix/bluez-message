//! Socket Unix du démon `bluez-messaged` : les [`Out`] partent en JSON lignes vers chaque client (GUI),
//! les [`Cmd`] reviennent en JSON lignes. À la connexion, un client reçoit d'abord un instantané
//! (liens, SMS connus, envois, appel en cours) pour reconstruire son état sans notification.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, broadcast, mpsc};

use crate::engine::{CallKind, Cmd, Item, Out, Profile, parse_cmd};
use crate::hfp::HfEvent;
use crate::history::SENT_ECHO_SECS;
use crate::map::Folder;
use crate::phone::{number_key, sms_peer};
use crate::stamp;
use crate::t;

/// Longueur maximale d'une ligne client (octets) ; au-delà la connexion est fermée.
pub const MAX_LINE: usize = 16 * 1024;
/// Longueur maximale d'une ligne du démon vue par la GUI (le répertoire tient sur une ligne).
const MAX_DAEMON_LINE: usize = 16 * 1024 * 1024;
const MAX_SMS: usize = 1000;
const MAX_SENT: usize = 200;
const MAX_CALL: usize = 64;
/// Historique des appels rejoué aux clients.
const MAX_LOG: usize = 500;
const RETRY: Duration = Duration::from_secs(2);
const MAX_CLIENTS: usize = 16;
/// Un client qui ne lit plus est déconnecté au-delà de ce délai.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const BROADCAST: usize = 4096;

/// Ce qu'un client qui se connecte doit rejouer.
#[derive(Debug, Default)]
pub struct Snapshot {
    status: BTreeMap<Profile, bool>,
    slc: bool,
    sms: VecDeque<Out>,
    keys: HashSet<(Folder, String)>,
    sent: VecDeque<Out>,
    /// Événements de l'appel en cours ; vidé quand l'appel se termine.
    call: Vec<Out>,
    log: VecDeque<Out>,
    contacts: Option<Out>,
    settings: Option<Out>,
    /// Lu jusqu'à, par conversation.
    reads: BTreeMap<String, String>,
}

fn push_capped(q: &mut VecDeque<Out>, o: Out, cap: usize) -> Option<Out> {
    q.push_back(o);
    if q.len() > cap { q.pop_front() } else { None }
}

impl Snapshot {
    /// `call_idle` : aucun appel en cours après application de `o` (machine d'état du démon).
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
                    // Déjà connu : on garde la version la plus récente (statut lu, etc.).
                    let same = |x: &Out| matches!(x, Out::Sms { sms: s, folder: f, .. } if f == folder && s.handle == sms.handle);
                    if let Some(slot) = self.sms.iter_mut().find(|x| same(x)) {
                        *slot = o;
                    }
                } else if let Some(Out::Sms { sms, folder, .. }) = push_capped(&mut self.sms, o, MAX_SMS) {
                    self.keys.remove(&(folder, sms.handle));
                }
            }
            // Échecs compris : la bulle « non envoyé » se retrouve à la réouverture de l'app.
            Out::Sent { .. } => {
                push_capped(&mut self.sent, o.clone(), MAX_SENT);
            }
            Out::Deleted { key, item } => self.forget(key, item.as_ref()),
            Out::CallLog { .. } => {
                push_capped(&mut self.log, o.clone(), MAX_LOG);
            }
            Out::Contacts { .. } => self.contacts = Some(o.clone()),
            Out::Settings { .. } => self.settings = Some(o.clone()),
            Out::Cleared => {
                self.sms.clear();
                self.keys.clear();
                self.sent.clear();
                self.log.clear();
                self.reads.clear();
            }
            Out::Read { key, until } => {
                let r = self.reads.entry(key.clone()).or_default();
                if until > r {
                    r.clone_from(until);
                }
            }
            Out::Muted { .. } if !call_idle => {
                self.call.retain(|c| !matches!(c, Out::Muted { .. }));
                self.call.push(o.clone());
            }
            Out::Call(HfEvent::SlcReady) => self.slc = true,
            Out::Call(_) if call_idle => self.call.clear(),
            Out::Call(_) if self.call.len() < MAX_CALL => self.call.push(o.clone()),
            _ => {}
        }
    }

    /// Retire ce qui a été supprimé de l'historique (élément ou conversation entière).
    fn forget(&mut self, key: &str, item: Option<&Item>) {
        let hit = |o: &Out| match o {
            Out::Sms { sms, folder, .. } => {
                let sent = folder.is_sent();
                number_key(sms_peer(sms, sent).0) == key
                    && item.is_none_or(|i| !i.call && i.outgoing == sent && i.timestamp == sms.timestamp && i.text == sms.text)
            }
            // L'envoi gardé porte l'heure du PC ; l'élément supprimé, souvent celle du téléphone (retour `sent`).
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
            if gone && let Out::Sms { sms, folder, .. } = o {
                keys.remove(&(*folder, sms.handle.clone()));
            }
            !gone
        });
        self.sent.retain(|o| !hit(o));
        self.log.retain(|o| !hit(o));
    }

    #[must_use]
    pub fn replay(&self) -> Vec<Out> {
        let mut v: Vec<Out> = self.status.iter().map(|(link, up)| Out::Status { link: *link, up: *up }).collect();
        if self.slc {
            v.push(Out::Call(HfEvent::SlcReady));
        }
        v.extend(self.settings.iter().cloned());
        v.extend(self.contacts.iter().cloned());
        v.extend(self.sms.iter().cloned());
        v.extend(self.log.iter().cloned());
        v.extend(self.sent.iter().cloned());
        v.extend(self.reads.iter().map(|(key, until)| Out::Read { key: key.clone(), until: until.clone() }));
        v.extend(self.call.iter().cloned());
        v
    }
}

/// Commande d'un client, avec le canal où renvoyer une erreur qui ne concerne que lui.
pub type Request = (Cmd, Option<mpsc::Sender<Out>>);

type Shared = Arc<Mutex<(Snapshot, broadcast::Sender<Out>)>>;

/// Côté démon : publication des événements vers les clients. Détient le verrou d'instance.
pub struct Hub {
    inner: Shared,
    _lock: File,
}

impl Hub {
    /// Instantané et diffusion sous le même verrou : un client qui se connecte ne perd ni ne double rien.
    pub fn publish(&self, o: Out, call_idle: bool) {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        g.0.record(&o, call_idle);
        let _ = g.1.send(o);
    }
}

/// Socket lié et verrou d'instance (`<socket>.lock`, à garder pendant toute la vie du démon).
#[derive(Debug)]
pub struct Bound {
    listener: UnixListener,
    lock: File,
}

/// Verrou exclusif non bloquant : deux démons lancés ensemble ne peuvent pas se voler le socket.
fn lock(path: &Path) -> io::Result<File> {
    let f = File::options().create(true).truncate(false).write(true).open(path)?;
    // SAFETY: descripteur valide détenu par `f` ; flock n'accède à aucune mémoire.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, t!("daemon already running (lock {0})", path.display())));
    }
    Ok(f)
}

/// Le socket n'est sûr que dans un dossier à soi, fermé aux autres : sinon un tiers pourrait s'y connecter
/// avant le `chmod`, ou remplacer socket et verrou.
fn private_parent(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let m = std::fs::metadata(dir)?;
    // SAFETY: geteuid n'a pas de précondition et ne peut pas échouer.
    let me = unsafe { libc::geteuid() };
    if m.uid() != me || m.mode() & 0o077 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, t!("{0} must belong to you and be closed to others (0700)", dir.display())));
    }
    Ok(())
}

/// Prend le verrou, refuse si un démon répond déjà, supprime un socket orphelin, droits 0600.
///
/// # Errors
/// Verrou pris, démon déjà actif, ou socket impossible à créer.
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

/// Accepte les clients en tâche de fond ; leurs commandes arrivent dans `requests`.
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
                    // Au-delà de MAX_CLIENTS, la connexion est refermée aussitôt.
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
    // Le lecteur détient le seul émetteur de réponses : sa fin (EOF, ligne trop longue) clôt l'écriture.
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
                // Client trop lent : il se reconnecte et repart de l'instantané.
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

/// Arrête la lecture des commandes quand l'écriture vers le client s'arrête.
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
            // Un client ne peut pas arrêter le démon : `quit` ferme seulement sa connexion.
            Ok(Cmd::Quit) => return,
            Ok(cmd) => {
                if requests.send((cmd, Some(reply.clone()))).await.is_err() {
                    return;
                }
            }
            Err(msg) => {
                let _ = reply.send(Out::Error { msg }).await;
            }
        }
    }
}

/// Côté GUI : état de la liaison avec le démon.
#[derive(Debug)]
pub enum Link {
    /// Connecté : l'instantané suit, l'état local est à réinitialiser.
    Up,
    Down(String),
    Event(Box<Out>),
}

/// Se connecte au démon et y reste (nouvel essai toutes les 2 s) jusqu'à la fermeture de `cmds`.
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

/// `Ok` : `cmds` fermé ; `Err` : liaison perdue.
async fn session(s: UnixStream, cmds: &mut mpsc::UnboundedReceiver<Cmd>, on: &mut impl FnMut(Link)) -> Result<(), String> {
    let (r, mut w) = s.into_split();
    let mut r = BufReader::new(r);
    // Conservé d'un tour à l'autre : une lecture interrompue par `select!` garde ses octets dans `buf`.
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

    #[test]
    fn snapshot_replays_state_without_new_flags() {
        let mut s = Snapshot::default();
        s.record(&Out::Status { link: Profile::Hfp, up: true }, true);
        s.record(&Out::Call(HfEvent::SlcReady), true);
        s.record(&sms("1", true), true);
        s.record(&sms("1", true), true); // doublon : remplace sans dupliquer
        s.record(&Out::Call(HfEvent::IncomingCall), false);
        s.record(&Out::Error { msg: "x".into() }, false);
        let r = s.replay();
        assert_eq!(
            r,
            [Out::Status { link: Profile::Hfp, up: true }, Out::Call(HfEvent::SlcReady), sms("1", false), Out::Call(HfEvent::IncomingCall)]
        );
        // Fin d'appel : plus rien de l'appel n'est rejoué.
        s.record(&ind("callsetup", 0), true);
        assert_eq!(s.replay().len(), 3);
        // Perte du lien HFP : SLC oublié.
        s.record(&Out::Status { link: Profile::Hfp, up: false }, true);
        assert!(!s.replay().contains(&Out::Call(HfEvent::SlcReady)));
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
        assert_eq!(s.replay().len(), 4, "échec d'envoi rejoué");
        let item = Item { timestamp: "20261015T100000".into(), text: "a".into(), outgoing: false, call: false };
        s.record(&Out::Deleted { key: "612345678".into(), item: Some(item) }, true);
        assert_eq!(s.replay().len(), 3);
        assert!(!s.keys.contains(&(Folder::Inbox, "1".into())));
        s.record(&Out::Deleted { key: "612345678".into(), item: None }, true);
        assert!(s.replay().is_empty());
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

    /// Dossier comme `$XDG_RUNTIME_DIR` : 0700 (le défaut de `tempdir` suit l'umask).
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
        // Second démon refusé.
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
        reply.unwrap().send(Out::Error { msg: "occupé".into() }).await.unwrap();
        assert_eq!(next(&mut lines).await, Some(Out::Error { msg: "occupé".into() }));

        w.write_all(b"pas du json\n").await.unwrap();
        assert!(matches!(next(&mut lines).await, Some(Out::Error { .. })));

        w.write_all(&vec![b'a'; MAX_LINE + 10]).await.unwrap();
        assert!(matches!(next(&mut lines).await, Some(Out::Error { .. })));
        assert_eq!(next(&mut lines).await, None, "connexion fermée après une ligne trop longue");
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
