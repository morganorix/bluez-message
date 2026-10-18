//! Web links in an SMS: only `http(s)://` URLs with their scheme are clickable (no e-mail, no bare
//! domain, no other scheme such as `javascript:` or `ftp:`).

use linkify::{LinkFinder, LinkKind};

/// Piece of a text: plain text or a web link.
#[derive(Debug, PartialEq, Eq)]
pub enum Span<'a> {
    Text(&'a str),
    Link(&'a str),
}

fn web(url: &str) -> bool {
    let lower = |n: usize| url.get(..n).map(str::to_ascii_lowercase);
    lower(7).as_deref() == Some("http://") || lower(8).as_deref() == Some("https://")
}

fn finder() -> LinkFinder {
    let mut f = LinkFinder::new();
    f.kinds(&[LinkKind::Url]).url_must_have_scheme(true);
    f
}

/// Splits `text` into plain text and web links (trailing punctuation stays text).
#[must_use]
pub fn spans(text: &str) -> Vec<Span<'_>> {
    let (mut out, mut start) = (vec![], 0);
    for l in finder().links(text).filter(|l| web(l.as_str())) {
        if l.start() > start {
            out.push(Span::Text(&text[start..l.start()]));
        }
        out.push(Span::Link(l.as_str()));
        start = l.end();
    }
    if start < text.len() {
        out.push(Span::Text(&text[start..]));
    }
    out
}

/// Web links of `text`, in order.
#[must_use]
pub fn links(text: &str) -> Vec<&str> {
    finder().links(text).map(|l| l.as_str()).filter(|u| web(u)).collect()
}

/// `text` contains a web link (cheap test before any split).
#[must_use]
pub fn any(text: &str) -> bool {
    text.contains("://") && !links(text).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_text_and_links() {
        assert_eq!(
            spans("Look: https://example.com/a?b=1, then http://example.org."),
            [Span::Text("Look: "), Span::Link("https://example.com/a?b=1"), Span::Text(", then "), Span::Link("http://example.org"), Span::Text(".")]
        );
        assert_eq!(spans("(see https://example.com/wiki/A_(b))"), [Span::Text("(see "), Span::Link("https://example.com/wiki/A_(b)"), Span::Text(")")]);
        assert_eq!(spans("HTTPS://EXAMPLE.COM"), [Span::Link("HTTPS://EXAMPLE.COM")]);
        assert!(spans("").is_empty());
    }

    #[test]
    fn only_web_links() {
        for t in ["mail me at a@example.com", "example.com", "ftp://example.com/f", "javascript://alert(1)", "file:///etc/passwd", "no link"] {
            assert!(!any(t), "{t}");
            assert_eq!(spans(t), [Span::Text(t)], "{t}");
        }
        assert_eq!(links("a https://a.example b http://b.example"), ["https://a.example", "http://b.example"]);
    }
}
