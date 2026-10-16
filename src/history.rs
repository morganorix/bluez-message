//! Historique local (SQLite) tenu par le démon : SMS et appels arrivés depuis sa première mise en service,
//! répertoire de l'iPhone. Les SMS plus anciens restés sur le téléphone ne sont pas repris.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rusqlite::{Connection, params};

use crate::phone::{number_key, sms_peer};
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

/// Écart d'horodatage sous lequel un SMS identique (même correspondant, sens et texte) est le même.
const SAME_SMS_SECS: i64 = 2;
/// Un envoi de l'app déjà revenu par le dossier `sent` (horloge du téléphone) dans cet écart n'est pas reconsigné.
pub const SENT_ECHO_SECS: i64 = 30;

pub struct History {
    db: Connection,
    since: String,
}

impl History {
    /// Ouvre (ou crée, dossier 0700 et fichier 0600) la base.
    ///
    /// # Errors
    /// Dossier ou base impossible à créer, ou schéma refusé.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        // Créé en 0600 avant que SQLite ne l'ouvre (pas de fenêtre avec l'umask).
        let _ = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path);
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        let db = Connection::open(path)?;
        Self::init(db)
    }

    /// Base en mémoire (tests).
    ///
    /// # Errors
    /// Schéma refusé.
    pub fn memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> rusqlite::Result<Self> {
        // Base créée avant la colonne `local` : ajoutée (erreur « duplicate column » ignorée sinon).
        if db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? < 1 {
            let _ = db.execute("ALTER TABLE messages ADD COLUMN local INTEGER NOT NULL DEFAULT 0", []);
        }
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        db.execute_batch(SCHEMA)?;
        // Avant la v2, tout était rejoué lu : l'existant est marqué lu jusqu'à son dernier élément.
        if version < 2 {
            db.execute(
                "INSERT OR IGNORE INTO reads (key, until) SELECT key, max(ts) FROM \
                 (SELECT key, ts FROM messages UNION ALL SELECT key, ts FROM calls) GROUP BY key",
                [],
            )?;
        }
        // v3 : un SMS supprimé reste en base, masqué, pour qu'un nouveau listage MAP ne le fasse pas revenir.
        let has_hidden: bool = db.query_row("SELECT count(*) FROM pragma_table_info('messages') WHERE name = 'hidden'", [], |r| r.get(0))?;
        if !has_hidden {
            db.execute("ALTER TABLE messages ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0", [])?;
        }
        db.pragma_update(None, "user_version", 3)?;
        db.execute("INSERT OR IGNORE INTO meta (k, v) VALUES ('since', ?1)", [stamp::now()])?;
        let since = db.query_row("SELECT v FROM meta WHERE k = 'since'", [], |r| r.get(0))?;
        Ok(Self { db, since })
    }

    /// Mise en service de l'historique (format MAP) : les SMS antérieurs sont ignorés.
    pub fn since(&self) -> &str {
        &self.since
    }

    /// `true` si le SMS est nouveau pour l'historique (à publier). Un envoyé qui revient du dossier
    /// `sent` remplace l'envoi consigné localement (horodatage du téléphone) sans être republié.
    ///
    /// # Errors
    /// Erreur SQLite.
    pub fn add_sms(&self, sms: &Sms, sent: bool) -> rusqlite::Result<bool> {
        let (number, _) = sms_peer(sms, sent);
        if sent {
            let n = self
                .db
                .prepare_cached(
                    "UPDATE OR IGNORE messages SET ts = ?3, local = 0 WHERE id = \
                     (SELECT id FROM messages WHERE key = ?1 AND text = ?2 AND outgoing = 1 AND local = 1 ORDER BY ts LIMIT 1)",
                )?
                .execute(params![number_key(number), sms.text, sms.timestamp])?;
            if n == 1 {
                return Ok(false);
            }
        }
        // iOS peut décaler d'une seconde l'horodatage d'un même SMS d'un listage à l'autre.
        if self.near_duplicate(number, &sms.timestamp, &sms.text, sent)? {
            return Ok(false);
        }
        self.insert_msg(number, &sms.timestamp, &sms.text, sent, false)
    }

    /// Masqués compris : un SMS supprimé compte comme déjà consigné, même purgé (texte vide).
    fn near_duplicate(&self, number: &str, ts: &str, text: &str, outgoing: bool) -> rusqlite::Result<bool> {
        Ok(self.near(number, ts, text, outgoing, SAME_SMS_SECS, "")? || self.near(number, ts, "", outgoing, SAME_SMS_SECS, " AND hidden = 1")?)
    }

    /// Un SMS de même correspondant, sens et texte à `secs` près ; `extra` : condition SQL supplémentaire (constante).
    fn near(&self, number: &str, ts: &str, text: &str, outgoing: bool, secs: i64, extra: &str) -> rusqlite::Result<bool> {
        let Some(t) = stamp::secs(ts) else { return Ok(false) };
        let mut st = self.db.prepare_cached(&format!("SELECT ts FROM messages WHERE key = ?1 AND text = ?2 AND outgoing = ?3{extra}"))?;
        let found = st
            .query_map(params![number_key(number), text, outgoing], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok().and_then(|s| stamp::secs(&s)))
            .any(|u| (u - t).abs() <= secs);
        Ok(found)
    }

    /// Envoi fait depuis l'app (horodatage du PC), en attente de son retour par le dossier `sent`.
    /// Rien n'est consigné si ce retour est déjà arrivé (il précède parfois la fin du transfert).
    ///
    /// # Errors
    /// Erreur SQLite.
    pub fn add_local_sent(&self, number: &str, ts: &str, text: &str) -> rusqlite::Result<bool> {
        if self.near(number, ts, text, true, SENT_ECHO_SECS, " AND local = 0 AND hidden = 0")? {
            return Ok(false);
        }
        self.insert_msg(number, ts, text, true, true)
    }

    /// Masque un SMS (`Some((ts, texte, sortant))`) ou retire tout l'historique d'une conversation ;
    /// un appel (`call`) est effacé. Renvoie le nombre de lignes touchées.
    ///
    /// # Errors
    /// Erreur SQLite.
    pub fn delete(&self, key: &str, item: Option<&crate::engine::Item>) -> rusqlite::Result<usize> {
        let n = match item {
            None => {
                self.db.execute("UPDATE messages SET hidden = 1 WHERE key = ?1", [key])?
                    + self.db.execute("DELETE FROM calls WHERE key = ?1", [key])?
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

    /// Efface les SMS et appels antérieurs à `before` (horodatage MAP) ; `since` avance jusque-là pour
    /// que les SMS restés sur l'iPhone ne reviennent pas au prochain listage. Renvoie le nombre de lignes.
    ///
    /// # Errors
    /// Erreur SQLite.
    pub fn purge_before(&mut self, before: &str) -> rusqlite::Result<usize> {
        let tx = self.db.transaction()?;
        let n = tx.execute("DELETE FROM messages WHERE ts < ?1", [before])? + tx.execute("DELETE FROM calls WHERE ts < ?1", [before])?;
        tx.execute("UPDATE meta SET v = max(v, ?1) WHERE k = 'since'", [before])?;
        let since: String = tx.query_row("SELECT v FROM meta WHERE k = 'since'", [], |r| r.get(0))?;
        tx.commit()?;
        self.since = since;
        Ok(n)
    }

    /// Efface le texte des SMS masqués (supprimés depuis l'app). La ligne reste, vide, comme témoin : un SMS
    /// encore sur l'iPhone ne revient pas (`near_duplicate`) ; antérieure à `since`, elle disparaît.
    ///
    /// # Errors
    /// Erreur SQLite.
    pub fn purge_hidden(&self) -> rusqlite::Result<usize> {
        let n = self.db.execute("UPDATE OR REPLACE messages SET text = '' WHERE hidden = 1 AND text <> ''", [])?;
        self.db.execute("DELETE FROM messages WHERE hidden = 1 AND ts < ?1", [&self.since])?;
        Ok(n)
    }

    /// Vide l'historique (SMS, appels, lectures) ; le répertoire est gardé. `since` passe à maintenant :
    /// les SMS restés sur l'iPhone ne reviennent pas.
    ///
    /// # Errors
    /// Erreur SQLite.
    pub fn clear(&mut self) -> rusqlite::Result<()> {
        let now = stamp::now();
        let tx = self.db.transaction()?;
        tx.execute_batch("DELETE FROM messages; DELETE FROM calls; DELETE FROM reads;")?;
        tx.execute("UPDATE meta SET v = ?1 WHERE k = 'since'", [&now])?;
        tx.commit()?;
        self.since = now;
        Ok(())
    }

    fn insert_msg(&self, number: &str, ts: &str, text: &str, outgoing: bool, local: bool) -> rusqlite::Result<bool> {
        let n = self
            .db
            .prepare_cached("INSERT OR IGNORE INTO messages (key, number, ts, text, outgoing, local) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
            .execute(params![number_key(number), number, ts, text, outgoing, local])?;
        Ok(n == 1)
    }

    /// Consigne un appel (`secs` : durée de communication).
    ///
    /// # Errors
    /// Erreur SQLite.
    pub fn add_call(&self, number: &str, ts: &str, kind: CallKind, secs: u64) -> rusqlite::Result<()> {
        self.db
            .prepare_cached("INSERT INTO calls (key, number, ts, kind, secs) VALUES (?1, ?2, ?3, ?4, ?5)")?
            .execute(params![number_key(number), number, ts, kind.as_str(), i64::try_from(secs).unwrap_or(i64::MAX)])?;
        Ok(())
    }

    /// Conversation lue jusqu'à `until` (ne recule jamais).
    ///
    /// # Errors
    /// Erreur SQLite.
    pub fn mark_read(&self, key: &str, until: &str) -> rusqlite::Result<()> {
        self.db
            .prepare_cached("INSERT INTO reads (key, until) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET until = max(until, excluded.until)")?
            .execute(params![key, until])?;
        Ok(())
    }

    /// Remplace le répertoire ; renvoie les paires (numéro, nom) publiées aux clients.
    ///
    /// # Errors
    /// Erreur SQLite (la transaction est annulée).
    pub fn replace_contacts(&mut self, contacts: &[(String, String)]) -> rusqlite::Result<Vec<(String, String)>> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM contacts", [])?;
        {
            let mut ins = tx.prepare("INSERT OR IGNORE INTO contacts (key, number, name) VALUES (?1, ?2, ?3)")?;
            for (number, name) in contacts {
                ins.execute(params![number_key(number), number, name])?;
            }
        }
        tx.commit()?;
        self.contacts()
    }

    fn contacts(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let mut st = self.db.prepare("SELECT number, name FROM contacts")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
    }

    /// Ce que le démon rejoue au démarrage : répertoire, les `limit` derniers SMS et appels, puis les lectures.
    ///
    /// # Errors
    /// Erreur SQLite.
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
            // Non lu par défaut : la table `reads` (rejouée ensuite) fixe ce qui a été lu.
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
        assert!(!h.add_sms(&sms("06 12 34 56 78", "20261015T100000", "a"), false).unwrap(), "même message, autre écriture du numéro");
        assert!(!h.add_sms(&sms("+33612345678", "20261015T095959", "a"), false).unwrap(), "horodatage décalé d'une seconde");
        assert!(h.add_sms(&sms("+33612345678", "20261015T100100", "a"), false).unwrap(), "même texte une minute plus tard : nouveau");
        assert!(h.add_local_sent("0612345678", "20261015T090000", "b").unwrap());
        // Retour par le dossier `sent` : remplace l'envoi local, pas de doublon.
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
        // Lecture : ne recule pas, rejouée en fin de chargement.
        h.mark_read("612345678", "20261015T120000").unwrap();
        h.mark_read("612345678", "20261015T100000").unwrap();
        assert_eq!(h.load(10).unwrap().last(), Some(&Out::Read { key: "612345678".into(), until: "20261015T120000".into() }));
        // Rejeu dans un état neuf : non lu jusqu'à la lecture consignée, seul le postérieur le reste.
        h.add_sms(&sms("+33612345678", "20261015T130000", "c"), false).unwrap();
        let mut st = crate::app::state::State::default();
        h.load(10).unwrap().into_iter().for_each(|o| drop(st.apply(o)));
        assert_eq!(st.conv("612345678").map(|c| c.unread), Some(1));
        // Retour par `sent` arrivé avant la fin du transfert : l'envoi local n'est pas consigné en plus.
        assert!(h.add_sms(&Sms { recipient_address: "0612345678".into(), timestamp: "20261015T140001".into(), text: "d".into(), ..Default::default() }, true).unwrap());
        assert!(!h.add_local_sent("+33612345678", "20261015T140000", "d").unwrap());
        assert!(h.add_local_sent("+33612345678", "20261015T150000", "d").unwrap(), "renvoi plus tard : consigné");
        // Suppression : masqué, ne revient pas au relistage MAP.
        let item = crate::engine::Item { timestamp: "20261015T130000".into(), text: "c".into(), outgoing: false, call: false };
        assert_eq!(h.delete("612345678", Some(&item)).unwrap(), 1);
        assert!(!h.add_sms(&sms("+33612345678", "20261015T130000", "c"), false).unwrap());
        let call = crate::engine::Item { timestamp: "20261015T110000".into(), text: String::new(), outgoing: false, call: true };
        assert_eq!(h.delete("612345678", Some(&call)).unwrap(), 1);
        assert!(!h.load(10).unwrap().iter().any(|o| matches!(o, Out::CallLog { .. }) || matches!(o, Out::Sms { sms: Sms { text, .. }, .. } if text == "c")));
        h.delete("612345678", None).unwrap();
        assert!(!h.load(10).unwrap().iter().any(|o| matches!(o, Out::Sms { .. })));
        // Limite : seuls les plus récents.
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
        // Le SMS périmé encore sur l'iPhone ne revient pas : `since` a avancé.
        assert_eq!(h.since(), "20261010T000000");
        let item = crate::engine::Item { timestamp: "20261015T100000".into(), text: "new".into(), outgoing: false, call: false };
        h.delete("612345678", Some(&item)).unwrap();
        assert_eq!(h.purge_hidden().unwrap(), 1);
        assert_eq!(h.purge_hidden().unwrap(), 0);
        // Texte effacé, mais le témoin empêche le retour du SMS supprimé.
        assert!(!h.add_sms(&sms("+33612345678", "20261015T100001", "new"), false).unwrap());
        let text: String = h.db.query_row("SELECT text FROM messages WHERE hidden = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(text, "");
        h.replace_contacts(&[("+33612345678".into(), "Bob".into())]).unwrap();
        assert!(h.add_sms(&sms("+33612345678", "20261015T120000", "x"), false).unwrap());
        h.mark_read("612345678", "20261015T100000").unwrap();
        h.clear().unwrap();
        assert!(h.since() > "20261015T100000");
        let v = h.load(10).unwrap();
        assert!(matches!(v.as_slice(), [Out::Contacts { .. }]), "répertoire gardé, le reste vidé : {v:?}");
    }
}
