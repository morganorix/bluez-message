//! Local history (SQLite) kept by the daemon: SMS and calls received since its first start,
//! iPhone contacts. Older SMS left on the phone are not imported.

use std::collections::HashMap;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::numbering::{self, Id};
use crate::phone::sms_peer;
use crate::engine::{CallKind, Out};
use crate::stamp;
use crate::map::{Folder, Sms};

const SCHEMA: &str = "
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

/// Timestamp gap below which an identical SMS (same correspondent, direction and text) is the same one.
const SAME_SMS_SECS: i64 = 2;
/// A send from the app that already came back through the `sent` folder (phone clock) within this gap is not recorded again.
pub const SENT_ECHO_SECS: i64 = 30;
/// Format of the grouping keys (see [`numbering::key`]), stored with the country in `meta 'key_home'`.
const KEY_FORMAT: &str = "v1";

pub struct History {
    db: Connection,
    since: String,
    /// Country numbers are read with (grouping keys), see [`History::rekey`].
    home: Option<Id>,
    /// Database file (copy before the first migration of the keys); `None` in memory.
    path: Option<PathBuf>,
}

impl History {
    /// Opens (or creates, directory 0700 and file 0600) the database.
    ///
    /// # Errors
    /// Directory or database cannot be created or restricted, or schema rejected.
    pub fn open(path: &Path) -> crate::error::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        // Created as 0600 before SQLite opens it (no umask window); a symbolic link is refused.
        std::fs::OpenOptions::new().append(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let db = Connection::open(path)?;
        Ok(Self { path: Some(path.to_owned()), ..Self::init(db)? })
    }

    /// In-memory database (tests).
    ///
    /// # Errors
    /// Schema rejected.
    pub fn memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> rusqlite::Result<Self> {
        // Deleted rows are overwritten with zeros on disk (and in the journal).
        db.pragma_update(None, "secure_delete", true)?;
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        db.execute_batch(SCHEMA)?;
        // Database created before the `local` column (v1): added if missing.
        if !Self::has_column(&db, "local")? {
            db.execute("ALTER TABLE messages ADD COLUMN local INTEGER NOT NULL DEFAULT 0", [])?;
        }
        // Before v2, everything was replayed as read: existing items are marked read up to their last item.
        if version < 2 {
            db.execute(
                "INSERT OR IGNORE INTO reads (key, until) SELECT key, max(ts) FROM \
                 (SELECT key, ts FROM messages UNION ALL SELECT key, ts FROM calls) GROUP BY key",
                [],
            )?;
        }
        // v3: a deleted SMS stays in the database, hidden, so that a new MAP listing does not bring it back.
        if !Self::has_column(&db, "hidden")? {
            db.execute("ALTER TABLE messages ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0", [])?;
        }
        db.pragma_update(None, "user_version", 3)?;
        db.execute("INSERT OR IGNORE INTO meta (k, v) VALUES ('since', ?1)", [stamp::now()])?;
        let since = db.query_row("SELECT v FROM meta WHERE k = 'since'", [], |r| r.get(0))?;
        Ok(Self { db, since, home: None, path: None })
    }

    /// Reads the numbers with country `home` from now on, and recomputes the stored grouping keys if
    /// they were made otherwise (older format, other country): conversations of the same number merge,
    /// read marks follow (the latest wins). The first time, the database is copied to `<file>.bak`.
    /// Returns `true` if the keys changed.
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
            self.backup()?;
        }
        let tx = self.db.transaction()?;
        // Old key → new keys (a key made of the last 9 digits may split).
        let mut moved: HashMap<String, Vec<String>> = HashMap::new();
        for table in ["messages", "calls", "contacts"] {
            let rows: Vec<(i64, String, String)> =
                tx.prepare(&format!("SELECT rowid, key, number FROM {table} ORDER BY rowid"))?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
            let changed: Vec<(i64, String)> = rows
                .into_iter()
                .filter_map(|(id, old, number)| {
                    let new = numbering::key(&number, home);
                    let seen = moved.entry(old.clone()).or_default();
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
                             FROM (SELECT ts, outgoing, text, hidden, local FROM messages WHERE rowid = ?1) AS d \
                             WHERE key = ?2 AND messages.ts = d.ts AND messages.outgoing = d.outgoing AND messages.text = d.text",
                            params![id, new],
                        )?;
                    }
                    drop.execute([id])?;
                }
            }
        }
        let reads: Vec<(String, String)> = tx.prepare("SELECT key, until FROM reads")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        tx.execute("DELETE FROM reads", [])?;
        {
            let mut ins = tx.prepare("INSERT INTO reads (key, until) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET until = max(until, excluded.until)")?;
            // A mark whose conversation no longer exists is dropped.
            for (old, until) in &reads {
                for new in moved.get(old).into_iter().flatten() {
                    ins.execute(params![new, until])?;
                }
            }
        }
        tx.execute("INSERT INTO meta (k, v) VALUES ('key_home', ?1) ON CONFLICT (k) DO UPDATE SET v = excluded.v", [&tag])?;
        tx.commit()?;
        self.home = home;
        Ok(true)
    }

    /// `<file>.bak`, copy made before the first [`Self::rekey`].
    fn backup_path(&self) -> Option<PathBuf> {
        let mut bak = self.path.clone()?.into_os_string();
        bak.push(".bak");
        Some(PathBuf::from(bak))
    }

    /// Copy of a non-empty database to `<file>.bak` (0600), if there is none yet.
    fn backup(&self) -> crate::error::Result<()> {
        let Some(bak) = self.backup_path() else { return Ok(()) };
        let empty: bool = self.db.query_row("SELECT NOT EXISTS (SELECT 1 FROM messages) AND NOT EXISTS (SELECT 1 FROM calls)", [], |r| r.get(0))?;
        if empty || bak.exists() {
            return Ok(());
        }
        // Created empty as 0600 first: `VACUUM INTO` would follow the umask.
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&bak)?;
        let copy = || -> crate::error::Result<()> {
            let to = bak.to_str().ok_or_else(|| std::io::Error::other("non UTF-8 path"))?;
            self.db.execute("VACUUM INTO ?1", [to])?;
            Ok(())
        };
        let done = copy();
        if done.is_err() {
            let _ = std::fs::remove_file(&bak);
        }
        done
    }

    fn has_column(db: &Connection, name: &str) -> rusqlite::Result<bool> {
        db.query_row("SELECT count(*) FROM pragma_table_info('messages') WHERE name = ?1", [name], |r| r.get(0))
    }

    /// History start (MAP format): earlier SMS are ignored.
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
                    "UPDATE OR IGNORE messages SET ts = ?3, local = 0 WHERE id = \
                     (SELECT id FROM messages WHERE key = ?1 AND text = ?2 AND outgoing = 1 AND local = 1 ORDER BY ts LIMIT 1)",
                )?
                .execute(params![numbering::key(number, self.home), sms.text, sms.timestamp])?;
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

    /// An SMS with the same correspondent, direction and text within `secs`; `extra`: additional SQL condition (constant).
    fn near(&self, number: &str, ts: &str, text: &str, outgoing: bool, secs: i64, extra: &str) -> rusqlite::Result<bool> {
        let Some(t) = stamp::secs(ts) else { return Ok(false) };
        let mut st = self.db.prepare_cached(&format!("SELECT ts FROM messages WHERE key = ?1 AND text = ?2 AND outgoing = ?3{extra}"))?;
        let found = st
            .query_map(params![numbering::key(number, self.home), text, outgoing], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok().and_then(|s| stamp::secs(&s)))
            .any(|u| (u - t).abs() <= secs);
        Ok(found)
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
        let n = match item {
            None => {
                let tx = self.db.unchecked_transaction()?;
                let n = tx.execute("UPDATE messages SET hidden = 1 WHERE key = ?1", [key])? + tx.execute("DELETE FROM calls WHERE key = ?1", [key])?;
                tx.execute("DELETE FROM reads WHERE key = ?1", [key])?;
                tx.commit()?;
                n
            }
            Some(i) if i.call => self.db.execute(
                "DELETE FROM calls WHERE id = (SELECT id FROM calls WHERE key = ?1 AND ts = ?2 AND (kind = 'out') = ?3 LIMIT 1)",
                params![key, i.timestamp, i.outgoing],
            )?,
            Some(i) => self.db.execute(
                "UPDATE messages SET hidden = 1 WHERE key = ?1 AND ts = ?2 AND text = ?3 AND outgoing = ?4",
                params![key, i.timestamp, i.text, i.outgoing],
            )?,
        };
        Ok(n)
    }

    /// Erases SMS and calls older than `before` (MAP timestamp); `since` advances to that point so
    /// that SMS left on the iPhone do not come back at the next listing. Returns the number of rows.
    ///
    /// # Errors
    /// SQLite error.
    pub fn purge_before(&mut self, before: &str) -> rusqlite::Result<usize> {
        let tx = self.db.transaction()?;
        let n = tx.execute("DELETE FROM messages WHERE ts < ?1", [before])? + tx.execute("DELETE FROM calls WHERE ts < ?1", [before])?;
        tx.execute("UPDATE meta SET v = max(v, ?1) WHERE k = 'since'", [before])?;
        let since: String = tx.query_row("SELECT v FROM meta WHERE k = 'since'", [], |r| r.get(0))?;
        tx.commit()?;
        self.since = since;
        Ok(n)
    }

    /// Erases the text of hidden SMS (deleted from the app). The row stays, empty, as a marker: an SMS
    /// still on the iPhone does not come back (`near_duplicate`); older than `since`, it disappears.
    ///
    /// # Errors
    /// SQLite error.
    pub fn purge_hidden(&self) -> rusqlite::Result<usize> {
        let n = self.db.execute("UPDATE OR REPLACE messages SET text = '' WHERE hidden = 1 AND text <> ''", [])?;
        self.db.execute("DELETE FROM messages WHERE hidden = 1 AND ts < ?1", [&self.since])?;
        Ok(n)
    }

    /// Clears the history (SMS, calls, read marks) and its `.bak` copy; contacts are kept. `since`
    /// moves to now: SMS left on the iPhone do not come back.
    ///
    /// # Errors
    /// SQLite error.
    pub fn clear(&mut self) -> rusqlite::Result<()> {
        let now = stamp::now();
        let tx = self.db.transaction()?;
        tx.execute_batch("DELETE FROM messages; DELETE FROM calls; DELETE FROM reads;")?;
        tx.execute("UPDATE meta SET v = ?1 WHERE k = 'since'", [&now])?;
        tx.commit()?;
        self.since = now;
        if let Some(bak) = self.backup_path()
            && let Err(e) = std::fs::remove_file(&bak)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("history backup not removed: {e}");
        }
        Ok(())
    }

    fn insert_msg(&self, number: &str, ts: &str, text: &str, outgoing: bool, local: bool) -> rusqlite::Result<bool> {
        let n = self
            .db
            .prepare_cached("INSERT OR IGNORE INTO messages (key, number, ts, text, outgoing, local) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
            .execute(params![numbering::key(number, self.home), number, ts, text, outgoing, local])?;
        Ok(n == 1)
    }

    /// Records a call (`secs`: conversation duration).
    ///
    /// # Errors
    /// SQLite error.
    pub fn add_call(&self, number: &str, ts: &str, kind: CallKind, secs: u64) -> rusqlite::Result<()> {
        self.db
            .prepare_cached("INSERT INTO calls (key, number, ts, kind, secs) VALUES (?1, ?2, ?3, ?4, ?5)")?
            .execute(params![numbering::key(number, self.home), number, ts, kind.as_str(), i64::try_from(secs).unwrap_or(i64::MAX)])?;
        Ok(())
    }

    /// Conversation read up to `until` (never goes backwards).
    ///
    /// # Errors
    /// SQLite error.
    pub fn mark_read(&self, key: &str, until: &str) -> rusqlite::Result<()> {
        self.db
            .prepare_cached("INSERT INTO reads (key, until) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET until = max(until, excluded.until)")?
            .execute(params![key, until])?;
        Ok(())
    }

    /// Replaces the contacts; returns the (number, name) pairs published to the clients.
    ///
    /// # Errors
    /// SQLite error (the transaction is rolled back).
    pub fn replace_contacts(&mut self, contacts: &[(String, String)]) -> rusqlite::Result<Vec<(String, String)>> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM contacts", [])?;
        {
            let mut ins = tx.prepare("INSERT OR IGNORE INTO contacts (key, number, name) VALUES (?1, ?2, ?3)")?;
            for (number, name) in contacts {
                ins.execute(params![numbering::key(number, self.home), number, name])?;
            }
        }
        tx.commit()?;
        self.contacts()
    }

    fn contacts(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let mut st = self.db.prepare("SELECT number, name FROM contacts")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
    }

    /// What the daemon replays at startup: contacts, the last `limit` SMS and calls, then the read marks.
    ///
    /// # Errors
    /// SQLite error.
    pub fn load(&self, limit: usize) -> rusqlite::Result<Vec<Out>> {
        let mut v = vec![];
        let names = self.contacts()?;
        if !names.is_empty() {
            v.push(Out::Contacts { names });
        }
        let lim = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut st = self.db.prepare(
            "SELECT id, number, ts, text, outgoing FROM (SELECT * FROM messages WHERE hidden = 0 ORDER BY ts DESC, id DESC LIMIT ?1) ORDER BY ts, id",
        )?;
        let msgs = st.query_map([lim], |r| {
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
        let mut st = self.db.prepare("SELECT number, ts, kind, secs FROM (SELECT * FROM calls ORDER BY ts DESC, id DESC LIMIT ?1) ORDER BY ts, id")?;
        let calls = st.query_map([lim], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?)))?;
        for c in calls {
            let (number, timestamp, kind, secs) = c?;
            if let Some(kind) = CallKind::parse(&kind) {
                v.push(Out::CallLog { number, timestamp, kind, secs: u64::try_from(secs).unwrap_or(0), read: false });
            }
        }
        let mut st = self.db.prepare("SELECT key, until FROM reads")?;
        for r in st.query_map([], |r| Ok(Out::Read { key: r.get(0)?, until: r.get(1)? }))? {
            v.push(r?);
        }
        Ok(v)
    }

    #[cfg(test)]
    fn set_since(&mut self, s: &str) {
        self.db.execute("UPDATE meta SET v = ?1 WHERE k = 'since'", [s]).unwrap();
        self.since = s.to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sms(addr: &str, ts: &str, text: &str) -> Sms {
        Sms { handle: "h".into(), address: addr.into(), timestamp: ts.into(), text: text.into(), ..Default::default() }
    }

    #[test]
    fn dedupes_and_reloads_in_order() {
        let mut h = History::memory().unwrap();
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
        let mut h = History::memory().unwrap();
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
        let mut h = History::memory().unwrap();
        h.set_since("20261001T000000");
        // Old keys (9 digits): written as such before the country was known.
        h.add_sms(&sms("06 00 00 00 01", "20261015T100000", "a"), false).unwrap();
        // Same SMS under another key (written by an older version), with its own read mark.
        h.db.execute("INSERT INTO messages (key, number, ts, text, outgoing) VALUES ('legacy', '+33 6 00 00 00 01', '20261015T100000', 'a', 0)", []).unwrap();
        h.add_call("03 89 00 00 00", "20261015T110000", CallKind::Missed, 0).unwrap();
        h.replace_contacts(&[("03 89 00 00 00".into(), "Shop".into())]).unwrap();
        h.mark_read("600000001", "20261015T090000").unwrap();
        h.mark_read("legacy", "20261015T100000").unwrap();
        h.mark_read("999999999", "20261015T100000").unwrap();
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
        // New SMS after the migration: same key.
        assert!(!h.add_sms(&sms("0600000001", "20261015T100000", "a"), false).unwrap());
        // Other country: national numbers read differently.
        assert!(h.rekey(Some(Id::BE)).unwrap());
        assert_ne!(keys(&h, "calls"), ["+33389000000"]);
        assert_eq!(keys(&h, "messages"), ["600000001"], "French national form, not a Belgian number: fallback");
    }

    #[test]
    fn rekey_keeps_deletion_of_merged_duplicate() {
        let mut h = History::memory().unwrap();
        h.set_since("20261001T000000");
        h.db.execute("INSERT INTO messages (key, number, ts, text, outgoing, hidden) VALUES ('a', '06 00 00 00 01', '20261015T100000', 'x', 0, 0)", []).unwrap();
        h.db.execute("INSERT INTO messages (key, number, ts, text, outgoing, hidden) VALUES ('b', '+33 6 00 00 00 01', '20261015T100000', 'x', 0, 1)", []).unwrap();
        assert!(h.rekey(Some(Id::FR)).unwrap());
        let rows: Vec<(String, bool)> =
            h.db.prepare("SELECT key, hidden FROM messages").unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        assert_eq!(rows, [("+33600000001".to_owned(), true)], "deleted in either copy: stays deleted");
    }

    #[test]
    fn backup_before_first_rekey() {
        let dir = std::env::temp_dir().join(format!("bluez-message-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = dir.join("history.db");
        let mut h = History::open(&db).unwrap();
        h.set_since("20261001T000000");
        h.add_sms(&sms("06 00 00 00 01", "20261015T100000", "a"), false).unwrap();
        assert!(h.rekey(Some(Id::FR)).unwrap());
        let bak = dir.join("history.db.bak");
        assert_eq!(std::fs::metadata(&bak).unwrap().permissions().mode() & 0o777, 0o600);
        let old: String = Connection::open(&bak).unwrap().query_row("SELECT key FROM messages", [], |r| r.get(0)).unwrap();
        assert_eq!(old, "600000001", "copy taken before the migration");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
