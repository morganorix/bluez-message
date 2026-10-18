//! Local history (SQLite) kept by the daemon: SMS and calls received since a device was first used,
//! iPhone contacts. Older SMS left on the phone are not imported. Every row belongs to one device:
//! a [`History`] only sees the rows of its own.

use std::collections::HashMap;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};

use crate::addr::BtAddr;
use crate::engine::{CallKind, ConvFlags, Cursor, HITS_PAGE, Hit, Out};
use crate::error::Error;
use crate::map::{Folder, Sms};
use crate::numbering::{self, Id};
use crate::phone::{fold, sms_peer};
use crate::stamp;

/// Single-device schema (v1 to v3): starting point of [`MIGRATIONS`].
const SCHEMA_V3: &str = "
CREATE TABLE IF NOT EXISTS meta (k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY, key TEXT NOT NULL, number TEXT NOT NULL, ts TEXT NOT NULL,
    text TEXT NOT NULL, outgoing INTEGER NOT NULL, local INTEGER NOT NULL DEFAULT 0,
    hidden INTEGER NOT NULL DEFAULT 0, UNIQUE (key, ts, outgoing, text));
CREATE INDEX IF NOT EXISTS messages_ts ON messages (ts);
CREATE TABLE IF NOT EXISTS calls (
    id INTEGER PRIMARY KEY, key TEXT NOT NULL, number TEXT NOT NULL, ts TEXT NOT NULL,
    kind TEXT NOT NULL, secs INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS calls_ts ON calls (ts);
CREATE TABLE IF NOT EXISTS contacts (key TEXT PRIMARY KEY, number TEXT NOT NULL, name TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS reads (key TEXT PRIMARY KEY, until TEXT NOT NULL);
";

/// v4 tables, created next to the v3 ones then renamed (SQLite cannot change a constraint).
/// The unique constraints also index the lookups by device and correspondent.
const SCHEMA_V4: &str = "
CREATE TABLE messages_new (
    id INTEGER PRIMARY KEY, device TEXT NOT NULL, key TEXT NOT NULL, number TEXT NOT NULL, ts TEXT NOT NULL,
    text TEXT NOT NULL, outgoing INTEGER NOT NULL, local INTEGER NOT NULL DEFAULT 0,
    hidden INTEGER NOT NULL DEFAULT 0, UNIQUE (device, key, ts, outgoing, text));
CREATE TABLE calls_new (
    id INTEGER PRIMARY KEY, device TEXT NOT NULL, key TEXT NOT NULL, number TEXT NOT NULL, ts TEXT NOT NULL,
    kind TEXT NOT NULL, secs INTEGER NOT NULL, UNIQUE (device, key, ts, kind));
CREATE TABLE contacts_new (
    device TEXT NOT NULL, key TEXT NOT NULL, number TEXT NOT NULL, name TEXT NOT NULL, PRIMARY KEY (device, key));
CREATE TABLE reads_new (device TEXT NOT NULL, key TEXT NOT NULL, until TEXT NOT NULL, PRIMARY KEY (device, key));
";

/// Step `i` brings the database from `user_version` `i` to `i + 1`, in its own transaction.
type Migration = fn(&Transaction<'_>, &str) -> rusqlite::Result<()>;
const MIGRATIONS: [Migration; 5] = [v1_base, v2_reads, v3_hidden, v4_device, v5_convs];
/// Index in [`MIGRATIONS`] of the step to v4, preceded by a copy to [`V3_BAK`].
const TO_V4: usize = 3;
/// Index in [`MIGRATIONS`] of the step to v5, preceded by a copy to [`V4_BAK`].
const TO_V5: usize = 4;

/// Copy made before the first [`History::rekey`].
const KEYS_BAK: &str = ".bak";
/// Copy made before the migration to v4 (one row per device).
const V3_BAK: &str = ".v3.bak";
/// Copy made before the migration to v5 (searchable text, conversation flags).
const V4_BAK: &str = ".v4.bak";

/// Timestamp gap below which an identical SMS (same correspondent, direction and text) is the same one.
const SAME_SMS_SECS: i64 = 2;
/// A send from the app that already came back through the `sent` folder (phone clock) within this gap is not recorded again.
pub const SENT_ECHO_SECS: i64 = 30;
/// Format of the grouping keys (see [`numbering::key`]), stored with the country in `meta 'key_home'`.
const KEY_FORMAT: &str = "v1";
/// Longest wait of a [`Reader`] for a write in progress.
const READ_BUSY: std::time::Duration = std::time::Duration::from_secs(5);

pub struct History {
    db: Connection,
    /// Active device (canonical address): every query is restricted to it.
    device: String,
    since: String,
    /// Country numbers are read with (grouping keys), see [`History::rekey`].
    home: Option<Id>,
    /// Database file (copies before migrations); `None` in memory.
    path: Option<PathBuf>,
    /// What [`History::reader`] opens: the file, or the URI of the shared in-memory database.
    source: String,
}

impl History {
    /// Opens (or creates, directory 0700 and file 0600) the database, migrated to the current schema,
    /// for the history of `device`.
    ///
    /// # Errors
    /// Directory or database cannot be created or restricted, copy or migration failed (rolled back),
    /// or database written by a newer version.
    pub fn open(path: &Path, device: &BtAddr) -> crate::error::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        // Created as 0600 before SQLite opens it (no umask window); a symbolic link is refused.
        std::fs::OpenOptions::new().append(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Self::init(Connection::open(path)?, device, Some(path.to_owned()))
    }

    /// In-memory database (fallback, tests).
    ///
    /// # Errors
    /// Schema rejected.
    pub fn memory(device: &BtAddr) -> crate::error::Result<Self> {
        // Named and shared: [`History::reader`] opens a second connection on it.
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let uri = format!("file:bluez-message-{}-{}?mode=memory&cache=shared", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_URI;
        let mut h = Self::init(Connection::open_with_flags(&uri, flags)?, device, None)?;
        h.source = uri;
        Ok(h)
    }

    fn init(mut db: Connection, device: &BtAddr, path: Option<PathBuf>) -> crate::error::Result<Self> {
        // Deleted rows are overwritten with zeros on disk (and in the journal).
        db.pragma_update(None, "secure_delete", true)?;
        // WAL: the slow reads of a [`Reader`] never block a write (stays "memory" for an in-memory database).
        db.pragma_update_and_check(None, "journal_mode", "wal", |r| r.get::<_, String>(0))?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let start = usize::try_from(version).ok().filter(|&v| v <= MIGRATIONS.len()).ok_or(Error::DbVersion(version))?;
        let device = device.to_string();
        for (v, step) in MIGRATIONS.iter().enumerate().skip(start) {
            match v {
                TO_V4 => backup(&db, path.as_deref(), V3_BAK)?,
                TO_V5 => backup(&db, path.as_deref(), V4_BAK)?,
                _ => {}
            }
            let tx = db.transaction()?;
            step(&tx, &device)?;
            tx.pragma_update(None, "user_version", v + 1)?;
            tx.commit()?;
        }
        let key = since_key(&device);
        db.execute("INSERT OR IGNORE INTO meta (k, v) VALUES (?1, ?2)", params![key, stamp::now()])?;
        let since = db.query_row("SELECT v FROM meta WHERE k = ?1", [&key], |r| r.get(0))?;
        let source = path.as_deref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        Ok(Self { db, device, since, home: None, path, source })
    }

    /// Reads the numbers with country `home` from now on, and recomputes the stored grouping keys of
    /// every device if they were made otherwise (older format, other country): conversations of the
    /// same number merge, read marks follow (the latest wins). The first time, the database is copied
    /// to `<file>.bak`. Returns `true` if the keys changed.
    ///
    /// # Errors
    /// SQLite error or copy failure (nothing is changed).
    pub fn rekey(&mut self, home: Option<Id>) -> crate::error::Result<bool> {
        let tag = format!("{KEY_FORMAT}:{}", home.map(numbering::code).unwrap_or_default());
        let done: Option<String> = self.db.query_row("SELECT v FROM meta WHERE k = 'key_home'", [], |r| r.get(0)).optional()?;
        if done.as_deref() == Some(tag.as_str()) {
            self.home = home;
            return Ok(false);
        }
        if done.is_none() {
            backup(&self.db, self.path.as_deref(), KEYS_BAK)?;
        }
        let tx = self.db.transaction()?;
        // (device, old key) → new keys (a key made of the last 9 digits may split).
        let mut moved: HashMap<(String, String), Vec<String>> = HashMap::new();
        for table in ["messages", "calls", "contacts"] {
            let rows: Vec<(i64, String, String, String)> = tx
                .prepare(&format!("SELECT rowid, device, key, number FROM {table} ORDER BY rowid"))?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let changed: Vec<(i64, String)> = rows
                .into_iter()
                .filter_map(|(id, device, old, number)| {
                    let new = numbering::key(&number, home);
                    let seen = moved.entry((device, old.clone())).or_default();
                    if !seen.contains(&new) {
                        seen.push(new.clone());
                    }
                    (new != old).then_some((id, new))
                })
                .collect();
            // Two passes through a key unique to each row: a row moved later cannot collide with one
            // moved earlier. A row then equal to a kept one (same SMS, same contact) is a duplicate.
            let mut tmp = tx.prepare(&format!("UPDATE {table} SET key = '~' || rowid WHERE rowid = ?1"))?;
            for (id, _) in &changed {
                tmp.execute([id])?;
            }
            let mut set = tx.prepare(&format!("UPDATE OR IGNORE {table} SET key = ?2 WHERE rowid = ?1"))?;
            let mut drop = tx.prepare(&format!("DELETE FROM {table} WHERE rowid = ?1"))?;
            for (id, new) in &changed {
                if set.execute(params![id, new])? == 0 {
                    if table == "messages" {
                        // The kept SMS takes over the duplicate's state: deleted if either was, confirmed if either was.
                        tx.execute(
                            "UPDATE messages SET hidden = max(messages.hidden, d.hidden), local = min(messages.local, d.local) \
                             FROM (SELECT device, ts, outgoing, text, hidden, local FROM messages WHERE rowid = ?1) AS d \
                             WHERE messages.device = d.device AND key = ?2 AND messages.ts = d.ts \
                             AND messages.outgoing = d.outgoing AND messages.text = d.text",
                            params![id, new],
                        )?;
                    }
                    drop.execute([id])?;
                }
            }
        }
        let reads: Vec<(String, String, String)> =
            tx.prepare("SELECT device, key, until FROM reads")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
        tx.execute("DELETE FROM reads", [])?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO reads (device, key, until) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (device, key) DO UPDATE SET until = max(until, excluded.until)",
            )?;
            // A mark whose conversation no longer exists is dropped.
            for (device, old, until) in reads {
                for new in moved.get(&(device.clone(), old)).into_iter().flatten() {
                    ins.execute(params![device, new, until])?;
                }
            }
        }
        let convs: Vec<(String, String, bool, bool)> = tx
            .prepare("SELECT device, key, muted, archived FROM convs")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        tx.execute("DELETE FROM convs", [])?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO convs (device, key, muted, archived) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (device, key) DO UPDATE SET muted = max(muted, excluded.muted), archived = min(archived, excluded.archived)",
            )?;
            // Merged conversations: muted if either was, archived only if both were.
            for (device, old, muted, archived) in convs {
                for new in moved.get(&(device.clone(), old)).into_iter().flatten() {
                    ins.execute(params![device, new, muted, archived])?;
                }
            }
        }
        tx.execute("INSERT INTO meta (k, v) VALUES ('key_home', ?1) ON CONFLICT (k) DO UPDATE SET v = excluded.v", [&tag])?;
        tx.commit()?;
        self.home = home;
        Ok(true)
    }

    /// History start of the active device (MAP format): earlier SMS are ignored.
    pub fn since(&self) -> &str {
        &self.since
    }

    /// `true` if the SMS is new to the history (to be published). A sent one coming back from the
    /// `sent` folder replaces the locally recorded send (phone timestamp) without being republished.
    ///
    /// # Errors
    /// SQLite error.
    pub fn add_sms(&self, sms: &Sms, sent: bool) -> rusqlite::Result<bool> {
        let (number, _) = sms_peer(sms, sent);
        if sent {
            let n = self
                .db
                .prepare_cached(
                    "UPDATE OR IGNORE messages SET ts = ?4, local = 0 WHERE id = \
                     (SELECT id FROM messages WHERE device = ?1 AND key = ?2 AND text = ?3 AND outgoing = 1 AND local = 1 ORDER BY ts LIMIT 1)",
                )?
                .execute(params![self.device, numbering::key(number, self.home), sms.text, sms.timestamp])?;
            if n == 1 {
                return Ok(false);
            }
        }
        // iOS may shift the timestamp of the same SMS by one second from one listing to the next.
        if self.near_duplicate(number, &sms.timestamp, &sms.text, sent)? {
            return Ok(false);
        }
        self.insert_msg(number, &sms.timestamp, &sms.text, sent, false)
    }

    /// Hidden ones included: a deleted SMS counts as already recorded, even when purged (empty text).
    fn near_duplicate(&self, number: &str, ts: &str, text: &str, outgoing: bool) -> rusqlite::Result<bool> {
        Ok(self.near(number, ts, text, outgoing, SAME_SMS_SECS, "")? || self.near(number, ts, "", outgoing, SAME_SMS_SECS, " AND hidden = 1")?)
    }

    /// An SMS with the same correspondent, direction and text within `secs`; `extra`: additional SQL condition.
    fn near(&self, number: &str, ts: &str, text: &str, outgoing: bool, secs: i64, extra: &'static str) -> rusqlite::Result<bool> {
        let Some(t) = stamp::secs(ts) else { return Ok(false) };
        let mut st = self.db.prepare_cached(&format!("SELECT ts FROM messages WHERE device = ?1 AND key = ?2 AND text = ?3 AND outgoing = ?4{extra}"))?;
        // A row error is propagated, not read as "no duplicate" (it would record the SMS twice).
        for ts in st.query_map(params![self.device, numbering::key(number, self.home), text, outgoing], |r| r.get::<_, String>(0))? {
            if stamp::secs(&ts?).is_some_and(|u| (u - t).abs() <= secs) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Send made from the app (PC timestamp), awaiting its return through the `sent` folder.
    /// Nothing is recorded if that return has already arrived (it sometimes precedes the end of the transfer).
    ///
    /// # Errors
    /// SQLite error.
    pub fn add_local_sent(&self, number: &str, ts: &str, text: &str) -> rusqlite::Result<bool> {
        if self.near(number, ts, text, true, SENT_ECHO_SECS, " AND local = 0 AND hidden = 0")? {
            return Ok(false);
        }
        self.insert_msg(number, ts, text, true, true)
    }

    /// Hides an SMS (`Some((ts, text, outgoing))`) or a conversation's whole history (its read mark
    /// is erased too); a call (`call`) is erased. Returns the number of rows touched.
    ///
    /// # Errors
    /// SQLite error.
    pub fn delete(&self, key: &str, item: Option<&crate::engine::Item>) -> rusqlite::Result<usize> {
        let d = &self.device;
        let n = match item {
            None => {
                let tx = self.db.unchecked_transaction()?;
                let n = tx.execute("UPDATE messages SET hidden = 1 WHERE device = ?1 AND key = ?2", [d, key])?
                    + tx.execute("DELETE FROM calls WHERE device = ?1 AND key = ?2", [d, key])?;
                tx.execute("DELETE FROM reads WHERE device = ?1 AND key = ?2", [d, key])?;
                tx.execute("DELETE FROM convs WHERE device = ?1 AND key = ?2", [d, key])?;
                tx.commit()?;
                n
            }
            Some(i) if i.call => self.db.execute(
                "DELETE FROM calls WHERE id = (SELECT id FROM calls WHERE device = ?1 AND key = ?2 AND ts = ?3 AND (kind = 'out') = ?4 LIMIT 1)",
                params![d, key, i.timestamp, i.outgoing],
            )?,
            Some(i) => self.db.execute(
                "UPDATE messages SET hidden = 1 WHERE device = ?1 AND key = ?2 AND ts = ?3 AND text = ?4 AND outgoing = ?5",
                params![d, key, i.timestamp, i.text, i.outgoing],
            )?,
        };
        Ok(n)
    }

    /// Erases the SMS and calls of every device older than `before` (MAP timestamp; retention is a
    /// global setting); each device's `since` advances to that point so that SMS left on the iPhone
    /// do not come back at the next listing. Returns the number of rows.
    ///
    /// # Errors
    /// SQLite error.
    pub fn purge_before(&mut self, before: &str) -> rusqlite::Result<usize> {
        let tx = self.db.transaction()?;
        let n = tx.execute("DELETE FROM messages WHERE ts < ?1", [before])? + tx.execute("DELETE FROM calls WHERE ts < ?1", [before])?;
        tx.execute("UPDATE meta SET v = max(v, ?1) WHERE k LIKE 'since:%'", [before])?;
        let since: String = tx.query_row("SELECT v FROM meta WHERE k = ?1", [since_key(&self.device)], |r| r.get(0))?;
        tx.commit()?;
        self.since = since;
        Ok(n)
    }

    /// Erases the text of hidden SMS (deleted from the app), for every device. The row stays, empty,
    /// as a marker: an SMS still on the iPhone does not come back (`near_duplicate`); older than its
    /// device's `since`, it disappears.
    ///
    /// # Errors
    /// SQLite error.
    pub fn purge_hidden(&self) -> rusqlite::Result<usize> {
        let tx = self.db.unchecked_transaction()?;
        let n = tx.execute("UPDATE OR REPLACE messages SET text = '', folded = '' WHERE hidden = 1 AND text <> ''", [])?;
        tx.execute("DELETE FROM messages WHERE hidden = 1 AND ts < (SELECT v FROM meta WHERE k = 'since:' || messages.device)", [])?;
        tx.commit()?;
        Ok(n)
    }

    /// Clears the active device's history (SMS, calls, read marks) and the `.bak` copies, which hold
    /// it too; contacts are kept. `since` moves to now: SMS left on the iPhone do not come back. Once
    /// no device has any history left, the file is compacted.
    ///
    /// # Errors
    /// SQLite error.
    pub fn clear(&mut self) -> rusqlite::Result<()> {
        let now = stamp::now();
        let d = &self.device;
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM messages WHERE device = ?1", [d])?;
        tx.execute("DELETE FROM calls WHERE device = ?1", [d])?;
        tx.execute("DELETE FROM reads WHERE device = ?1", [d])?;
        tx.execute("DELETE FROM convs WHERE device = ?1", [d])?;
        tx.execute("UPDATE meta SET v = ?2 WHERE k = ?1", [&since_key(d), &now])?;
        tx.commit()?;
        self.since = now;
        for suffix in [KEYS_BAK, V3_BAK, V4_BAK] {
            if let Some(bak) = backup_path(self.path.as_deref(), suffix)
                && let Err(e) = std::fs::remove_file(&bak)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("history backup not removed: {e}");
            }
        }
        if is_empty(&self.db)? {
            self.db.execute_batch("VACUUM")?;
        }
        // Old pages left in the WAL are written back and the file emptied.
        self.db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    fn insert_msg(&self, number: &str, ts: &str, text: &str, outgoing: bool, local: bool) -> rusqlite::Result<bool> {
        let n = self
            .db
            .prepare_cached(
                "INSERT OR IGNORE INTO messages (device, key, number, ts, text, outgoing, local, folded) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![self.device, numbering::key(number, self.home), number, ts, text, outgoing, local, fold(text)])?;
        Ok(n == 1)
    }

    /// Records a call (`secs`: conversation duration); the same call recorded twice is kept once.
    ///
    /// # Errors
    /// SQLite error.
    pub fn add_call(&self, number: &str, ts: &str, kind: CallKind, secs: u64) -> rusqlite::Result<()> {
        self.db
            .prepare_cached("INSERT OR IGNORE INTO calls (device, key, number, ts, kind, secs) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
            .execute(params![self.device, numbering::key(number, self.home), number, ts, kind.as_str(), i64::try_from(secs).unwrap_or(i64::MAX)])?;
        Ok(())
    }

    /// Conversation read up to `until` (never goes backwards).
    ///
    /// # Errors
    /// SQLite error.
    pub fn mark_read(&self, key: &str, until: &str) -> rusqlite::Result<()> {
        self.db
            .prepare_cached(
                "INSERT INTO reads (device, key, until) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (device, key) DO UPDATE SET until = max(until, excluded.until)",
            )?
            .execute(params![self.device, key, until])?;
        Ok(())
    }

    /// Flags of conversation `key`; both off: the row is removed (the table only holds exceptions).
    ///
    /// # Errors
    /// SQLite error.
    pub fn set_conv(&self, key: &str, flags: ConvFlags) -> rusqlite::Result<()> {
        if flags == ConvFlags::default() {
            self.db.prepare_cached("DELETE FROM convs WHERE device = ?1 AND key = ?2")?.execute([&self.device, key])?;
        } else {
            self.db
                .prepare_cached(
                    "INSERT INTO convs (device, key, muted, archived) VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT (device, key) DO UPDATE SET muted = excluded.muted, archived = excluded.archived",
                )?
                .execute(params![self.device, key, flags.muted, flags.archived])?;
        }
        Ok(())
    }

    /// Replaces the active device's contacts; returns the (number, name) pairs published to the clients.
    ///
    /// # Errors
    /// SQLite error (the transaction is rolled back).
    pub fn replace_contacts(&mut self, contacts: &[(String, String)]) -> rusqlite::Result<Vec<(String, String)>> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM contacts WHERE device = ?1", [&self.device])?;
        {
            let mut ins = tx.prepare("INSERT OR IGNORE INTO contacts (device, key, number, name) VALUES (?1, ?2, ?3, ?4)")?;
            for (number, name) in contacts {
                ins.execute(params![self.device, numbering::key(number, self.home), number, name])?;
            }
        }
        tx.commit()?;
        self.contacts()
    }

    fn contacts(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let mut st = self.db.prepare_cached("SELECT number, name FROM contacts WHERE device = ?1")?;
        st.query_map([&self.device], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
    }

    /// What the daemon replays at startup: contacts, the last `limit` SMS and calls, then the read
    /// marks, read in one transaction (consistent snapshot).
    ///
    /// # Errors
    /// SQLite error.
    pub fn load(&self, limit: usize) -> rusqlite::Result<Vec<Out>> {
        let tx = self.db.unchecked_transaction()?;
        let v = self.snapshot(&tx, limit)?;
        tx.commit()?;
        Ok(v)
    }

    fn snapshot(&self, tx: &Transaction<'_>, limit: usize) -> rusqlite::Result<Vec<Out>> {
        let mut v = vec![];
        let names = self.contacts()?;
        if !names.is_empty() {
            v.push(Out::Contacts { names });
        }
        let d = &self.device;
        let lim = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut st = tx.prepare(
            "SELECT id, number, ts, text, outgoing FROM \
             (SELECT * FROM messages WHERE device = ?1 AND hidden = 0 ORDER BY ts DESC, id DESC LIMIT ?2) ORDER BY ts, id",
        )?;
        let msgs = st.query_map(params![d, lim], |r| {
            let (id, number, ts, text, out): (i64, String, String, String, bool) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
            // Unread by default: the `reads` table (replayed afterwards) sets what has been read.
            let mut sms = Sms { handle: format!("db{id}"), timestamp: ts, text, read: out, ..Default::default() };
            if out {
                sms.recipient_address = number;
            } else {
                sms.address = number;
            }
            Ok(Out::Sms { sms, folder: if out { Folder::Sent } else { Folder::Inbox }, new: false })
        })?;
        for m in msgs {
            v.push(m?);
        }
        let mut st = tx.prepare(
            "SELECT number, ts, kind, secs FROM (SELECT * FROM calls WHERE device = ?1 ORDER BY ts DESC, id DESC LIMIT ?2) ORDER BY ts, id",
        )?;
        let calls = st.query_map(params![d, lim], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))?;
        for c in calls {
            let (number, timestamp, kind, secs) = c?;
            if let Some(kind) = CallKind::parse(&kind) {
                v.push(Out::CallLog { number, timestamp, kind, secs: u64::try_from(secs).unwrap_or(0), read: false });
            }
        }
        let mut st = tx.prepare("SELECT key, until FROM reads WHERE device = ?1")?;
        for r in st.query_map([d], |r| Ok(Out::Read { key: r.get(0)?, until: r.get(1)? }))? {
            v.push(r?);
        }
        let mut st = tx.prepare("SELECT key, muted, archived FROM convs WHERE device = ?1")?;
        for r in st.query_map([d], |r| Ok(Out::Conv { key: r.get(0)?, flags: ConvFlags { muted: r.get(1)?, archived: r.get(2)? } }))? {
            v.push(r?);
        }
        Ok(v)
    }

    /// Second, read-only connection on the same database, for the slow reads (search, pages, export)
    /// done away from the writer. Waits up to [`READ_BUSY`] for a write in progress.
    ///
    /// # Errors
    /// Database cannot be opened.
    pub fn reader(&self) -> crate::error::Result<Reader> {
        let mut flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if self.path.is_none() {
            flags |= OpenFlags::SQLITE_OPEN_URI;
        }
        let db = Connection::open_with_flags(&self.source, flags)?;
        db.busy_timeout(READ_BUSY)?;
        Ok(Reader { db, device: self.device.clone() })
    }

    #[cfg(test)]
    fn set_since(&mut self, s: &str) {
        self.db.execute("UPDATE meta SET v = ?2 WHERE k = ?1", [&since_key(&self.device), s]).unwrap();
        self.since = s.to_owned();
    }
}

/// Read-only view of the history of the active device (see [`History::reader`]).
pub struct Reader {
    db: Connection,
    device: String,
}

impl Reader {
    /// SMS of the active device whose text contains `query` (accents and case ignored, see [`fold`]),
    /// newest first, at most [`HITS_PAGE`] strictly before `before`; deleted ones left out.
    /// Returns the page and the cursor of the following one, if any.
    ///
    /// # Errors
    /// SQLite error.
    pub fn search(&self, query: &str, before: Option<&Cursor>) -> rusqlite::Result<(Vec<Hit>, Option<Cursor>)> {
        let folded = fold(crate::phone::clean(query, false).trim());
        // Only combining marks: an empty pattern would match every message.
        if folded.is_empty() {
            return Ok((vec![], None));
        }
        let pattern = format!("%{}%", like_escape(&folded));
        let top = Cursor::after("99991231T235959");
        let c = before.unwrap_or(&top);
        let st = self.db.prepare_cached(
            "SELECT id, key, number, ts, text, outgoing FROM messages WHERE device = ?1 AND hidden = 0 \
             AND folded LIKE ?2 ESCAPE '\\' AND (ts < ?3 OR (ts = ?3 AND id < ?4)) ORDER BY ts DESC, id DESC LIMIT ?5",
        )?;
        page(st, params![self.device, pattern, c.ts, c.id, page_limit(HITS_PAGE)], HITS_PAGE)
    }

    /// SMS of conversation `key` strictly before `before`, newest first, at most `limit`; deleted ones left out.
    /// Returns the page and the cursor of the following one, if any.
    ///
    /// # Errors
    /// SQLite error.
    pub fn older(&self, key: &str, before: &Cursor, limit: usize) -> rusqlite::Result<(Vec<Hit>, Option<Cursor>)> {
        let st = self.db.prepare_cached(
            "SELECT id, key, number, ts, text, outgoing FROM messages WHERE device = ?1 AND key = ?2 AND hidden = 0 \
             AND (ts < ?3 OR (ts = ?3 AND id < ?4)) ORDER BY ts DESC, id DESC LIMIT ?5",
        )?;
        page(st, params![self.device, key, before.ts, before.id, page_limit(limit)], limit)
    }

    /// Writes the history of the active device as one JSON object (format 1): its contacts, SMS
    /// (deleted ones left out) and calls, oldest first, restricted to conversation `key` if given,
    /// headed by the `device` name if known. Read in one transaction (consistent snapshot), streamed
    /// row by row; no device address.
    ///
    /// # Errors
    /// SQLite or write error.
    pub fn export(&self, key: Option<&str>, device: Option<&str>, w: &mut impl std::io::Write) -> crate::error::Result<()> {
        let tx = self.db.unchecked_transaction()?;
        let p = params![self.device, key];
        let mut head = serde_json::json!({ "format": 1, "exported": stamp::now(), "conversation": key });
        if let Some(d) = device {
            head["device"] = d.into();
        }
        let head = serde_json::to_string(&head).map_err(std::io::Error::from)?;
        // `{...}` without its closing brace, the three arrays follow.
        write!(w, "{},\"contacts\":", &head[..head.len() - 1])?;
        json_rows(w, &tx, "SELECT number, name FROM contacts WHERE device = ?1 AND (?2 IS NULL OR key = ?2) ORDER BY name, number", p, |r| {
            Ok(serde_json::json!({ "number": r.get::<_, String>(0)?, "name": r.get::<_, String>(1)? }))
        })?;
        w.write_all(b",\"messages\":")?;
        json_rows(
            w,
            &tx,
            "SELECT ts, number, outgoing, text FROM messages WHERE device = ?1 AND (?2 IS NULL OR key = ?2) AND hidden = 0 ORDER BY ts, id",
            p,
            |r| {
                let direction = if r.get::<_, bool>(2)? { "out" } else { "in" };
                Ok(serde_json::json!({ "timestamp": r.get::<_, String>(0)?, "number": r.get::<_, String>(1)?, "direction": direction, "text": r.get::<_, String>(3)? }))
            },
        )?;
        w.write_all(b",\"calls\":")?;
        json_rows(w, &tx, "SELECT ts, number, kind, secs FROM calls WHERE device = ?1 AND (?2 IS NULL OR key = ?2) ORDER BY ts, id", p, |r| {
            Ok(serde_json::json!({ "timestamp": r.get::<_, String>(0)?, "number": r.get::<_, String>(1)?, "kind": r.get::<_, String>(2)?, "secs": r.get::<_, i64>(3)? }))
        })?;
        w.write_all(b"}\n")?;
        tx.commit()?;
        Ok(())
    }

    /// [`Reader::export`] to a new file of `dir`, `bluez-message-<date>[-<key>].json` (mode 0600),
    /// written beside it then renamed: never a partial file, never an existing one overwritten.
    ///
    /// # Errors
    /// SQLite or file error, or the file already exists.
    pub fn export_to(&self, dir: &Path, key: Option<&str>, device: Option<&str>) -> crate::error::Result<PathBuf> {
        self.export_at(dir, key, device, &stamp::now())
    }

    /// [`Reader::export_to`] stamped `ts`: a name already taken gets `-2` to `-9`, then the export is refused.
    fn export_at(&self, dir: &Path, key: Option<&str>, device: Option<&str>, ts: &str) -> crate::error::Result<PathBuf> {
        let mut name = format!("bluez-message-{}", ts.replace('T', "-"));
        if let Some(k) = key {
            name.push('-');
            name.extend(k.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '+').take(32));
        }
        // `tempfile` creates it 0600; its errors name the path, kept out of the logs.
        let tmp = tempfile::Builder::new().prefix(".bluez-message-").suffix(".part").tempfile_in(dir).map_err(|e| std::io::Error::from(e.kind()))?;
        let mut w = std::io::BufWriter::new(tmp);
        self.export(key, device, &mut w)?;
        let mut tmp = w.into_inner().map_err(std::io::IntoInnerError::into_error)?;
        tmp.as_file().sync_all()?;
        for n in 1..=9 {
            let path = dir.join(if n == 1 { format!("{name}.json") } else { format!("{name}-{n}.json") });
            match tmp.persist_noclobber(&path) {
                Ok(_) => return Ok(path),
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => tmp = e.file,
                Err(e) => return Err(std::io::Error::from(e.error.kind()).into()),
            }
        }
        Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists).into())
    }
}

/// JSON array of the rows of `sql`, one `item` each, written as they are read.
fn json_rows(
    w: &mut impl std::io::Write,
    tx: &Transaction<'_>,
    sql: &str,
    p: impl rusqlite::Params,
    item: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value>,
) -> crate::error::Result<()> {
    let mut st = tx.prepare(sql)?;
    let mut rows = st.query(p)?;
    w.write_all(b"[")?;
    let mut first = true;
    while let Some(r) = rows.next()? {
        if !std::mem::take(&mut first) {
            w.write_all(b",")?;
        }
        serde_json::to_writer(&mut *w, &item(r)?).map_err(std::io::Error::from)?;
    }
    w.write_all(b"]")?;
    Ok(())
}

/// `LIMIT` of a page of `n`: one more row tells whether a following page exists.
fn page_limit(n: usize) -> i64 {
    i64::try_from(n).map_or(i64::MAX, |n| n.saturating_add(1))
}

/// Rows `id, key, number, ts, text, outgoing` of `st`: the first `n`, and the cursor after them if more came.
fn page(mut st: rusqlite::CachedStatement<'_>, p: impl rusqlite::Params, n: usize) -> rusqlite::Result<(Vec<Hit>, Option<Cursor>)> {
    let mut items = st
        .query_map(p, |r| Ok(Hit { id: r.get(0)?, key: r.get(1)?, number: r.get(2)?, timestamp: r.get(3)?, text: r.get(4)?, outgoing: r.get(5)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = items.len() > n;
    items.truncate(n);
    let next = more.then(|| items.last().map(Hit::cursor)).flatten();
    Ok((items, next))
}

/// `s` taken literally by `LIKE … ESCAPE '\'`.
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `meta` key of a device's history start.
fn since_key(device: &str) -> String {
    format!("since:{device}")
}

/// Neither SMS nor calls, whatever the device.
fn is_empty(db: &Connection) -> rusqlite::Result<bool> {
    db.query_row("SELECT NOT EXISTS (SELECT 1 FROM messages) AND NOT EXISTS (SELECT 1 FROM calls)", [], |r| r.get(0))
}

/// `<file><suffix>`; `None` in memory.
fn backup_path(path: Option<&Path>, suffix: &str) -> Option<PathBuf> {
    let mut bak = path?.as_os_str().to_owned();
    bak.push(suffix);
    Some(PathBuf::from(bak))
}

/// Copy of a non-empty database to `<file><suffix>` (0600), if there is none yet.
fn backup(db: &Connection, path: Option<&Path>, suffix: &str) -> crate::error::Result<()> {
    let Some(bak) = backup_path(path, suffix) else { return Ok(()) };
    if is_empty(db)? || bak.exists() {
        return Ok(());
    }
    // Created empty as 0600 first: `VACUUM INTO` would follow the umask.
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&bak)?;
    let copy = || -> crate::error::Result<()> {
        let to = bak.to_str().ok_or_else(|| std::io::Error::other("non UTF-8 path"))?;
        db.execute("VACUUM INTO ?1", [to])?;
        Ok(())
    };
    let done = copy();
    if done.is_err() {
        let _ = std::fs::remove_file(&bak);
    }
    done
}

/// `table` comes from a constant: it is bound anyway.
fn has_column(db: &Connection, table: &str, name: &str) -> rusqlite::Result<bool> {
    db.query_row("SELECT count(*) FROM pragma_table_info(?1) WHERE name = ?2", [table, name], |r| r.get(0))
}

/// v1: base tables; a database created before the `local` column gets it.
fn v1_base(tx: &Transaction<'_>, _: &str) -> rusqlite::Result<()> {
    tx.execute_batch(SCHEMA_V3)?;
    if !has_column(tx, "messages", "local")? {
        tx.execute("ALTER TABLE messages ADD COLUMN local INTEGER NOT NULL DEFAULT 0", [])?;
    }
    Ok(())
}

/// v2: before, everything was replayed as read; existing items are marked read up to their last item.
fn v2_reads(tx: &Transaction<'_>, _: &str) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO reads (key, until) SELECT key, max(ts) FROM \
         (SELECT key, ts FROM messages UNION ALL SELECT key, ts FROM calls) GROUP BY key",
        [],
    )?;
    Ok(())
}

/// v3: a deleted SMS stays in the database, hidden, so that a new MAP listing does not bring it back.
fn v3_hidden(tx: &Transaction<'_>, _: &str) -> rusqlite::Result<()> {
    if !has_column(tx, "messages", "hidden")? {
        tx.execute("ALTER TABLE messages ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0", [])?;
    }
    Ok(())
}

/// v4: one history per device; the existing rows and history start go to `device` (the one configured).
/// Calls recorded twice (same correspondent, second and kind) are kept once.
/// A v4 base set back to v3 by an older release (which also re-adds `since`) only gets its history start fixed.
fn v4_device(tx: &Transaction<'_>, device: &str) -> rusqlite::Result<()> {
    if !has_column(tx, "messages", "device")? {
        copy_to_v4(tx, device)?;
    }
    tx.execute("UPDATE OR IGNORE meta SET k = ?1 WHERE k = 'since'", [since_key(device)])?;
    tx.execute("DELETE FROM meta WHERE k = 'since'", [])?;
    Ok(())
}

/// v5: searchable text (lowercase without accents, see [`fold`]) and per-conversation flags (mute, archive).
/// A v5 base set back by an older release only gets the rows it added without `folded` filled.
fn v5_convs(tx: &Transaction<'_>, _: &str) -> rusqlite::Result<()> {
    if !has_column(tx, "messages", "folded")? {
        tx.execute("ALTER TABLE messages ADD COLUMN folded TEXT NOT NULL DEFAULT ''", [])?;
    }
    let rows: Vec<(i64, String)> =
        tx.prepare("SELECT id, text FROM messages WHERE folded = '' AND text <> ''")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut set = tx.prepare("UPDATE messages SET folded = ?2 WHERE id = ?1")?;
    for (id, text) in rows {
        set.execute(params![id, fold(&text)])?;
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS convs (
            device TEXT NOT NULL, key TEXT NOT NULL, muted INTEGER NOT NULL DEFAULT 0,
            archived INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (device, key));",
    )
}

fn copy_to_v4(tx: &Transaction<'_>, device: &str) -> rusqlite::Result<()> {
    tx.execute_batch(SCHEMA_V4)?;
    tx.execute(
        "INSERT INTO messages_new (id, device, key, number, ts, text, outgoing, local, hidden) \
         SELECT id, ?1, key, number, ts, text, outgoing, local, hidden FROM messages",
        [device],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO calls_new (id, device, key, number, ts, kind, secs) \
         SELECT id, ?1, key, number, ts, kind, secs FROM calls ORDER BY id",
        [device],
    )?;
    tx.execute("INSERT INTO contacts_new (device, key, number, name) SELECT ?1, key, number, name FROM contacts", [device])?;
    tx.execute("INSERT INTO reads_new (device, key, until) SELECT ?1, key, until FROM reads", [device])?;
    tx.execute_batch(
        "DROP TABLE messages; DROP TABLE calls; DROP TABLE contacts; DROP TABLE reads;
         ALTER TABLE messages_new RENAME TO messages; ALTER TABLE calls_new RENAME TO calls;
         ALTER TABLE contacts_new RENAME TO contacts; ALTER TABLE reads_new RENAME TO reads;
         CREATE INDEX messages_ts ON messages (device, ts); CREATE INDEX calls_ts ON calls (device, ts);",
    )

}


#[cfg(test)]
mod tests {
    use super::*;

    /// Fictitious addresses.
    const DEV: &str = "00:11:22:33:44:55";
    const OTHER: &str = "66:77:88:99:AA:BB";

    fn dev() -> BtAddr {
        DEV.parse().unwrap()
    }

    /// Fresh, process-unique directory.
    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bluez-message-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sms(addr: &str, ts: &str, text: &str) -> Sms {
        Sms { handle: "h".into(), address: addr.into(), timestamp: ts.into(), text: text.into(), ..Default::default() }
    }

    #[test]
    fn conv_flags_kept_per_device_and_replayed() {
        let h = History::memory(&dev()).unwrap();
        let conv = |h: &History| h.load(10).unwrap().into_iter().filter(|o| matches!(o, Out::Conv { .. })).collect::<Vec<_>>();
        let muted = ConvFlags { muted: true, archived: false };
        h.set_conv("600000001", ConvFlags { muted: true, archived: true }).unwrap();
        h.set_conv("600000001", muted).unwrap();
        h.db.execute("INSERT INTO convs (device, key, muted) VALUES (?1, '600000002', 1)", [OTHER]).unwrap();
        assert_eq!(conv(&h), [Out::Conv { key: "600000001".into(), flags: muted }], "upsert, other device left out");
        h.set_conv("600000001", ConvFlags::default()).unwrap();
        assert!(conv(&h).is_empty());
        assert_eq!(count(&h.db, "convs"), 1, "row removed when both flags are off");
        h.set_conv("600000001", muted).unwrap();
        h.delete("600000001", None).unwrap();
        assert!(conv(&h).is_empty(), "deleted with the conversation");
    }

    #[test]
    fn dedupes_and_reloads_in_order() {
        let mut h = History::memory(&dev()).unwrap();
        h.set_since("20261001T000000");
        assert_eq!(h.since(), "20261001T000000");
        assert!(h.add_sms(&sms("+33612345678", "20261015T100000", "a"), false).unwrap());
        assert!(!h.add_sms(&sms("06 12 34 56 78", "20261015T100000", "a"), false).unwrap(), "same message, number written differently");
        assert!(!h.add_sms(&sms("+33612345678", "20261015T095959", "a"), false).unwrap(), "timestamp off by one second");
        assert!(h.add_sms(&sms("+33612345678", "20261015T100100", "a"), false).unwrap(), "same text one minute later: new");
        assert!(h.add_local_sent("0612345678", "20261015T090000", "b").unwrap());
        // Return through the `sent` folder: replaces the local send, no duplicate.
        let back = Sms { recipient_address: "+33612345678".into(), timestamp: "20261015T090003".into(), text: "b".into(), ..Default::default() };
        assert!(!h.add_sms(&back, true).unwrap());
        h.add_call("0612345678", "20261015T110000", CallKind::Missed, 0).unwrap();
        let names = h.replace_contacts(&[("+33 6 12 34 56 78".into(), "Bob".into())]).unwrap();
        assert_eq!(names, [("+33 6 12 34 56 78".to_owned(), "Bob".to_owned())]);
        let v = h.load(10).unwrap();
        assert!(matches!(&v[0], Out::Contacts { names } if names.len() == 1));
        let texts: Vec<_> = v.iter().filter_map(|o| if let Out::Sms { sms, folder, .. } = o { Some((sms.text.as_str(), folder.as_str())) } else { None }).collect();
        assert_eq!(texts, [("b", "sent"), ("a", "inbox"), ("a", "inbox")]);
        assert!(matches!(v.last(), Some(Out::CallLog { kind: CallKind::Missed, .. })));
        // Read mark: does not go backwards, replayed at the end of loading.
        h.mark_read("612345678", "20261015T120000").unwrap();
        h.mark_read("612345678", "20261015T100000").unwrap();
        assert_eq!(h.load(10).unwrap().last(), Some(&Out::Read { key: "612345678".into(), until: "20261015T120000".into() }));
        // Replay into a fresh state: unread up to the recorded read mark, only what is later stays unread.
        h.add_sms(&sms("+33612345678", "20261015T130000", "c"), false).unwrap();
        let mut st = crate::app::state::State::default();
        h.load(10).unwrap().into_iter().for_each(|o| drop(st.apply(o)));
        assert_eq!(st.conv("612345678").map(|c| c.unread), Some(1));
        // Return through `sent` arrived before the end of the transfer: the local send is not recorded in addition.
        assert!(h.add_sms(&Sms { recipient_address: "0612345678".into(), timestamp: "20261015T140001".into(), text: "d".into(), ..Default::default() }, true).unwrap());
        assert!(!h.add_local_sent("+33612345678", "20261015T140000", "d").unwrap());
        assert!(h.add_local_sent("+33612345678", "20261015T150000", "d").unwrap(), "resent later: recorded");
        // Deletion: hidden, does not come back at the MAP relisting.
        let item = crate::engine::Item { timestamp: "20261015T130000".into(), text: "c".into(), outgoing: false, call: false };
        assert_eq!(h.delete("612345678", Some(&item)).unwrap(), 1);
        assert!(!h.add_sms(&sms("+33612345678", "20261015T130000", "c"), false).unwrap());
        let call = crate::engine::Item { timestamp: "20261015T110000".into(), text: String::new(), outgoing: false, call: true };
        assert_eq!(h.delete("612345678", Some(&call)).unwrap(), 1);
        assert!(!h.load(10).unwrap().iter().any(|o| matches!(o, Out::CallLog { .. }) || matches!(o, Out::Sms { sms: Sms { text, .. }, .. } if text == "c")));
        h.delete("612345678", None).unwrap();
        // Whole conversation: messages hidden and its read mark dropped.
        assert!(!h.load(10).unwrap().iter().any(|o| matches!(o, Out::Sms { .. } | Out::Read { .. })));
        // Limit: only the most recent ones.
        h.add_sms(&sms("+33700000000", "20261015T160000", "e"), false).unwrap();
        h.add_sms(&sms("+33700000000", "20261015T160100", "f"), false).unwrap();
        assert_eq!(h.load(1).unwrap().iter().filter(|o| matches!(o, Out::Sms { .. })).count(), 1);
    }

    #[test]
    fn purge_and_clear() {
        let mut h = History::memory(&dev()).unwrap();
        h.set_since("20261001T000000");
        h.add_sms(&sms("+33612345678", "20261002T100000", "old"), false).unwrap();
        h.add_sms(&sms("+33612345678", "20261015T100000", "new"), false).unwrap();
        h.add_call("0612345678", "20261002T110000", CallKind::Missed, 0).unwrap();
        assert_eq!(h.purge_before("20261010T000000").unwrap(), 2);
        // The stale SMS still on the iPhone does not come back: `since` has advanced.
        assert_eq!(h.since(), "20261010T000000");
        let item = crate::engine::Item { timestamp: "20261015T100000".into(), text: "new".into(), outgoing: false, call: false };
        h.delete("612345678", Some(&item)).unwrap();
        assert_eq!(h.purge_hidden().unwrap(), 1);
        assert_eq!(h.purge_hidden().unwrap(), 0);
        // Text erased, but the marker prevents the deleted SMS from coming back.
        assert!(!h.add_sms(&sms("+33612345678", "20261015T100001", "new"), false).unwrap());
        let text: String = h.db.query_row("SELECT text FROM messages WHERE hidden = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(text, "");
        h.replace_contacts(&[("+33612345678".into(), "Bob".into())]).unwrap();
        assert!(h.add_sms(&sms("+33612345678", "20261015T120000", "x"), false).unwrap());
        h.mark_read("612345678", "20261015T100000").unwrap();
        h.clear().unwrap();
        assert!(h.since() > "20261015T100000");
        let v = h.load(10).unwrap();
        assert!(matches!(v.as_slice(), [Out::Contacts { .. }]), "contacts kept, the rest cleared: {v:?}");
    }
    #[test]
    fn rekey_merges_and_is_idempotent() {
        let mut h = History::memory(&dev()).unwrap();
        h.set_since("20261001T000000");
        // Old keys (9 digits): written as such before the country was known.
        h.add_sms(&sms("06 00 00 00 01", "20261015T100000", "a"), false).unwrap();
        // Same SMS under another key (written by an older version), with its own read mark.
        h.db.execute("INSERT INTO messages (device, key, number, ts, text, outgoing) VALUES (?1, 'legacy', '+33 6 00 00 00 01', '20261015T100000', 'a', 0)", [DEV]).unwrap();
        h.add_call("03 89 00 00 00", "20261015T110000", CallKind::Missed, 0).unwrap();
        h.replace_contacts(&[("03 89 00 00 00".into(), "Shop".into())]).unwrap();
        h.mark_read("600000001", "20261015T090000").unwrap();
        h.mark_read("legacy", "20261015T100000").unwrap();
        h.mark_read("999999999", "20261015T100000").unwrap();
        h.db.execute_batch(
            "INSERT INTO convs (device, key, muted, archived) VALUES
             ('00:11:22:33:44:55', '600000001', 1, 1), ('00:11:22:33:44:55', 'legacy', 0, 0), ('00:11:22:33:44:55', '999999999', 1, 1);",
        )
        .unwrap();
        assert!(h.rekey(Some(Id::FR)).unwrap());
        assert!(!h.rekey(Some(Id::FR)).unwrap(), "already done");
        let keys = |h: &History, t: &str| -> Vec<String> {
            h.db.prepare(&format!("SELECT key FROM {t} ORDER BY key")).unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };
        assert_eq!(keys(&h, "messages"), ["+33600000001"], "same SMS under two writings: merged");
        assert_eq!(keys(&h, "calls"), ["+33389000000"]);
        assert_eq!(keys(&h, "contacts"), ["+33389000000"]);
        let reads: Vec<(String, String)> = h.db.prepare("SELECT key, until FROM reads").unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        assert_eq!(reads, [("+33600000001".to_owned(), "20261015T100000".to_owned())], "latest mark kept, orphan dropped");
        let convs: Vec<(String, bool, bool)> =
            h.db.prepare("SELECT key, muted, archived FROM convs").unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect();
        assert_eq!(convs, [("+33600000001".to_owned(), true, false)], "muted if either, archived if both, orphan dropped");
        // New SMS after the migration: same key.
        assert!(!h.add_sms(&sms("0600000001", "20261015T100000", "a"), false).unwrap());
        // Other country: national numbers read differently.
        assert!(h.rekey(Some(Id::BE)).unwrap());
        assert_ne!(keys(&h, "calls"), ["+33389000000"]);
        assert_eq!(keys(&h, "messages"), ["600000001"], "French national form, not a Belgian number: fallback");
    }

    #[test]
    fn rekey_keeps_deletion_of_merged_duplicate() {
        let mut h = History::memory(&dev()).unwrap();
        h.set_since("20261001T000000");
        h.db.execute("INSERT INTO messages (device, key, number, ts, text, outgoing, hidden) VALUES (?1, 'a', '06 00 00 00 01', '20261015T100000', 'x', 0, 0)", [DEV]).unwrap();
        h.db.execute("INSERT INTO messages (device, key, number, ts, text, outgoing, hidden) VALUES (?1, 'b', '+33 6 00 00 00 01', '20261015T100000', 'x', 0, 1)", [DEV]).unwrap();
        assert!(h.rekey(Some(Id::FR)).unwrap());
        let rows: Vec<(String, bool)> =
            h.db.prepare("SELECT key, hidden FROM messages").unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        assert_eq!(rows, [("+33600000001".to_owned(), true)], "deleted in either copy: stays deleted");
    }

    #[test]
    fn a_long_read_does_not_block_the_writer() {
        let dir = tmp("wal");
        let mut h = History::open(&dir.join("history.db"), &dev()).unwrap();
        h.set_since("20261001T000000");
        h.db.busy_timeout(std::time::Duration::from_millis(100)).unwrap();
        let r = h.reader().unwrap();
        // Read transaction left open, as during an export.
        r.db.execute_batch("BEGIN").unwrap();
        let n: i64 = r.db.query_row("SELECT count(*) FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
        assert!(h.add_sms(&sms("+33600000001", "20261015T100000", "a"), false).unwrap());
        r.db.execute_batch("COMMIT").unwrap();
        let wal = dir.join("history.db-wal");
        assert_eq!(std::fs::metadata(&wal).unwrap().permissions().mode() & 0o777, 0o600);
        h.clear().unwrap();
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0, "emptied by the purge");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn backup_before_first_rekey() {
        let dir = tmp("rekey");
        let db = dir.join("history.db");
        let mut h = History::open(&db, &dev()).unwrap();
        h.set_since("20261001T000000");
        h.add_sms(&sms("06 00 00 00 01", "20261015T100000", "a"), false).unwrap();
        assert!(h.rekey(Some(Id::FR)).unwrap());
        let bak = dir.join("history.db.bak");
        assert_eq!(std::fs::metadata(&bak).unwrap().permissions().mode() & 0o777, 0o600);
        let old: String = Connection::open(&bak).unwrap().query_row("SELECT key FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(old, "600000001", "copy taken before the migration");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrates_v3_to_v4() {
        let dir = tmp("v3");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("history.db");
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch(SCHEMA_V3).unwrap();
            c.execute_batch(
                "INSERT INTO meta (k, v) VALUES ('since', '20261001T000000'), ('key_home', 'v1:FR');
                 INSERT INTO messages (key, number, ts, text, outgoing) VALUES ('+33600000001', '0600000001', '20261015T100000', 'a', 0);
                 INSERT INTO calls (key, number, ts, kind, secs) VALUES ('+33600000001', '0600000001', '20261015T110000', 'missed', 0);
                 INSERT INTO calls (key, number, ts, kind, secs) VALUES ('+33600000001', '0600000001', '20261015T110000', 'missed', 0);
                 INSERT INTO contacts (key, number, name) VALUES ('+33600000001', '0600000001', 'Bob');
                 INSERT INTO reads (key, until) VALUES ('+33600000001', '20261015T100000');
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let h = History::open(&db, &dev()).unwrap();
        assert_eq!(h.since(), "20261001T000000", "history start kept");
        let version: i64 = h.db.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 5);
        for table in ["messages", "calls", "contacts", "reads"] {
            let devices: Vec<String> =
                h.db.prepare(&format!("SELECT DISTINCT device FROM {table}")).unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
            assert_eq!(devices, [DEV], "{table}");
        }
        let calls: i64 = h.db.query_row("SELECT count(*) FROM calls", [], |r| r.get(0)).unwrap();
        assert_eq!(calls, 1, "duplicate call merged");
        assert!(h.load(10).unwrap().contains(&Out::Read { key: "+33600000001".into(), until: "20261015T100000".into() }));
        let bak = dir.join("history.db.v3.bak");
        assert_eq!(std::fs::metadata(&bak).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(!has_column(&Connection::open(&bak).unwrap(), "messages", "device").unwrap(), "copy taken before the migration");
        drop(h);
        let h = History::open(&db, &dev()).unwrap();
        assert_eq!(h.load(10).unwrap().iter().filter(|o| matches!(o, Out::Sms { .. })).count(), 1, "reopening changes nothing");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn v4_set_back_to_v3_by_older_release() {
        let dir = tmp("v3back");
        let db = dir.join("history.db");
        {
            let mut h = History::open(&db, &dev()).unwrap();
            h.set_since("20261001T000000");
            h.add_call("+33600000001", "20261015T110000", CallKind::Missed, 0).unwrap();
        }
        // What v0.1.5 does on start: user_version 3 and a fresh 'since'.
        Connection::open(&db)
            .unwrap()
            .execute_batch("PRAGMA user_version = 3; INSERT OR IGNORE INTO meta (k, v) VALUES ('since', '20261018T000000');")
            .unwrap();
        let h = History::open(&db, &dev()).unwrap();
        assert_eq!(h.since(), "20261001T000000", "history start kept");
        let version: i64 = h.db.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 5);
        let stale: i64 = h.db.query_row("SELECT count(*) FROM meta WHERE k = 'since'", [], |r| r.get(0)).unwrap();
        assert_eq!(stale, 0);
        assert_eq!(h.load(10).unwrap().iter().filter(|o| matches!(o, Out::CallLog { .. })).count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn devices_are_isolated() {
        let dir = tmp("devices");
        let db = dir.join("history.db");
        let other: BtAddr = OTHER.parse().unwrap();
        {
            let mut a = History::open(&db, &dev()).unwrap();
            a.set_since("20261001T000000");
            a.add_sms(&sms("+33600000001", "20261015T100000", "a"), false).unwrap();
            a.add_call("+33600000001", "20261015T110000", CallKind::Missed, 0).unwrap();
            a.replace_contacts(&[("+33600000001".into(), "Bob".into())]).unwrap();
            a.mark_read("600000001", "20261015T100000").unwrap();
        }
        let mut b = History::open(&db, &other).unwrap();
        assert!(b.since() > "20261015T110000", "own history start");
        assert!(b.load(10).unwrap().is_empty(), "nothing from the other device");
        // Same SMS on the other device: recorded there too.
        b.set_since("20261001T000000");
        assert!(b.add_sms(&sms("+33600000001", "20261015T100000", "a"), false).unwrap());
        assert_eq!(b.delete("600000001", None).unwrap(), 1, "only its own SMS");
        b.replace_contacts(&[]).unwrap();
        b.clear().unwrap();
        drop(b);
        let a = History::open(&db, &dev()).unwrap();
        assert_eq!(a.since(), "20261001T000000");
        let v = a.load(10).unwrap();
        assert!(matches!(v.as_slice(), [Out::Contacts { .. }, Out::Sms { .. }, Out::CallLog { .. }, Out::Read { .. }]), "untouched: {v:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn count(c: &Connection, table: &str) -> i64 {
        c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn migrates_v4_to_v5() {
        let dir = tmp("v4");
        let db = dir.join("history.db");
        {
            let mut h = History::open(&db, &dev()).unwrap();
            h.set_since("20261001T000000");
            h.add_sms(&sms("06 00 00 00 01", "20261015T100000", "Été à l'Île"), false).unwrap();
            h.add_sms(&sms("06 00 00 00 01", "20261015T100100", "ok"), false).unwrap();
            h.add_call("06 00 00 00 01", "20261015T110000", CallKind::Missed, 0).unwrap();
            h.mark_read("+33600000001", "20261015T100000").unwrap();
            h.db.execute_batch("ALTER TABLE messages DROP COLUMN folded; DROP TABLE convs; PRAGMA user_version = 4;").unwrap();
        }
        let h = History::open(&db, &dev()).unwrap();
        let version: i64 = h.db.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 5);
        let folded: Vec<String> = h.db.prepare("SELECT folded FROM messages ORDER BY ts").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(folded, ["ete a l'ile", "ok"]);
        assert_eq!([count(&h.db, "messages"), count(&h.db, "calls"), count(&h.db, "reads"), count(&h.db, "convs")], [2, 1, 1, 0]);
        let ok: String = h.db.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(ok, "ok");
        let bak = dir.join("history.db.v4.bak");
        assert_eq!(std::fs::metadata(&bak).unwrap().permissions().mode() & 0o777, 0o600);
        let old: i64 = Connection::open(&bak).unwrap().pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(old, 4, "copy taken before the migration");
        // New SMS: folded on insert.
        h.add_sms(&sms("06 00 00 00 01", "20261015T100200", "ÇA"), false).unwrap();
        let last: String = h.db.query_row("SELECT folded FROM messages ORDER BY ts DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(last, "ca");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn v5_set_back_by_older_release() {
        let dir = tmp("v5back");
        let db = dir.join("history.db");
        {
            let mut h = History::open(&db, &dev()).unwrap();
            h.set_since("20261001T000000");
            h.add_sms(&sms("06 00 00 00 01", "20261015T100000", "Déjà"), false).unwrap();
            h.db.execute("INSERT INTO convs (device, key, muted) VALUES (?1, '+33600000001', 1)", [DEV]).unwrap();
            // Older release: writes without `folded` and resets the version.
            h.db.execute_batch(
                "INSERT INTO messages (device, key, number, ts, text, outgoing) VALUES ('00:11:22:33:44:55', '+33600000001', '0600000001', '20261015T100100', 'Où', 0);
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let h = History::open(&db, &dev()).unwrap();
        let folded: Vec<String> = h.db.prepare("SELECT folded FROM messages ORDER BY ts").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(folded, ["deja", "ou"]);
        assert_eq!(count(&h.db, "convs"), 1, "flags kept");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clear_drops_flags_and_folded_purged() {
        let mut h = History::memory(&dev()).unwrap();
        h.set_since("20261001T000000");
        h.add_sms(&sms("06 00 00 00 01", "20261015T100000", "Déjà"), false).unwrap();
        h.db.execute("UPDATE messages SET hidden = 1", []).unwrap();
        h.purge_hidden().unwrap();
        let f: String = h.db.query_row("SELECT folded FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(f, "", "erased text not searchable");
        h.db.execute("INSERT INTO convs (device, key, muted) VALUES (?1, '+33600000001', 1), ('66:77:88:99:AA:BB', 'x', 1)", [DEV]).unwrap();
        h.clear().unwrap();
        assert_eq!(count(&h.db, "convs"), 1, "other device kept");
    }

    #[test]
    fn search_and_older_pages() {
        let dir = tmp("search");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("history.db");
        let h = History::open(&db, &dev()).unwrap();
        h.add_sms(&sms("+33600000001", "20261015T100000", "Bel été à Noël"), false).unwrap();
        h.add_sms(&sms("+33600000001", "20261015T100100", "100% sûr"), false).unwrap();
        h.add_sms(&sms("+33600000001", "20261015T100200", "a_b"), false).unwrap();
        h.add_sms(&sms("+33600000002", "20261015T100300", "ETE encore"), false).unwrap();
        let texts = |v: &[Hit]| v.iter().map(|x| x.text.clone()).collect::<Vec<_>>();
        let (v, next) = h.reader().unwrap().search("Éte", None).unwrap();
        assert_eq!(texts(&v), ["ETE encore", "Bel été à Noël"], "accents and case ignored, newest first");
        assert!(next.is_none());
        assert_eq!(v[1].key, "600000001");
        assert_eq!(texts(&h.reader().unwrap().search("%", None).unwrap().0), ["100% sûr"], "% taken literally");
        assert_eq!(texts(&h.reader().unwrap().search("_", None).unwrap().0), ["a_b"], "_ taken literally");
        assert!(h.reader().unwrap().search("\\", None).unwrap().0.is_empty());
        assert!(h.reader().unwrap().search("\u{301}\u{301}", None).unwrap().0.is_empty(), "nothing left once folded: no match-all");
        // Deleted messages and another device's history left out.
        let item = crate::engine::Item { timestamp: "20261015T100300".into(), text: "ETE encore".into(), outgoing: false, call: false };
        h.delete("600000002", Some(&item)).unwrap();
        let other = History::open(&db, &"00:11:22:33:44:66".parse().unwrap()).unwrap();
        other.add_sms(&sms("+33600000001", "20261015T110000", "été ailleurs"), false).unwrap();
        assert_eq!(texts(&h.reader().unwrap().search("ete", None).unwrap().0), ["Bel été à Noël"]);
        // Pagination: 100 per page, the cursor resumes strictly after the last one, same-second rows kept.
        for i in 0..150 {
            h.add_sms(&sms("+33600000003", &format!("20261016T{:02}{:02}00", i / 60, i % 60), &format!("page {i}")), false).unwrap();
        }
        h.add_sms(&sms("+33600000004", "20261016T000000", "page bis"), false).unwrap();
        let (p1, next) = h.reader().unwrap().search("page", None).unwrap();
        assert_eq!(p1.len(), HITS_PAGE);
        let (p2, end) = h.reader().unwrap().search("page", next.as_ref()).unwrap();
        assert_eq!(p2.len(), 51);
        assert!(end.is_none());
        let mut all: Vec<_> = p1.iter().chain(&p2).map(|x| x.id).collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 151, "no row lost nor repeated across pages");
        // Older: one conversation only, before the cursor.
        let (o, next) = h.reader().unwrap().older("600000003", &Cursor::after("20261016T000500"), 3).unwrap();
        assert_eq!(texts(&o), ["page 5", "page 4", "page 3"], "same timestamp as the cursor included");
        let (o, end) = h.reader().unwrap().older("600000003", next.as_ref().unwrap(), 3).unwrap();
        assert_eq!(texts(&o), ["page 2", "page 1", "page 0"]);
        assert!(end.is_none());
        drop((h, other));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn newer_version_refused() {
        let dir = tmp("newer");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("history.db");
        Connection::open(&db).unwrap().pragma_update(None, "user_version", 6).unwrap();
        assert!(matches!(History::open(&db, &dev()), Err(Error::DbVersion(6))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exports_json_of_active_device() {
        let dir = tmp("export");
        let db = dir.join("history.db");
        {
            let mut o = History::open(&db, &OTHER.parse().unwrap()).unwrap();
            o.set_since("20261001T000000");
            o.add_sms(&sms("+33600000003", "20261015T090000", "other device"), false).unwrap();
        }
        let mut h = History::open(&db, &dev()).unwrap();
        h.set_since("20261001T000000");
        h.add_sms(&sms("+33600000001", "20261015T100000", "a \"quoted\"\nline"), false).unwrap();
        h.add_sms(&sms("+33600000001", "20261015T100500", "hidden"), false).unwrap();
        h.add_local_sent("+33600000001", "20261015T101000", "b").unwrap();
        h.add_sms(&sms("+33600000002", "20261015T110000", "c"), false).unwrap();
        h.add_call("+33600000001", "20261015T120000", CallKind::Missed, 0).unwrap();
        h.replace_contacts(&[("+33600000001".into(), "Bob".into()), ("+33600000002".into(), "Alice".into())]).unwrap();
        let item = crate::engine::Item { timestamp: "20261015T100500".into(), text: "hidden".into(), outgoing: false, call: false };
        h.delete("600000001", Some(&item)).unwrap();

        let mut all = vec![];
        h.reader().unwrap().export(None, Some("Phone"), &mut all).unwrap();
        let all = String::from_utf8(all).unwrap();
        assert!(all.ends_with("}\n"));
        assert!(!all.contains(DEV) && !all.contains(OTHER), "no device address");
        let v: serde_json::Value = serde_json::from_str(&all).unwrap();
        assert_eq!(v["format"], 1);
        assert!(v["conversation"].is_null());
        assert_eq!(v["device"], "Phone");
        let names: Vec<_> = v["contacts"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Alice", "Bob"]);
        let texts: Vec<_> = v["messages"].as_array().unwrap().iter().map(|m| (m["direction"].as_str().unwrap(), m["text"].as_str().unwrap())).collect();
        assert_eq!(texts, [("in", "a \"quoted\"\nline"), ("out", "b"), ("in", "c")], "oldest first, hidden and other device left out");
        assert_eq!(v["calls"][0], serde_json::json!({ "timestamp": "20261015T120000", "number": "+33600000001", "kind": "missed", "secs": 0 }));

        let path = h.reader().unwrap().export_to(&dir, Some("600000002"), None).unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("bluez-message-2") && name.ends_with("-600000002.json"), "{name}");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["conversation"], "600000002");
        assert_eq!(v["contacts"].as_array().unwrap().len(), 1);
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
        assert!(v["calls"].as_array().unwrap().is_empty());
        // Existing file not overwritten; no temporary file left behind.
        let size = std::fs::metadata(&path).unwrap().len();
        let next = h.reader().unwrap().export_to(&dir, Some("600000002"), None).unwrap();
        assert_ne!(next, path);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size);
        // Same second: `-2` to `-9`, then refused.
        let r = h.reader().unwrap();
        let names: Vec<_> = (0..9).map(|_| r.export_at(&dir, None, None, "20261018T120000").unwrap().file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names[..3], ["bluez-message-20261018-120000.json", "bluez-message-20261018-120000-2.json", "bluez-message-20261018-120000-3.json"]);
        assert_eq!(names[8], "bluez-message-20261018-120000-9.json");
        assert!(r.export_at(&dir, None, None, "20261018T120000").is_err());
        assert!(std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".part")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
