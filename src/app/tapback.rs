//! Réactions (tapbacks) : iOS ne les transmet pas par MAP. Elles circulent en texte (« A aimé « … » »,
//! « Liked “…” ») ; on les reconnaît pour les afficher en badge sur le message visé, comme Messages.

use super::state::{Delivery, Msg};
use crate::t;

/// Emoji et verbes iOS (français, anglais). L'envoi emploie l'anglais : Google Messages ne convertit
/// en emoji que le format iOS anglais (`Loved “…”`) ; un iPhone en face reçoit ce texte tel quel.
pub const KINDS: [(&str, &[&str]); 6] = [
    ("❤", &["A adoré", "Loved"]),
    ("👍", &["A aimé", "Liked"]),
    ("👎", &["N'a pas aimé", "Disliked"]),
    ("😂", &["A ri de", "Laughed at"]),
    ("‼", &["A souligné", "Emphasized"]),
    ("❓", &["A posé une question sur", "Questioned"]),
];

/// Libellé de la réaction dans la langue de l'interface.
#[must_use]
pub fn label(emoji: &str) -> &'static str {
    match emoji {
        "❤" => t!("Loved"),
        "👍" => t!("Liked"),
        "👎" => t!("Disliked"),
        "😂" => t!("Laughed at"),
        "‼" => t!("Emphasized"),
        _ => t!("Questioned"),
    }
}

/// Extrait cité au plus (en caractères), au-delà terminé par `…`.
const QUOTE_MAX: usize = 200;

/// Texte d'envoi de la réaction `emoji` à `text`.
#[must_use]
pub fn format(emoji: &str, text: &str) -> Option<String> {
    let verb = KINDS.iter().find(|(e, _)| *e == emoji)?.1[1];
    let mut quote: String = text.chars().take(QUOTE_MAX).collect();
    if quote.len() < text.len() {
        quote.push('…');
    }
    Some(format!("{verb} “{quote}”"))
}

/// Réaction lue dans un texte : emoji, extrait cité, extrait tronqué (`…` final).
#[must_use]
pub fn parse(text: &str) -> Option<(&'static str, &str, bool)> {
    let text = text.trim();
    let (emoji, rest) = KINDS.iter().find_map(|(e, verbs)| {
        verbs.iter().find_map(|v| text.strip_prefix(v).and_then(|r| r.strip_prefix([' ', '\u{a0}'])).map(|r| (*e, r)))
    })?;
    let quote = [('«', '»'), ('“', '”'), ('"', '"')]
        .iter()
        .find_map(|(o, c)| rest.strip_prefix(*o).and_then(|r| r.strip_suffix(*c)))?
        .trim_matches([' ', '\u{a0}', '\u{202f}']);
    let (quote, cut) = match quote.strip_suffix('…') {
        Some(q) => (q.trim_end(), true),
        None => (quote, false),
    };
    (!quote.is_empty()).then_some((emoji, quote, cut))
}

/// Badge d'un message : emoji, réaction envoyée par moi, encore en cours d'envoi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub emoji: &'static str,
    pub mine: bool,
    pub pending: bool,
}

/// Rattachement des réactions d'un fil : `badges[i]` sur le message `i`, `folded[i]` si le message `i`
/// est une réaction affichée en badge (donc pas en bulle).
#[derive(Debug, Default)]
pub struct Fold {
    pub badges: Vec<Vec<Badge>>,
    pub folded: Vec<bool>,
}

/// Chaque réaction se rattache au dernier message antérieur cité (texte égal, ou préfixe si l'extrait
/// est tronqué) ; une réaction en échec ou sans cible reste une bulle. Comme Messages, une nouvelle
/// réaction d'un même auteur remplace la précédente.
#[must_use]
pub fn fold(msgs: &[Msg]) -> Fold {
    let mut f = Fold { badges: vec![vec![]; msgs.len()], folded: vec![false; msgs.len()] };
    for (i, m) in msgs.iter().enumerate() {
        if m.call.is_some() || m.delivery == Delivery::Failed {
            continue;
        }
        let Some((emoji, quote, cut)) = parse(&m.text) else { continue };
        let target = msgs[..i].iter().rposition(|t| {
            t.call.is_none() && parse(&t.text).is_none() && if cut { t.text.starts_with(quote) } else { t.text.trim() == quote }
        });
        let Some(t) = target else { continue };
        f.folded[i] = true;
        let b = &mut f.badges[t];
        b.retain(|x| x.mine != m.outgoing);
        b.push(Badge { emoji, mine: m.outgoing, pending: m.delivery == Delivery::Pending });
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str, outgoing: bool) -> Msg {
        Msg { text: text.into(), timestamp: "20261015T100000".into(), outgoing, ..Msg::default() }
    }

    #[test]
    fn parse_fr_en_and_truncation() {
        assert_eq!(parse("A adoré « Coucou toucou »"), Some(("❤", "Coucou toucou", false)));
        assert_eq!(parse("A aimé «\u{a0}Coucou\u{a0}»"), Some(("👍", "Coucou", false)));
        assert_eq!(parse("Laughed at “haha”"), Some(("😂", "haha", false)));
        assert_eq!(parse("A souligné « Long texte… »"), Some(("‼", "Long texte", true)));
        assert_eq!(parse("A aimé ce film"), None);
        assert_eq!(parse("A aimé « »"), None);
    }

    #[test]
    fn format_round_trips() {
        let long = "x".repeat(250);
        let t = format("❓", &long).unwrap();
        let (e, q, cut) = parse(&t).unwrap();
        assert_eq!((e, q.len(), cut), ("❓", 200, true));
        assert_eq!(format("❤", "Coucou").as_deref(), Some("Loved “Coucou”"));
        assert_eq!(format("🙂", "a"), None);
    }

    #[test]
    fn fold_attaches_replaces_and_keeps_orphans() {
        let msgs = vec![
            msg("Coucou toucou", false),
            msg("A aimé « Coucou toucou »", true),
            msg("A adoré « Coucou toucou »", true),
            msg("Loved “Coucou toucou”", false),
            msg("A aimé « inconnu »", false),
        ];
        let f = fold(&msgs);
        assert_eq!(f.folded, [false, true, true, true, false]);
        assert_eq!(
            f.badges[0],
            [Badge { emoji: "❤", mine: true, pending: false }, Badge { emoji: "❤", mine: false, pending: false }]
        );
    }

    #[test]
    fn failed_reaction_stays_a_bubble() {
        let mut r = msg("A aimé « a »", true);
        r.delivery = Delivery::Failed;
        let f = fold(&[msg("a", false), r]);
        assert_eq!(f.folded, [false, false]);
        assert!(f.badges[0].is_empty());
    }
}
