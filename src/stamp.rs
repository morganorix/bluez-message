//! MAP timestamps (`YYYYMMDDThhmmss`, local time): production, validation and conversion.

/// Local time in MAP format, to place a sent message in the thread.
#[must_use]
pub fn now() -> String {
    ago(0)
}

/// Local time `secs` seconds ago, in MAP format.
#[must_use]
pub fn ago(secs: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let t = libc::time_t::try_from(now.saturating_sub(secs)).unwrap_or(libc::time_t::MAX);
    // SAFETY: `tm` is a local zero-initialised buffer; localtime_r is reentrant and only writes into `tm`.
    let (tm, ok) = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let ok = !libc::localtime_r(&raw const t, &raw mut tm).is_null();
        (tm, ok)
    };
    // Out-of-range time (never in practice): the epoch, still a valid stamp.
    if !ok {
        return "19700101T000000".to_owned();
    }
    format!("{:04}{:02}{:02}T{:02}{:02}{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// MAP timestamp `YYYYMMDDThhmmss`, optionally followed by a time zone (`Z`, `+hhmm`).
#[must_use]
pub fn is_valid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 15
        && b.len() <= 20
        && b[8] == b'T'
        && b.iter().enumerate().all(|(i, c)| i == 8 || c.is_ascii_digit() || (i >= 15 && matches!(c, b'Z' | b'+' | b'-')))
}

/// MAP timestamp `YYYYMMDDTHHMMSS` (optional suffix ignored) → seconds since 1970 (civil calendar, no time zone).
#[must_use]
pub fn secs(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 15 || b[8] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r).filter(|x| x.bytes().all(|c| c.is_ascii_digit())).and_then(|x| x.parse::<i64>().ok());
    let (y, m, d) = (n(0..4)?, n(4..6)?, n(6..8)?);
    let (hh, mm, ss) = (n(9..11)?, n(11..13)?, n(13..15)?);
    // Seconds up to 60 (leap second).
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days since 1970 (H. Hinnant's "days from civil" algorithm).
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
        for bad in ["20261315T000000", "20261000T000000", "20261032T000000", "20261015T240000", "20261015T236000", "20261015T235961"] {
            assert_eq!(secs(bad), None, "{bad}");
        }
    }
}
