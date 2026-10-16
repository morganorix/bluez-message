//! Horodatages MAP (`AAAAMMJJThhmmss`, heure locale) : production, validation et conversion.

/// Heure locale au format MAP, pour placer un envoi dans le fil.
#[must_use]
pub fn now() -> String {
    ago(0)
}

/// Heure locale d'il y a `secs` secondes, au format MAP.
#[must_use]
pub fn ago(secs: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let t = now.saturating_sub(secs) as libc::time_t;
    // SAFETY: `tm` est un tampon local initialisé à zéro ; localtime_r est réentrant et n'écrit que dans `tm`.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&raw const t, &raw mut tm);
        tm
    };
    format!("{:04}{:02}{:02}T{:02}{:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Horodatage MAP `AAAAMMJJThhmmss`, suivi éventuellement d'un fuseau (`Z`, `+hhmm`).
#[must_use]
pub fn is_valid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 15
        && b.len() <= 20
        && b[8] == b'T'
        && b.iter().enumerate().all(|(i, c)| i == 8 || c.is_ascii_digit() || (i >= 15 && matches!(c, b'Z' | b'+' | b'-')))
}

/// Horodatage MAP `AAAAMMJJTHHMMSS` (suffixe éventuel ignoré) → secondes depuis 1970 (calendrier civil, sans fuseau).
#[must_use]
pub fn secs(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 15 || b[8] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r).filter(|x| x.bytes().all(|c| c.is_ascii_digit())).and_then(|x| x.parse::<i64>().ok());
    let (y, m, d) = (n(0..4)?, n(4..6)?, n(6..8)?);
    let (hh, mm, ss) = (n(9..11)?, n(11..13)?, n(13..15)?);
    // Jours depuis 1970 (algorithme « days from civil » de H. Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_format() {
        let s = now();
        assert_eq!(s.len(), 15);
        assert_eq!(&s[8..9], "T");
    }

    #[test]
    fn stamp_seconds() {
        assert_eq!(secs("19700101T000000"), Some(0));
        assert_eq!(secs("20261015T152754"), Some(1_792_078_074));
        assert_eq!(secs("20240301T000000").zip(secs("20240229T235959")).map(|(a, b)| a - b), Some(1));
        assert_eq!(secs("2026101515:2754"), None);
        assert_eq!(secs("é"), None);
    }
}
