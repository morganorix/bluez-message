//! Numéros et correspondants : clé de regroupement, correspondant d'un SMS, recherche par nom ou numéro.

use crate::map::Sms;

/// Numéro et nom (MAP) du correspondant d'un SMS.
#[must_use]
pub fn sms_peer(sms: &Sms, sent: bool) -> (&str, &str) {
    if sent { (pick(&sms.recipient_address, &sms.recipient), &sms.recipient) } else { (pick(&sms.address, &sms.sender), &sms.sender) }
}

/// Normalise un numéro pour regrouper « +33 7 12… » et « 07 12… » : 9 derniers chiffres.
pub fn number_key(n: &str) -> String {
    let digits: String = n.chars().filter(char::is_ascii_digit).collect();
    if digits.len() >= 9 { digits[digits.len() - 9..].to_owned() } else if digits.is_empty() { n.trim().to_owned() } else { digits }
}

/// Minuscules sans accents, pour comparer des noms.
pub(crate) fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'â' | 'ä' | 'á' | 'ã' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' | 'í' => 'i',
            'ô' | 'ö' | 'ó' | 'õ' => 'o',
            'ù' | 'û' | 'ü' | 'ú' => 'u',
            'ç' => 'c',
            'ÿ' => 'y',
            'ñ' => 'n',
            c => c,
        })
        .collect()
}

/// Recherche décodée une fois : morceau de nom replié, ou chiffres si la saisie est un numéro.
pub(crate) struct Query {
    name: String,
    digits: Option<String>,
}

impl Query {
    pub(crate) fn new(q: &str) -> Self {
        let q = q.trim();
        let numeric = q.chars().all(|c| c.is_ascii_digit() || " +-.()".contains(c));
        Self { name: fold(q), digits: numeric.then(|| digits(q)) }
    }

    /// `folded` : nom déjà passé par `fold`.
    pub(crate) fn hits(&self, folded: &str, number: &str) -> bool {
        match &self.digits {
            None => folded.contains(&self.name),
            Some(qd) if qd.is_empty() => self.name.is_empty(),
            Some(qd) => {
                let nd = digits(number);
                nd.contains(qd.as_str()) || (nd.len() >= 9 && format!("0{}", number_key(number)).contains(qd.as_str()))
            }
        }
    }
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// `q` trouve ce correspondant : morceau du nom (casse et accents ignorés) ou, si `q` est un numéro, suite de
/// chiffres du numéro sous sa forme internationale ou nationale (`+33 6…` comme `06…`).
#[must_use]
pub fn matches(q: &str, name: &str, number: &str) -> bool {
    Query::new(q).hits(&fold(name), number)
}

/// Texte venu du téléphone ou du réseau (expéditeur, numéro, SMS, contact) : retire les caractères de
/// contrôle (sauf `\n` si `multiline`), les marques de direction bidi et les caractères invisibles, qui
/// permettent de maquiller un nom ou un numéro affiché. Les séparateurs U+2028/2029 deviennent `\n` en
/// multiligne. Le ZWJ (U+200D) des emojis composés est gardé.
#[must_use]
pub fn clean(s: &str, multiline: bool) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\u{2028}' | '\u{2029}' => multiline.then_some('\n'),
            '\n' if multiline => Some(c),
            '\u{061C}' | '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}' => None,
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

fn pick<'a>(a: &'a str, b: &'a str) -> &'a str {
    if a.is_empty() { b } else { a }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_controls_and_bidi() {
        assert_eq!(clean("Banque\u{202E}gnp.exe", false), "Banquegnp.exe");
        assert_eq!(clean("+33\u{2066}6\u{200F}12\0", false), "+33612");
        assert_eq!(clean("a\r\nb\tc", false), "abc");
        assert_eq!(clean("a\r\nb", true), "a\nb");
        assert_eq!(clean("a\u{2028}b\u{2029}c", true), "a\nb\nc");
        assert_eq!(clean("\u{FEFF}B\u{200B}o\u{2060}b\u{2028}", false), "Bob");
        // ZWJ des emojis composés gardé.
        assert_eq!(clean("👨\u{200D}👩", true), "👨\u{200D}👩");
    }
}
