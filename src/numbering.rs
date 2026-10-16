//! Phone numbers through libphonenumber's metadata (crate `phonenumber`): grouping key (E.164) and
//! landline detection, so that numbers unable to receive an SMS stay out of the SMS list. National
//! numbers are read with the home country; a number with its calling code does not depend on it.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

pub use phonenumber::country::Id;
use phonenumber::metadata::DATABASE;
use phonenumber::{Mode, Type};

/// Entries kept by the analysis cache before it is emptied.
const CACHE_MAX: usize = 4096;

/// Country of ISO 3166 code `code` (`fr`, case-insensitive).
#[must_use]
pub fn parse_code(code: &str) -> Option<Id> {
    Id::from_str(&code.to_ascii_uppercase()).ok()
}

/// ISO 3166 code, lowercase (`fr`), as stored in the settings.
#[must_use]
pub fn code(id: Id) -> String {
    id.as_ref().to_ascii_lowercase()
}

/// Countries known to the metadata, sorted by code, with their label (`FR · +33`).
pub fn countries() -> &'static [(Id, String)] {
    static ALL: OnceLock<Vec<(Id, String)>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut v: Vec<(Id, String)> =
            DATABASE.iter().filter_map(|m| Some((Id::from_str(m.id()).ok()?, format!("{} · +{}", m.id(), m.country_code())))).collect();
        v.sort_by(|a, b| a.0.as_ref().cmp(b.0.as_ref()));
        v.dedup_by_key(|c| c.0);
        v
    })
}

/// Label of a country (`FR · +33`).
#[must_use]
pub fn label(id: Id) -> &'static str {
    countries().iter().find(|c| c.0 == id).map_or("", |c| c.1.as_str())
}

/// Home country: `pref` (setting), else the one of the interface language `lang` when it differs
/// from the system's (`de` → `de`), else the territory of the system locale
/// (`LC_ALL` > `LC_TELEPHONE` > `LANG`: `fr_BE.UTF-8` → `be`), else its language (`fr` → `fr`).
#[must_use]
pub fn home(pref: Option<&str>, lang: Option<&str>) -> Option<Id> {
    if let Some(p) = pref {
        return parse_code(p);
    }
    let env = |v: &str| std::env::var(v).ok().filter(|s| !s.is_empty());
    let sys = ["LC_ALL", "LC_TELEPHONE", "LANG"].iter().find_map(|v| env(v));
    resolve(sys.as_deref(), lang)
}

/// [`home`] without a setting: `sys` system locale, `lang` interface language.
fn resolve(sys: Option<&str>, lang: Option<&str>) -> Option<Id> {
    let local = sys.and_then(from_locale);
    match lang {
        // Language that is not a country code (`en`): the system's territory still applies.
        Some(l) if !sys.and_then(language).is_some_and(|s| s.eq_ignore_ascii_case(l)) => from_locale(l).or(local),
        _ => local,
    }
}

/// Language of a locale (`fr_BE.UTF-8` → `fr`).
fn language(raw: &str) -> Option<&str> {
    raw.split(['.', '@']).next()?.split(['_', '-']).next()
}

fn from_locale(raw: &str) -> Option<Id> {
    let code = raw.split(['.', '@']).next()?;
    let mut parts = code.split(['_', '-']);
    let lang = parts.next()?;
    match parts.next() {
        Some(territory) => parse_code(territory),
        None => parse_code(lang),
    }
}

/// What the analysis of a number yields.
#[derive(Clone)]
struct Facts {
    key: String,
    landline: bool,
}

/// Analyses by number and home country.
type Cache = Mutex<HashMap<(String, Option<Id>), Facts>>;

fn facts(number: &str, home: Option<Id>) -> Facts {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    let k = (number.to_owned(), home);
    if let Some(f) = cache.lock().ok().and_then(|c| c.get(&k).cloned()) {
        return f;
    }
    let f = analyse(number, home);
    if let Ok(mut c) = cache.lock() {
        if c.len() >= CACHE_MAX {
            c.clear();
        }
        c.insert(k, f.clone());
    }
    f
}

fn analyse(number: &str, home: Option<Id>) -> Facts {
    // Alphanumeric senders: the crate would read their letters as a keypad.
    let parsed = if number.chars().any(char::is_alphabetic) { None } else { phonenumber::parse(home, number).ok().filter(phonenumber::is_valid) };
    match parsed {
        // Country unknown: national numbers unreadable, so every key stays on the fallback to keep
        // `06…` and `+33 6…` together.
        Some(n) => Facts {
            key: if home.is_some() { n.format().mode(Mode::E164).to_string() } else { fallback_key(number) },
            landline: n.number_type(&DATABASE) == Type::FixedLine,
        },
        None => Facts { key: fallback_key(number), landline: false },
    }
}

/// Key of a number the metadata cannot validate: last 9 digits, else the text as is.
fn fallback_key(n: &str) -> String {
    let digits: String = n.chars().filter(char::is_ascii_digit).collect();
    if digits.len() >= 9 { digits[digits.len() - 9..].to_owned() } else if digits.is_empty() { n.trim().to_owned() } else { digits }
}

/// Grouping key: E.164 (`+33389000000`) for a valid number (national ones read with `home`),
/// else the last 9 digits (short codes, unknown ranges, `home` unknown), else the text (alphanumeric senders).
#[must_use]
pub fn key(number: &str, home: Option<Id>) -> String {
    facts(number, home).key
}

/// `true` only when `number` is surely a landline (`FixedLine`): mobiles, ranges shared between
/// both, internet telephony, short codes, alphanumeric senders and invalid numbers count as able to text.
#[must_use]
pub fn is_landline(number: &str, home: Option<Id>) -> bool {
    facts(number, home).landline
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landlines_and_mobiles() {
        let fr = Some(Id::FR);
        assert!(is_landline("03 89 00 00 00", fr));
        assert!(is_landline("+33 3 89 00 00 00", None));
        assert!(is_landline("0033389000000", fr));
        assert!(!is_landline("06 00 00 00 00", fr));
        assert!(!is_landline("+33 7 00 00 00 00", fr));
        // Internet telephony, shared ranges, short codes, alphanumeric senders: never a landline.
        assert!(!is_landline("09 70 00 00 00", fr));
        assert!(!is_landline("+1 202 555 0100", fr));
        assert!(!is_landline("36 31", fr));
        assert!(!is_landline("ExampleBank", fr));
        assert!(!is_landline("", fr));
        assert!(!is_landline("03 89 00 00 00", None));
        // The calling code wins over the home country.
        assert!(!is_landline("+32 470 00 00 00", fr));
        assert!(is_landline("0470 00 00 00", fr), "national number: read as French");
    }

    #[test]
    fn keys() {
        let fr = Some(Id::FR);
        assert_eq!(key("06 00 00 00 00", fr), "+33600000000");
        assert_eq!(key("+33 6 00 00 00 00", Some(Id::BE)), "+33600000000");
        assert_eq!(key("0033 6 00 00 00 00", fr), "+33600000000");
        assert_eq!(key("03.89.00.00.00", fr), "+33389000000");
        assert_eq!(key("+32 470 00 00 00", fr), "+32470000000");
        assert_eq!(key("0470 00 00 00", Some(Id::BE)), "+32470000000");
        // Not validated: fallback.
        assert_eq!(key("36 31", fr), "3631");
        assert_eq!(key(" ExampleBank ", fr), "ExampleBank");
        assert_eq!(key("06 00 00 00 00", None), "600000000");
        assert_eq!(key("+33 6 00 00 00 00", None), "600000000", "country unknown: fallback for all");
    }

    #[test]
    fn country_list() {
        let all = countries();
        assert!(all.len() > 200);
        assert!(all.windows(2).all(|w| w[0].0.as_ref() < w[1].0.as_ref()), "sorted, no duplicate");
        assert_eq!(label(Id::FR), "FR · +33");
        assert_eq!(parse_code("be"), Some(Id::BE));
        assert_eq!(parse_code("001"), None);
        assert_eq!(parse_code("xx"), None);
        assert_eq!(code(Id::FR), "fr");
    }

    #[test]
    fn home_from_locale() {
        assert_eq!(from_locale("fr_BE.UTF-8"), Some(Id::BE));
        assert_eq!(from_locale("de"), Some(Id::DE));
        assert_eq!(from_locale("en_US.UTF-8"), Some(Id::US));
        assert_eq!(from_locale("C"), None);
        assert_eq!(home(Some("it"), Some("de")), Some(Id::IT));
        assert_eq!(resolve(Some("fr_BE.UTF-8"), None), Some(Id::BE));
        assert_eq!(resolve(Some("fr_BE.UTF-8"), Some("fr")), Some(Id::BE), "same language: the system's territory");
        assert_eq!(resolve(Some("fr_BE.UTF-8"), Some("de")), Some(Id::DE), "the chosen language decides");
        assert_eq!(resolve(Some("fr_FR.UTF-8"), Some("en")), Some(Id::FR), "language that is not a country");
        assert_eq!(resolve(None, Some("it")), Some(Id::IT));
        assert_eq!(resolve(None, None), None);
    }
}
