//! Traduction de l'interface. Les textes source sont en anglais dans le code (`t!("…")`) ; la langue
//! vient de la locale du système (`LC_ALL` > `LC_MESSAGES` > `LANG`, ou `BLUEZ_MESSAGE_LANG` pour forcer).
//! Fichiers `<lang>.json` (`{ "English source": "Traduction" }`) : ceux embarqués (`locales/`), puis ceux
//! de l'utilisateur dans `$XDG_CONFIG_HOME/bluez-message/locales/` qui les complètent ou les remplacent.
//! Sans fichier pour la langue : anglais. Voir `locales/README.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{PoisonError, RwLock};

/// Traductions livrées avec les binaires.
const EMBEDDED: &[(&str, &str)] = &[("fr", include_str!("../locales/fr.json"))];

/// Gabarit : toutes les clés (valeurs vides), tenu à jour par les tests.
pub const TEMPLATE: &str = include_str!("../locales/template.json");

type Table = HashMap<String, String>;

/// Table en vigueur. Remplaçable à chaud ([`set_lang`]) : l'ancienne reste en mémoire (quelques Ko par
/// changement de langue) pour que les `&'static str` déjà rendus restent valides.
static TABLE: RwLock<Option<&'static Table>> = RwLock::new(None);

/// Traduction de `s` dans la langue choisie, ou `s` lui-même.
pub fn tr(s: &'static str) -> &'static str {
    table().get(s).map_or(s, String::as_str)
}

fn table() -> &'static Table {
    if let Some(t) = *TABLE.read().unwrap_or_else(PoisonError::into_inner) {
        return t;
    }
    // Premier appel : langue du réglage enregistré, sinon du système.
    TABLE.write().unwrap_or_else(PoisonError::into_inner).get_or_insert_with(|| Box::leak(Box::new(load(stored_lang()))))
}

/// Change la langue (`None` : celle du système) ; `BLUEZ_MESSAGE_LANG` garde la priorité.
/// Chaque langue n'est chargée (et laissée en mémoire) qu'une fois.
pub fn set_lang(lang: Option<&str>) {
    static LOADED: std::sync::Mutex<BTreeMap<Option<String>, &'static Table>> = std::sync::Mutex::new(BTreeMap::new());
    let key = lang.filter(|l| crate::settings::valid_lang(l)).map(str::to_owned);
    let t = *LOADED.lock().unwrap_or_else(PoisonError::into_inner).entry(key.clone()).or_insert_with(|| Box::leak(Box::new(load(key))));
    *TABLE.write().unwrap_or_else(PoisonError::into_inner) = Some(t);
}

/// Langue du réglage enregistré, lue sans passer par `t!` (appelée pendant l'initialisation de la table).
fn stored_lang() -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Only {
        lang: Option<String>,
    }
    let p = crate::config::config_dir()?.join("bluez-message/settings.json");
    let only: Only = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    only.lang.filter(|l| crate::settings::valid_lang(l))
}

/// Langues disponibles : anglais, traductions embarquées et fichiers de l'utilisateur.
pub fn available() -> Vec<String> {
    let mut out = BTreeSet::from(["en".to_owned()]);
    out.extend(EMBEDDED.iter().map(|(l, _)| (*l).to_owned()));
    if let Some(Ok(rd)) = user_dir().map(std::fs::read_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "json")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && crate::settings::valid_lang(stem)
            {
                out.insert(stem.to_owned());
            }
        }
    }
    out.into_iter().collect()
}

/// Nom d'une langue dans cette langue ; à défaut, son code.
#[must_use]
pub fn native_name(code: &str) -> &str {
    match code.split(['_', '-']).next().unwrap_or(code) {
        "en" => "English",
        "fr" => "Français",
        "de" => "Deutsch",
        "es" => "Español",
        "it" => "Italiano",
        "pt" => "Português",
        "nl" => "Nederlands",
        "pl" => "Polski",
        "ru" => "Русский",
        "uk" => "Українська",
        "ja" => "日本語",
        "zh" => "中文",
        "ar" => "العربية",
        _ => code,
    }
}

/// Remplace `{0}`, `{1}`… de `pattern` par `args`, en un seul passage : une valeur insérée (corps de
/// SMS, nom) contenant `{1}` n'est jamais réinterprétée.
pub fn fill(pattern: &str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut rest = pattern;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if let Some((a, len)) = marker(rest).and_then(|(n, len)| args.get(n).map(|a| (a, len))) {
            out.push_str(&a.to_string());
            rest = &rest[len..];
        } else {
            out.push('{');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Marqueur `{n}` (chiffres ASCII seulement) en tête de `s` : numéro et longueur.
fn marker(s: &str) -> Option<(usize, usize)> {
    let body = s.strip_prefix('{')?;
    let end = body.find(|c: char| !c.is_ascii_digit())?;
    (end > 0 && body[end..].starts_with('}')).then(|| body[..end].parse().ok().map(|n| (n, end + 2)))?
}

/// `t!("Text")` traduit ; `t!("Sent to {0}", who)` traduit puis remplace les marqueurs.
#[macro_export]
macro_rules! t {
    ($s:literal) => {
        $crate::i18n::tr($s)
    };
    ($s:literal, $($a:expr),+ $(,)?) => {
        $crate::i18n::fill($crate::i18n::tr($s), &[$(&$a as &dyn ::std::fmt::Display),+])
    };
}

/// Codes à essayer, du plus général au plus précis : `pt_BR.UTF-8@euro` → `["pt", "pt_BR"]`.
/// `BLUEZ_MESSAGE_LANG`, puis `pref` (réglage), puis la locale du système.
fn langs(pref: Option<String>) -> Vec<String> {
    let env = |v: &str| std::env::var(v).ok().filter(|s| !s.is_empty());
    let raw = env("BLUEZ_MESSAGE_LANG")
        .or(pref)
        .or_else(|| ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|v| env(v)))
        .unwrap_or_default();
    let code = raw.split(['.', '@']).next().unwrap_or_default();
    // Nom de fichier : lettres, chiffres, `_` et `-` seulement (pas de traversée de chemin).
    if code.is_empty() || code == "C" || code == "POSIX" || !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return vec![];
    }
    let base = code.split(['_', '-']).next().unwrap_or(code).to_owned();
    if base == code { vec![base] } else { vec![base, code.to_owned()] }
}

fn user_dir() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("bluez-message/locales"))
}

fn load(pref: Option<String>) -> Table {
    let mut table = HashMap::new();
    for lang in langs(pref) {
        if let Some((_, json)) = EMBEDDED.iter().find(|(l, _)| *l == lang) {
            merge(&mut table, json, &format!("embedded {lang}"));
        }
        if let Some(path) = user_dir().map(|d| d.join(format!("{lang}.json"))) {
            match std::fs::read_to_string(&path) {
                Ok(json) => merge(&mut table, &json, &path.display().to_string()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %path.display(), "translation file unreadable: {e}"),
            }
        }
    }
    table
}

/// Ajoute les traductions valides de `json` : valeur vide ignorée (repli), marqueurs `{n}` différents de
/// la source rejetés (le texte affiché perdrait ou inventerait une valeur).
fn merge(table: &mut HashMap<String, String>, json: &str, origin: &str) {
    let map: BTreeMap<String, String> = match serde_json::from_str(json) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(origin, "translation file ignored (invalid JSON): {e}");
            return;
        }
    };
    for (k, v) in map {
        if v.is_empty() {
            continue;
        }
        if markers(&k) != markers(&v) {
            tracing::warn!(origin, key = %k, "translation ignored: {{n}} markers differ from the source");
            continue;
        }
        table.insert(k, v);
    }
}

/// Marqueurs `{0}`, `{1}`… présents dans `s`.
#[must_use]
pub fn markers(s: &str) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        rest = &rest[i..];
        if let Some((n, _)) = marker(rest) {
            out.insert(n);
        }
        rest = &rest[1..];
    }
    out
}

/// Clés du gabarit.
#[must_use]
pub fn template_keys() -> BTreeSet<String> {
    serde_json::from_str::<BTreeMap<String, String>>(TEMPLATE).map(|m| m.into_keys().collect()).unwrap_or_default()
}

/// Contrôle d'un fichier de traduction : clés manquantes, inconnues, marqueurs incohérents.
///
/// # Errors
/// Message traduit si le JSON est illisible.
pub fn check(json: &str) -> Result<Vec<String>, String> {
    let map: BTreeMap<String, String> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let keys = template_keys();
    let mut issues = vec![];
    for k in &keys {
        if map.get(k).is_none_or(String::is_empty) {
            issues.push(crate::t!("missing: {0}", k));
        }
    }
    for (k, v) in &map {
        if !keys.contains(k) {
            issues.push(crate::t!("unknown key: {0}", k));
        } else if !v.is_empty() && markers(k) != markers(v) {
            issues.push(crate::t!("{n} markers differ: {0}", k));
        }
    }
    Ok(issues)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_replaces_markers() {
        assert_eq!(fill("{1}/{0} {2}", &[&"a", &2, &"c"]), "2/a c");
        assert_eq!(fill("none", &[&1]), "none");
        assert_eq!(fill("{0} {1}", &[&"{1}", &"x"]), "{1} x");
        assert_eq!(fill("{0 {+1} {9} {}", &[&"a", &"b"]), "{0 {+1} {9} {}");
    }

    #[test]
    fn markers_found() {
        assert_eq!(markers("x {0} {1} {name} {+3} {2"), BTreeSet::from([0, 1]));
        assert_eq!(markers("{{0}}"), BTreeSet::from([0]));
    }

    #[test]
    fn merge_rejects_bad_markers_and_empty() {
        let mut t = HashMap::new();
        merge(&mut t, r#"{"A {0}": "B", "C": "", "D {0}": "E {0}"}"#, "test");
        assert_eq!(t.len(), 1);
        assert_eq!(t["D {0}"], "E {0}");
        merge(&mut t, "pas du json", "test");
        assert_eq!(t.len(), 1);
    }

    /// Littéraux `t!("…")` du code source.
    fn source_keys() -> BTreeSet<String> {
        fn walk(dir: &std::path::Path, out: &mut BTreeSet<String>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("i18n.rs") {
                    let src = std::fs::read_to_string(&p).unwrap();
                    let mut rest = src.as_str();
                    while let Some(i) = rest.find("t!(\"") {
                        // `print!(`, `assert!(`… ne sont pas `t!(`.
                        let ident = rest[..i].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_');
                        rest = &rest[i + 4..];
                        if ident {
                            continue;
                        }
                        let end = rest.find('"').unwrap();
                        let key = &rest[..end];
                        assert!(!key.ends_with('\\'), "pas d'échappement dans une clé : {}", p.display());
                        out.insert(key.to_owned());
                        rest = &rest[end..];
                    }
                }
            }
        }
        let mut out = BTreeSet::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
        // Clés de ce module (contrôle d'un fichier).
        out.extend(["missing: {0}", "unknown key: {0}", "{n} markers differ: {0}"].map(String::from));
        out
    }

    #[test]
    fn template_matches_sources() {
        let src = source_keys();
        let tpl = template_keys();
        let missing: Vec<_> = src.difference(&tpl).collect();
        let orphan: Vec<_> = tpl.difference(&src).collect();
        assert!(missing.is_empty() && orphan.is_empty(), "template.json : manquantes {missing:?}, orphelines {orphan:?}");
        let map: BTreeMap<String, String> = serde_json::from_str(TEMPLATE).unwrap();
        assert!(map.values().all(String::is_empty), "template.json : valeurs vides");
    }

    #[test]
    fn embedded_translations_are_complete() {
        for (lang, json) in EMBEDDED {
            let issues = check(json).unwrap();
            assert!(issues.is_empty(), "{lang}.json : {issues:?}");
        }
    }
}
