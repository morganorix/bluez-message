//! iPhone phonebook through PBAP (obexd, session bus): names and numbers, to show a name
//! in place of the number. iOS asks for authorisation on the first read.

use std::collections::HashMap;
use std::time::Duration;

use zbus::{OwnedObjectPath, OwnedVariant, Variant, proxy};

use crate::addr::BtAddr;
use crate::bluez::bounded;
use crate::error::Result;
use crate::obex::{CALL_TIMEOUT, Session};
use crate::phone::clean;

/// Phonebook bounds: the `Contacts` snapshot goes out on a single IPC line (16 MiB on the GUI side).
/// 20,000 × (128 chars × 4 bytes + 64) ≈ 12 MB at worst.
const MAX_VCF_BYTES: usize = 32 * 1024 * 1024;
const MAX_CONTACTS: usize = 20_000;
const MAX_NAME: usize = 128;
const MAX_TEL: usize = 64;
/// Bound of each step of a size probe (session opening, then `Select` + `GetSize`).
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[proxy(interface = "org.bluez.obex.PhonebookAccess1", default_service = "org.bluez.obex")]
trait PhonebookAccess {
    fn select(&self, location: &str, phonebook: &str) -> zbus::Result<()>;
    fn get_size(&self) -> zbus::Result<u16>;
    fn pull_all(&self, targetfile: &str, filters: HashMap<&str, Variant<'_>>) -> zbus::Result<(OwnedObjectPath, HashMap<String, OwnedVariant>)>;
}

/// Internal phonebook read through PBAP.
pub struct Phonebook {
    /// (number, name) pairs.
    pub contacts: Vec<(String, String)>,
    /// Number of entries announced by the iPhone (`GetSize`), `None` if it did not answer.
    pub size: Option<u16>,
}

/// Reads the internal phonebook (`int/pb`).
///
/// # Errors
/// Session refused (iOS authorisation), D-Bus error or failed transfer.
pub async fn pull(addr: &BtAddr) -> Result<Phonebook> {
    let session = Session::open(addr, "pbap").await?;
    let r = pull_in(&session).await;
    let _ = session.close().await;
    r
}

/// Number of entries of the internal phonebook: a cheap change probe (additions and removals only,
/// the iPhone leaves the PBAP version counters at zero).
///
/// # Errors
/// iPhone unreachable, session refused, D-Bus error or timeout.
pub async fn size(addr: &BtAddr) -> Result<u16> {
    let session = bounded(PROBE_TIMEOUT, Session::open(addr, "pbap")).await?;
    let r = bounded(PROBE_TIMEOUT, async {
        let pb = select_pb(&session).await?;
        pb.get_size().await
    })
    .await;
    let _ = session.close().await;
    r
}

async fn select_pb(session: &Session) -> zbus::Result<PhonebookAccessProxy<'_>> {
    let pb = PhonebookAccessProxy::builder(&session.conn).path(session.path.as_ref()).build().await?;
    pb.select("int", "pb").await?;
    Ok(pb)
}

async fn pull_in(session: &Session) -> Result<Phonebook> {
    let (size, (transfer, props)) = bounded(CALL_TIMEOUT, async {
        let pb = select_pb(session).await?;
        let size = pb.get_size().await.ok();
        let filters = HashMap::from([("Format", Variant::from("vcard30")), ("Fields", Variant::from(vec!["FN", "N", "TEL"]))]);
        // Empty file: obexd chooses the temporary file and returns it in `Filename`.
        Ok::<_, zbus::Error>((size, pb.pull_all("", filters).await?))
    })
    .await?;
    let file = crate::bluez::prop_str(&props, "Filename").map(str::to_owned);
    let done = session.wait_transfer(transfer, Duration::from_secs(120)).await;
    let Some(file) = file else { return done.map(|()| Phonebook { contacts: vec![], size }) };
    // The temporary file (phonebook in clear) is removed whatever the outcome of the transfer.
    let text = done.and_then(|()| Ok(read_bounded(&file).map(|b| String::from_utf8_lossy(&b).into_owned())?));
    if let Err(e) = std::fs::remove_file(&file)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("PBAP temporary file not removed: {e}");
    }
    Ok(Phonebook { contacts: parse_vcards(&text?), size })
}

/// Reads `MAX_VCF_BYTES` at most: beyond that, the end is ignored (the last truncated card has no `END`).
fn read_bounded(file: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut b = Vec::new();
    let limit = u64::try_from(MAX_VCF_BYTES).unwrap_or(u64::MAX);
    std::fs::File::open(file)?.take(limit.saturating_add(1)).read_to_end(&mut b)?;
    if b.len() > MAX_VCF_BYTES {
        tracing::warn!("PBAP phonebook larger than {MAX_VCF_BYTES} bytes: truncated");
        b.truncate(MAX_VCF_BYTES);
    }
    Ok(b)
}

/// Minimal vCard 3.0: unfolded lines, `FN` (otherwise `N`), each `TEL`.
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
