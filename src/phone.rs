//! Numbers and correspondents: correspondent of an SMS, search by name or number.

use std::ops::Range;

use crate::map::Sms;

/// Number and name (MAP) of an SMS correspondent.
#[must_use]
pub fn sms_peer(sms: &Sms, sent: bool) -> (&str, &str) {
    if sent { (pick(&sms.recipient_address, &sms.recipient), &sms.recipient) } else { (pick(&sms.address, &sms.sender), &sms.sender) }
}

/// Number without its formatting separators (spaces, `-`, `.`, parentheses).
#[must_use]
pub fn strip_separators(n: &str) -> String {
    n.chars().filter(|c| !matches!(c, ' ' | '-' | '.' | '(' | ')')).collect()
}

/// Lowercase without accents, to compare names.
pub(crate) fn fold(s: &str) -> String {
    s.chars().flat_map(fold_char).collect()
}

/// [`fold`] of one character (lowercase may give several).
fn fold_char(c: char) -> impl Iterator<Item = char> {
    c.to_lowercase().map(|c| match c {
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
}

/// One line of `text` around the first match of `query` (case and accents ignored, like the history
/// search), with at most `around` characters on each side, and the byte range of the match in it.
/// Without a match: the start of the text, no range.
#[must_use]
pub fn excerpt(text: &str, query: &str, around: usize) -> (String, Option<Range<usize>>) {
    let chars: Vec<char> = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    // Folded characters, each with the index of the character it comes from.
    let folded: Vec<(char, usize)> = chars.iter().enumerate().flat_map(|(i, c)| fold_char(*c).map(move |f| (f, i))).collect();
    let q: Vec<char> = fold(query.trim()).chars().collect();
    let hit = (!q.is_empty())
        .then(|| folded.windows(q.len()).position(|w| w.iter().map(|(c, _)| c).eq(q.iter())))
        .flatten()
        .map(|i| (folded[i].1, folded[i + q.len() - 1].1 + 1));
    let (from, to) = hit.unwrap_or((0, 0));
    let start = from.saturating_sub(around);
    let end = (to + around).max(start + 2 * around).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut range = None;
    for (i, c) in chars.iter().enumerate().take(end).skip(start) {
        if hit.is_some() && i == from {
            range = Some(out.len()..out.len());
        }
        out.push(*c);
        if let Some(r) = range.as_mut().filter(|_| i + 1 == to) {
            r.end = out.len();
        }
    }
    if end < chars.len() {
        out.push('…');
    }
    (out, range)
}

/// Search decoded once: folded name fragment, or digits if the input is a number.
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

    /// `folded`: name already passed through `fold`.
    pub(crate) fn hits(&self, folded: &str, number: &str) -> bool {
        match &self.digits {
            None => folded.contains(&self.name),
            Some(qd) if qd.is_empty() => self.name.is_empty(),
            Some(qd) => {
                // "06 12…" also finds "+33 6 12…": national form rebuilt from the last 9 digits.
                let nd = digits(number);
                nd.contains(qd.as_str()) || (nd.len() >= 9 && format!("0{}", &nd[nd.len() - 9..]).contains(qd.as_str()))
            }
        }
    }
}

fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// `q` matches this correspondent: name fragment (case and accents ignored) or, if `q` is a number, run of
/// digits of the number in its international or national form (`+33 6…` as well as `06…`).
#[must_use]
pub fn matches(q: &str, name: &str, number: &str) -> bool {
    Query::new(q).hits(&fold(name), number)
}

/// Text coming from the phone or the network (sender, number, SMS, contact): removes control
/// characters (except `\n` if `multiline`), bidi direction marks and invisible characters, which
/// allow a displayed name or number to be disguised. U+2028/2029 separators become `\n` in
/// multiline. Kept on purpose (format characters with a visible role): ZWJ U+200D (composed emojis),
/// ZWNJ U+200C (Persian, Indic scripts) and tags U+E0020-E007F (subdivision flags).
#[must_use]
pub fn clean(s: &str, multiline: bool) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\u{2028}' | '\u{2029}' => multiline.then_some('\n'),
            '\n' if multiline => Some(c),
            '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}' => None,
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
        assert_eq!(clean("Al\u{00AD}i\u{2062}ce\u{206A}\u{FFF9}\u{180E}", false), "Alice");
        assert_eq!(clean("\u{1F3F4}\u{E0067}\u{E007F} a\u{200C}b", false), "\u{1F3F4}\u{E0067}\u{E007F} a\u{200C}b");
        assert_eq!(clean("a\r\nb\tc", false), "abc");
        assert_eq!(clean("a\r\nb", true), "a\nb");
        assert_eq!(clean("a\u{2028}b\u{2029}c", true), "a\nb\nc");
        assert_eq!(clean("\u{FEFF}B\u{200B}o\u{2060}b\u{2028}", false), "Bob");
        // ZWJ of composed emojis kept.
        assert_eq!(clean("👨\u{200D}👩", true), "👨\u{200D}👩");
    }

    #[test]
    fn excerpt_marks_the_folded_match() {
        let (e, r) = excerpt("Un BEL ÉTÉ\nà Noël", "ete", 4);
        assert_eq!(e, "…BEL ÉTÉ à N…");
        assert_eq!(&e[r.unwrap()], "ÉTÉ");
        // Lowercase longer than the character (İ → i̇): range still on whole characters.
        let (e, r) = excerpt("İstanbul", "stan", 10);
        assert_eq!(&e[r.unwrap()], "stan");
        let (e, r) = excerpt("abc", "zz", 10);
        assert_eq!((e.as_str(), r), ("abc", None));
        assert_eq!(excerpt("abcdef", "", 1), ("ab…".to_owned(), None));
    }
}
