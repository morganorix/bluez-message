//! Répertoire de l'iPhone par PBAP (obexd, bus session) : noms et numéros, pour afficher un nom
//! à la place du numéro. iOS demande l'autorisation à la première lecture.

use std::collections::HashMap;
use std::time::Duration;

use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::addr::BtAddr;
use crate::error::Result;
use crate::obex::Session;
use crate::phone::clean;

/// Bornes du répertoire : l'instantané `Contacts` part sur une seule ligne IPC (16 Mio côté GUI).
/// 20 000 × (128 car. × 4 octets + 64) ≈ 12 Mo au pire.
const MAX_VCF_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CONTACTS: usize = 20_000;
const MAX_NAME: usize = 128;
const MAX_TEL: usize = 64;

#[proxy(interface = "org.bluez.obex.PhonebookAccess1", default_service = "org.bluez.obex")]
trait PhonebookAccess {
    fn select(&self, location: &str, phonebook: &str) -> zbus::Result<()>;
    fn pull_all(&self, targetfile: &str, filters: HashMap<&str, Value<'_>>) -> zbus::Result<(OwnedObjectPath, HashMap<String, OwnedValue>)>;
}

/// Lit le répertoire interne (`int/pb`) : paires (numéro, nom).
///
/// # Errors
/// Session refusée (autorisation iOS), erreur D-Bus ou transfert en échec.
pub async fn pull(addr: &BtAddr) -> Result<Vec<(String, String)>> {
    let session = Session::open(addr, "pbap").await?;
    let r = pull_in(&session).await;
    let _ = session.close().await;
    r
}

async fn pull_in(session: &Session) -> Result<Vec<(String, String)>> {
    let pb = PhonebookAccessProxy::builder(&session.conn).path(session.path.as_ref())?.build().await?;
    pb.select("int", "pb").await?;
    let filters = HashMap::from([("Format", Value::from("vcard30")), ("Fields", Value::from(vec!["FN", "N", "TEL"]))]);
    // Fichier vide : obexd choisit le fichier temporaire et le renvoie dans `Filename`.
    let (transfer, props) = pb.pull_all("", filters).await?;
    let file = crate::bluez::prop_str(&props, "Filename").map(str::to_owned);
    session.wait_transfer(transfer, Duration::from_secs(120)).await?;
    let Some(file) = file else { return Ok(vec![]) };
    let text = read_bounded(&file).map(|b| String::from_utf8_lossy(&b).into_owned());
    let _ = std::fs::remove_file(&file);
    Ok(parse_vcards(&text?))
}

/// Lit au plus `MAX_VCF_BYTES` : au-delà, la fin est ignorée (la dernière fiche tronquée n'a pas d'`END`).
fn read_bounded(file: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut b = Vec::new();
    std::fs::File::open(file)?.take(MAX_VCF_BYTES + 1).read_to_end(&mut b)?;
    if b.len() as u64 > MAX_VCF_BYTES {
        tracing::warn!("PBAP phonebook larger than {MAX_VCF_BYTES} bytes: truncated");
        b.truncate(MAX_VCF_BYTES as usize);
    }
    Ok(b)
}

/// vCard 3.0 minimal : lignes dépliées, `FN` (sinon `N`), chaque `TEL`.
pub fn parse_vcards(src: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let (mut name, mut tels) = (String::new(), Vec::<String>::new());
    let mut lines: Vec<String> = vec![];
    for l in src.lines() {
        let l = l.trim_end_matches('\r');
        match (l.strip_prefix(' ').or_else(|| l.strip_prefix('\t')), lines.last_mut()) {
            (Some(rest), Some(prev)) => prev.push_str(rest),
            _ => lines.push(l.to_owned()),
        }
    }
    for l in &lines {
        let Some((prop, value)) = l.split_once(':') else { continue };
        // `item1.TEL;TYPE=CELL` → `TEL`
        let base = prop.split(';').next().unwrap_or("");
        let base = base.rsplit('.').next().unwrap_or(base).to_ascii_uppercase();
        match base.as_str() {
            "BEGIN" => (name, tels) = (String::new(), vec![]),
            "FN" => name = unescape(value),
            "N" if name.is_empty() => {
                let parts: Vec<String> = value.split(';').map(unescape).collect();
                name = [parts.get(1), parts.first()].into_iter().flatten().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" ");
            }
            "TEL" if !value.trim().is_empty() => tels.push(clean(value.trim(), false).chars().take(MAX_TEL).collect()),
            "END" => {
                let n = clean(name.trim(), false);
                let n: String = n.trim().chars().take(MAX_NAME).collect();
                if !n.is_empty() {
                    out.extend(tels.drain(..).map(|t| (t, n.clone())).take(MAX_CONTACTS - out.len()));
                    if out.len() >= MAX_CONTACTS {
                        tracing::warn!("PBAP phonebook capped at {MAX_CONTACTS} numbers");
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n' | 'N') => o.push(' '),
                Some(x) => o.push(x),
                None => {}
            }
        } else {
            o.push(c);
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cards() {
        let src = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Bob Dupont\r\nTEL;TYPE=CELL:+33 6 12\r\n 34 56 78\r\nitem1.TEL:01 02 03 04 05\r\nEND:VCARD\r\n\
                   BEGIN:VCARD\r\nN:Martin;Alice;;;\r\nTEL:0700000000\r\nEND:VCARD\r\n\
                   BEGIN:VCARD\r\nFN:Sans numéro\r\nEND:VCARD\r\n\
                   BEGIN:VCARD\r\nFN:\r\nTEL:0711111111\r\nEND:VCARD\r\n\
                   BEGIN:VCARD\r\nFN:A\\, B\r\nTEL:0722222222\r\nEND:VCARD\r\n";
        assert_eq!(
            parse_vcards(src),
            [
                ("+33 6 1234 56 78".to_owned(), "Bob Dupont".to_owned()),
                ("01 02 03 04 05".into(), "Bob Dupont".into()),
                ("0700000000".into(), "Alice Martin".into()),
                ("0722222222".into(), "A, B".into()),
            ]
        );
    }

    #[test]
    fn caps_size() {
        let long = "x".repeat(MAX_NAME + 10);
        let card = format!("BEGIN:VCARD\r\nFN:{long}\r\nTEL:{}\r\nEND:VCARD\r\n", "1".repeat(MAX_TEL + 10));
        let v = parse_vcards(&card.repeat(MAX_CONTACTS + 5));
        assert_eq!(v.len(), MAX_CONTACTS);
        assert_eq!(v[0].1.chars().count(), MAX_NAME);
        assert_eq!(v[0].0.len(), MAX_TEL);
    }
}
