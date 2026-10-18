//! Link previews (title, description, thumbnail) fetched by the GUI only, when `link_previews` is on:
//! a single background thread, https only, bounded body and image, public addresses only (no request
//! to the local network, even after a redirect). Neither the URL nor the address is logged, at most
//! the domain in debug.

use std::collections::{HashMap, VecDeque};
use std::io::{Cursor, Read};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::Duration;

use eframe::egui;
use image::{ImageReader, Limits};
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use ureq::{Agent, ResponseExt};

use crate::phone;

/// Pending requests; beyond that, a link is asked again on a later frame.
const QUEUE: usize = 32;
/// Previews kept (failures included), oldest dropped first.
const CACHE: usize = 64;
/// Whole request, redirects included.
const TIMEOUT: Duration = Duration::from_secs(5);
const REDIRECTS: u32 = 3;
/// Read of the page, stopped earlier at `</head>`.
const HTML_MAX: u64 = 512 * 1024;
const IMAGE_MAX: u64 = 2 * 1024 * 1024;
/// Decoded image: side and memory.
const IMAGE_SIDE: u32 = 4096;
const IMAGE_ALLOC: u64 = 64 * 1024 * 1024;
/// Thumbnail side, in pixels.
pub const THUMB: u32 = 96;
const TITLE_MAX: usize = 200;
const DESCRIPTION_MAX: usize = 300;
/// Generic: the site learns neither the application nor its version.
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64)";

/// Preview shown under a bubble.
pub struct Card {
    pub title: String,
    pub description: String,
    pub domain: String,
    pub image: Option<egui::TextureHandle>,
}

/// Result of the thread; the texture is created on the GUI thread.
struct Fetched {
    meta: Meta,
    domain: String,
    image: Option<egui::ColorImage>,
}

enum Entry {
    Pending,
    Ready(Option<Card>),
}

#[derive(Debug, thiserror::Error)]
enum Fail {
    // Fixed texts: ureq errors may carry the URL, kept out of the logs.
    #[error("request failed")]
    Http(#[from] ureq::Error),
    #[error("read failed")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Image(#[from] image::ImageError),
    #[error("not an HTML page")]
    NotHtml,
    #[error("not an image")]
    NotImage,
    #[error("no title")]
    NoTitle,
}

/// Requests to the fetching thread (URLs) and its results (URL, card data or failure).
type Worker = (SyncSender<String>, Receiver<(String, Option<Fetched>)>);

/// Previews of the links seen, with their fetching thread (started on first need).
#[derive(Default)]
pub struct Previews {
    worker: Option<Worker>,
    cache: HashMap<String, Entry>,
    order: VecDeque<String>,
}

impl Previews {
    /// Preview of `url` if ready; otherwise asks for it (once) and returns `None`, also on failure.
    pub fn get(&mut self, ctx: &egui::Context, url: &str) -> Option<&Card> {
        self.receive(ctx);
        if !self.cache.contains_key(url) {
            self.ask(ctx, url);
        }
        match self.cache.get(url) {
            Some(Entry::Ready(card)) => card.as_ref(),
            _ => None,
        }
    }

    fn receive(&mut self, ctx: &egui::Context) {
        let Some((_, rx)) = &self.worker else { return };
        let done: Vec<_> = rx.try_iter().collect();
        for (url, f) in done {
            let card = f.map(|f| Card {
                title: f.meta.title,
                description: f.meta.description,
                domain: f.domain,
                image: f.image.map(|img| ctx.load_texture("link-preview", img, egui::TextureOptions::LINEAR)),
            });
            self.insert(url, Entry::Ready(card));
        }
    }

    fn ask(&mut self, ctx: &egui::Context, url: &str) {
        if !https(url) {
            self.insert(url.to_owned(), Entry::Ready(None));
            return;
        }
        if self.worker.is_none() {
            self.worker = spawn(ctx.clone());
        }
        let Some((tx, _)) = &self.worker else {
            self.insert(url.to_owned(), Entry::Ready(None));
            return;
        };
        match tx.try_send(url.to_owned()) {
            Ok(()) => self.insert(url.to_owned(), Entry::Pending),
            // Queue full: asked again on a later frame.
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => self.insert(url.to_owned(), Entry::Ready(None)),
        }
    }

    fn insert(&mut self, url: String, entry: Entry) {
        if let Some(e) = self.cache.get_mut(&url) {
            *e = entry;
            return;
        }
        if self.order.len() >= CACHE
            && let Some(old) = self.order.pop_front()
        {
            self.cache.remove(&old);
        }
        self.order.push_back(url.clone());
        self.cache.insert(url, entry);
    }
}

fn spawn(ctx: egui::Context) -> Option<Worker> {
    let (tx, jobs) = mpsc::sync_channel::<String>(QUEUE);
    let (done, rx) = mpsc::channel();
    let started = std::thread::Builder::new().name("link-previews".into()).spawn(move || {
        let agent = agent();
        for url in jobs {
            if done.send((url.clone(), fetch(&agent, &url))).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    });
    match started {
        Ok(_) => Some((tx, rx)),
        Err(e) => {
            tracing::warn!("link previews: thread not started: {e}");
            None
        }
    }
}

fn https(url: &str) -> bool {
    url.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://"))
}

/// Host of `url` without `www.`, for display and logs.
fn domain(url: &str) -> Option<String> {
    let host = url.parse::<Uri>().ok()?.host()?.to_ascii_lowercase();
    Some(host.strip_prefix("www.").map(str::to_owned).unwrap_or(host))
}

fn agent() -> Agent {
    let config = Agent::config_builder()
        .https_only(true)
        .max_redirects(REDIRECTS)
        .timeout_global(Some(TIMEOUT))
        // The default reads the `*_PROXY` variables: requests go straight to the site.
        .proxy(None)
        .user_agent(USER_AGENT)
        .build();
    Agent::with_parts(config, DefaultConnector::default(), SafeResolver::default())
}

fn fetch(agent: &Agent, url: &str) -> Option<Fetched> {
    let domain = domain(url)?;
    match page(agent, url) {
        Ok((meta, image)) => Some(Fetched { meta, domain, image }),
        Err(e) => {
            tracing::debug!(%domain, "link preview: {e}");
            None
        }
    }
}

/// Meta data of the page, then its image (a failing image leaves the preview without one).
fn page(agent: &Agent, url: &str) -> Result<(Meta, Option<egui::ColorImage>), Fail> {
    let mut resp = agent.get(url).call()?;
    let html = resp.body().mime_type().is_some_and(|m| m == "text/html" || m == "application/xhtml+xml");
    if !html {
        return Err(Fail::NotHtml);
    }
    let base = resp.get_uri().clone();
    let head = read_head(resp.body_mut().as_reader().take(HTML_MAX))?;
    let meta = parse(&head);
    if meta.title.is_empty() {
        return Err(Fail::NoTitle);
    }
    let image = meta.image.as_deref().and_then(|i| absolute(&base, i)).and_then(|i| match picture(agent, &i) {
        Ok(img) => Some(img),
        Err(e) => {
            tracing::debug!("link preview image: {e}");
            None
        }
    });
    Ok((meta, image))
}

fn picture(agent: &Agent, url: &str) -> Result<egui::ColorImage, Fail> {
    let mut resp = agent.get(url).call()?;
    if !resp.body().mime_type().is_some_and(|m| m.starts_with("image/")) {
        return Err(Fail::NotImage);
    }
    thumb(&resp.body_mut().with_config().limit(IMAGE_MAX).read_to_vec()?)
}

/// Start of the page up to `</head>` (included) or the end of `r`.
fn read_head(mut r: impl Read) -> std::io::Result<String> {
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 8192]);
    loop {
        let n = r.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        // Tag possibly split between two reads.
        let from = buf.len().saturating_sub(6);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf[from..].windows(7).position(|w| w.eq_ignore_ascii_case(b"</head>")) {
            buf.truncate(from + i + 7);
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Decodes a bounded image and reduces it to [`THUMB`] pixels at most.
fn thumb(bytes: &[u8]) -> Result<egui::ColorImage, Fail> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(IMAGE_SIDE);
    limits.max_image_height = Some(IMAGE_SIDE);
    limits.max_alloc = Some(IMAGE_ALLOC);
    reader.limits(limits);
    let img = reader.decode()?.thumbnail(THUMB, THUMB).to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
}

/// What a page says about itself.
#[derive(Debug, Default, PartialEq, Eq)]
struct Meta {
    title: String,
    description: String,
    image: Option<String>,
}

/// `og:*` tags, falling back on `<title>` and `<meta name="description">`.
fn parse(head: &str) -> Meta {
    let Ok(dom) = tl::parse(head, tl::ParserOptions::default()) else { return Meta::default() };
    let parser = dom.parser();
    let (mut og, mut plain) = (Meta::default(), Meta::default());
    // Tag names in any case (`query_selector` only matches lower case).
    let tags = || dom.nodes().iter().filter_map(tl::Node::as_tag);
    let named = |t: &tl::HTMLTag, n: &str| t.name().as_bytes().eq_ignore_ascii_case(n.as_bytes());
    for tag in tags().filter(|t| named(t, "meta")) {
        let attr = |k: &str| tag.attributes().get(k).flatten().map(|b| b.as_utf8_str().into_owned());
        let Some(content) = attr("content") else { continue };
        match attr("property").or_else(|| attr("name")).map(|k| k.to_ascii_lowercase()).as_deref() {
            Some("og:title") => og.title = content,
            Some("og:description") => og.description = content,
            Some("og:image") => og.image = Some(content),
            Some("description") => plain.description = content,
            _ => {}
        }
    }
    if let Some(t) = tags().find(|t| named(t, "title")) {
        plain.title = text(t, parser);
    }
    let pick = |a: String, b: String| if a.trim().is_empty() { b } else { a };
    Meta {
        title: tidy(&pick(og.title, plain.title), TITLE_MAX),
        description: tidy(&pick(og.description, plain.description), DESCRIPTION_MAX),
        image: og.image.map(|i| unescape(i.trim())),
    }
}

/// Text inside `tag`, walked with an explicit stack: `tl`'s recursive `inner_text` could overflow
/// the thread stack on a deeply nested page.
fn text(tag: &tl::HTMLTag, parser: &tl::Parser) -> String {
    let mut out = String::new();
    // Children pushed in reverse order so that they pop in document order.
    let push = |stack: &mut Vec<tl::NodeHandle>, t: &tl::HTMLTag| {
        let start = stack.len();
        stack.extend(t.children().top().iter().copied());
        stack[start..].reverse();
    };
    let mut stack = vec![];
    push(&mut stack, tag);
    while let Some(node) = stack.pop().and_then(|h| h.get(parser)) {
        match node {
            tl::Node::Raw(r) => out.push_str(&r.as_utf8_str()),
            tl::Node::Tag(t) => push(&mut stack, t),
            tl::Node::Comment(_) => {}
        }
        if out.len() > 4 * TITLE_MAX {
            break;
        }
    }
    out
}

/// Entities decoded, invisible characters removed, spaces collapsed, at most `max` characters.
fn tidy(s: &str, max: usize) -> String {
    let s = phone::clean(&unescape(s), false);
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let mut cut: String = s.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

/// Common HTML entities; any other one stays as is.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        // `;` is ASCII: the byte position is a character boundary.
        let decoded = rest.bytes().skip(1).take(10).position(|b| b == b';').and_then(|end| {
            let name = &rest[1..=end];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .map(|h| u32::from_str_radix(h, 16))
                    .or_else(|| name.strip_prefix('#').map(str::parse))
                    .and_then(Result::ok)
                    .and_then(char::from_u32),
            };
            c.map(|c| (c, end + 2))
        });
        if let Some((c, len)) = decoded {
            out.push(c);
            rest = &rest[len..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// `og:image` made absolute against the page (after redirects); https only.
fn absolute(base: &Uri, image: &str) -> Option<String> {
    if https(image) {
        return Some(image.to_owned());
    }
    if let Some(rest) = image.strip_prefix("//") {
        return Some(format!("https://{rest}"));
    }
    if image.starts_with('/') && base.scheme_str() == Some("https") {
        return Some(format!("https://{}{image}", base.authority()?));
    }
    None
}

/// System resolver keeping public addresses only; literal IP hosts refused.
#[derive(Debug, Default)]
struct SafeResolver(DefaultResolver);

impl Resolver for SafeResolver {
    fn resolve(&self, uri: &Uri, config: &Config, timeout: NextTimeout) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let host = uri.host().unwrap_or_default();
        if host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok() {
            return Err(ureq::Error::HostNotFound);
        }
        let mut out = self.empty();
        for a in self.0.resolve(uri, config, timeout)?.iter().filter(|a| public(a.ip())) {
            out.push(*a);
        }
        if out.is_empty() { Err(ureq::Error::HostNotFound) } else { Ok(out) }
    }
}

/// Globally routable address (neither local network, nor reserved, nor documentation, nor multicast).
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_v4(v4);
            }
            let s = v6.segments();
            // 2000::/3 only, without documentation, 6to4 and Teredo.
            s[0] & 0xe000 == 0x2000 && !(s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] == 0)) && s[0] != 0x2002
        }
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    let reserved = a == 0 || a >= 240 || (a == 100 && b & 0xc0 == 64) || (a == 192 && b == 0 && c == 0) || (a == 198 && b & 0xfe == 18);
    !(reserved || ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_broadcast() || ip.is_documentation() || ip.is_multicast())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_open_graph_then_falls_back() {
        let og = &format!(
            r#"<html><head><title>Plain</title>
            <meta property="og:title" content="Caf&eacute; &amp; th&#233; &#x2014; ok">
            <meta property="og:description" content="  A   {}page ">
            <meta property="og:image" content="/img/a.png?x=1&amp;y=2">
            </head><body>ignored</body></html>"#,
            '\u{202E}'
        );
        assert_eq!(
            parse(og),
            Meta { title: "Caf&eacute; & thé — ok".into(), description: "A page".into(), image: Some("/img/a.png?x=1&y=2".into()) }
        );
        let plain = r#"<head><TITLE> Only
            title </TITLE><meta name="Description" content="Desc"></head>"#;
        assert_eq!(parse(plain), Meta { title: "Only title".into(), description: "Desc".into(), image: None });
        assert_eq!(parse("not html at all"), Meta::default());
    }

    #[test]
    fn bounds_the_texts() {
        let long = "é".repeat(TITLE_MAX + 50);
        let t = tidy(&long, TITLE_MAX);
        assert_eq!(t.chars().count(), TITLE_MAX);
        assert!(t.ends_with('…'));
        assert_eq!(tidy("short", TITLE_MAX), "short");
        assert_eq!(unescape("a & b &unknown; &#0; &#xZZ; &amp"), "a & b &unknown; \0 &#xZZ; &amp");
    }

    #[test]
    fn nested_title_without_recursion() {
        assert_eq!(parse("<head><title>a<b>b<!-- c --></b>c</title></head>").title, "abc");
        // Deeper than a recursive walk survives on a 2 MiB thread stack, within HTML_MAX.
        let n = 70_000;
        let deep = format!("<head><title>x{}y{}</title></head>", "<b>".repeat(n), "</b>".repeat(n));
        assert!(deep.len() < HTML_MAX as usize);
        assert_eq!(parse(&deep).title, "xy");
    }

    #[test]
    fn head_reading_stops_at_end_of_head() {
        let page = format!("<head><title>x</title></HEAD>{}", "<p>body</p>".repeat(100_000));
        assert_eq!(read_head(page.as_bytes()).unwrap(), "<head><title>x</title></HEAD>");
        // Split across two 8 KiB reads.
        let split = format!("{}</head>tail", " ".repeat(8190));
        assert!(read_head(split.as_bytes()).unwrap().ends_with("</head>"));
        let big = "a".repeat(2 * HTML_MAX as usize);
        assert_eq!(read_head(big.as_bytes().take(HTML_MAX)).unwrap().len(), HTML_MAX as usize);
    }

    #[test]
    fn only_https_images() {
        let base: Uri = "https://example.com/a/b".parse().unwrap();
        assert_eq!(absolute(&base, "https://cdn.example.com/i.png").as_deref(), Some("https://cdn.example.com/i.png"));
        assert_eq!(absolute(&base, "//cdn.example.com/i.png").as_deref(), Some("https://cdn.example.com/i.png"));
        assert_eq!(absolute(&base, "/i.png").as_deref(), Some("https://example.com/i.png"));
        for bad in ["http://example.com/i.png", "i.png", "data:image/png;base64,AAAA", "javascript:x"] {
            assert_eq!(absolute(&base, bad), None, "{bad}");
        }
    }

    #[test]
    fn local_addresses_refused() {
        for ip in [
            "127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "192.0.0.8",
            "198.18.0.1", "240.0.0.1", "255.255.255.255", "224.0.0.1", "192.0.2.1", "::1", "::", "fe80::1", "fc00::1", "fd12::1",
            "::ffff:127.0.0.1", "::ffff:10.0.0.1", "2001:db8::1", "2002:a00:1::", "2001::1", "ff02::1", "64:ff9b::a00:1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["93.184.215.14", "1.1.1.1", "2606:4700::1111", "::ffff:93.184.215.14"] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn literal_ip_hosts_refused() {
        let agent = agent();
        let r = SafeResolver::default();
        let never = NextTimeout { after: ureq::unversioned::transport::time::Duration::NotHappening, reason: ureq::Timeout::Global };
        for url in ["https://127.0.0.1/", "https://[::1]:8443/", "https://93.184.215.14/"] {
            let uri: Uri = url.parse().unwrap();
            assert!(matches!(r.resolve(&uri, agent.config(), never), Err(ureq::Error::HostNotFound)), "{url}");
        }
    }

    #[test]
    fn images_bounded() {
        let encode = |w: u32, h: u32| {
            let mut out = Cursor::new(Vec::new());
            image::RgbaImage::new(w, h).write_to(&mut out, image::ImageFormat::Png).unwrap();
            out.into_inner()
        };
        let t = thumb(&encode(400, 200)).unwrap();
        assert_eq!(t.size, [THUMB as usize, THUMB as usize / 2]);
        assert!(thumb(&encode(IMAGE_SIDE + 1, 1)).is_err());
        assert!(thumb(b"<svg xmlns='http://www.w3.org/2000/svg'/>").is_err());
        assert!(thumb(&[0x89, b'P', b'N', b'G', 0, 1, 2]).is_err());
    }

    #[test]
    fn non_https_never_fetched() {
        let ctx = egui::Context::default();
        let mut p = Previews::default();
        for url in ["http://example.com/", "ftp://example.com/"] {
            assert!(p.get(&ctx, url).is_none());
        }
        assert!(p.worker.is_none());
        assert_eq!(domain("https://www.Example.com/a").as_deref(), Some("example.com"));
    }

    #[test]
    fn cache_is_bounded() {
        let mut p = Previews::default();
        for i in 0..CACHE + 10 {
            p.insert(format!("https://{i}.example/"), Entry::Ready(None));
        }
        assert_eq!((p.cache.len(), p.order.len()), (CACHE, CACHE));
        assert!(!p.cache.contains_key("https://0.example/"));
        p.insert(format!("https://{}.example/", CACHE + 9), Entry::Pending);
        assert_eq!(p.order.len(), CACHE);
    }
}
