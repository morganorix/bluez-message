//! Journalisation commune aux binaires : niveau `info` par défaut (aucun numéro, nom, texte ni
//! adresse), `BLUEZ_MESSAGE_LOG=debug` pour le diagnostic. Les adresses Bluetooth sont masquées à
//! l'écriture, y compris dans les erreurs D-Bus (`/org/bluez/hci0/dev_AA_BB_…`).

use std::io::{self, Write};
use tracing::level_filters::LevelFilter;

/// Variable d'environnement du niveau (`off`, `error`, `warn`, `info`, `debug`, `trace`).
pub const ENV: &str = "BLUEZ_MESSAGE_LOG";

/// Niveau demandé ; `Err` si la valeur est illisible (on retombe alors sur `info`).
fn level(raw: Option<&str>) -> Result<LevelFilter, ()> {
    raw.map_or(Ok(LevelFilter::INFO), |v| v.trim().parse().map_err(|_| ()))
}

/// Remplace chaque adresse `AA:BB:CC:DD:EE:FF` ou `AA_BB_CC_DD_EE_FF` par `XX…`, même séparateur.
fn redact(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if let Some(sep) = mac_at(b, i) {
            out.push_str(&["XX"; 6].join(&char::from(sep).to_string()));
            i += 17;
        } else {
            let ch = s[i..].chars().next().unwrap_or_default();
            out.push(ch);
            i += ch.len_utf8().max(1);
        }
    }
    out
}

/// Séparateur de l'adresse commençant à `i`, si `b[i..i+17]` en est une (bornée par des non-hexa).
fn mac_at(b: &[u8], i: usize) -> Option<u8> {
    let w = b.get(i..i + 17)?;
    let sep = w[2];
    if !(sep == b':' || sep == b'_') || (i > 0 && b[i - 1].is_ascii_hexdigit()) || b.get(i + 17).is_some_and(u8::is_ascii_hexdigit) {
        return None;
    }
    w.chunks(3).all(|c| c[0].is_ascii_hexdigit() && c[1].is_ascii_hexdigit() && c.get(2).is_none_or(|&x| x == sep)).then_some(sep)
}

/// stderr, adresses masquées (une ligne d'événement par `write`).
struct Redacted;

impl Write for Redacted {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::stderr().write_all(redact(&String::from_utf8_lossy(buf)).as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

/// Installe l'abonné `tracing` sur stderr (journal systemd pour le démon).
pub fn init() {
    let raw = std::env::var(ENV).ok();
    let lvl = level(raw.as_deref());
    let _ = tracing_subscriber::fmt().with_writer(|| Redacted).with_max_level(lvl.unwrap_or(LevelFilter::INFO)).try_init();
    if lvl.is_err() {
        tracing::warn!("{ENV}: unreadable value, using info");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_by_default() {
        assert_eq!(level(None), Ok(LevelFilter::INFO));
        assert_eq!(level(Some("verbose")), Err(()));
        assert_eq!(level(Some(" debug ")), Ok(LevelFilter::DEBUG));
        assert_eq!(level(Some("warn")), Ok(LevelFilter::WARN));
    }

    #[test]
    fn addresses_are_masked() {
        assert_eq!(redact("Unknown object '/org/bluez/hci0/dev_AA_BB_CC_DD_EE_0F'"), "Unknown object '/org/bluez/hci0/dev_XX_XX_XX_XX_XX_XX'");
        assert_eq!(redact("device aa:bb:cc:dd:ee:ff gone, é"), "device XX:XX:XX:XX:XX:XX gone, é");
        assert_eq!(redact("0AA:BB:CC:DD:EE:FF"), "0AA:BB:CC:DD:EE:FF");
        assert_eq!(redact("10:11:53.727115Z"), "10:11:53.727115Z");
    }
}
