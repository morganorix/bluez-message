//! Colour emojis. egui only rasterises monochrome glyphs: each emoji (ZWJ sequence, skin tone,
//! flag, keycap) is drawn from its Twemoji SVG embedded in the binary, no system font is read.
//! An emoji Twemoji lacks is displayed as text.

use std::collections::HashMap;
use std::ops::Range;

use eframe::egui::text::{LayoutJob, TextWrapping};
use eframe::egui::load::{Bytes, SizeHint, SizedTexture, TexturePoll};
use eframe::egui::{self, Color32, FontId, Pos2, TextFormat, TextureFilter, TextureOptions};
use twemoji_assets::svg::SvgTwemojiAsset;
use unicode_properties::{EmojiStatus, UnicodeEmoji};
use unicode_segmentation::UnicodeSegmentation;

use super::link::{self, Span};
use super::theme::font;
use super::widgets as w;

/// Character displayed as an emoji by default (`Emoji_Presentation`), skin tones and regional indicators included.
fn presentation(c: char) -> bool {
    use EmojiStatus as S;
    matches!(
        c.emoji_status(),
        S::EmojiPresentation | S::EmojiPresentationAndModifierBase | S::EmojiPresentationAndEmojiComponent | S::EmojiPresentationAndModifierAndEmojiComponent
    )
}

/// Grapheme cluster (UAX #29) shown as an emoji (UTS #51): an `Emoji` character by default in emoji
/// presentation, or made one by VS16, a keycap or a skin tone; VS15 asks for text (`☀︎`, `#`, `©` stay text).
fn is_emoji(g: &str) -> bool {
    let mut cs = g.chars();
    let Some(first) = cs.next() else { return false };
    first.is_emoji_char() && !g.contains('\u{FE0E}') && (presentation(first) || cs.any(|c| c == '\u{FE0F}' || c == '\u{20E3}' || presentation(c)))
}

/// `text` contains at least one emoji.
fn any(text: &str) -> bool {
    !text.is_ascii() && text.graphemes(true).any(is_emoji)
}

/// Piece of a text: plain text or a run of stuck-together emojis.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    Text(&'a str),
    Emoji(&'a str),
}

/// Splits `text` into text and emoji runs, by grapheme cluster.
#[must_use]
pub fn split(text: &str) -> Vec<Piece<'_>> {
    let piece = |emoji, s| if emoji { Piece::Emoji(s) } else { Piece::Text(s) };
    let (mut out, mut start, mut emoji) = (vec![], 0, false);
    for (i, g) in text.grapheme_indices(true) {
        let e = is_emoji(g);
        if e != emoji && i > start {
            out.push(piece(emoji, &text[start..i]));
            start = i;
        }
        emoji = e;
    }
    if start < text.len() {
        out.push(piece(emoji, &text[start..]));
    }
    out
}

/// Message made of 1 to 3 emojis (and spaces): displayed large, without a bubble, like Messages.
/// A sequence (skin tone, family, flag) counts as one.
#[must_use]
pub fn only(text: &str) -> bool {
    let mut n = 0;
    for g in text.graphemes(true) {
        if is_emoji(g) {
            n += 1;
        } else if !g.chars().all(char::is_whitespace) {
            return false;
        }
    }
    (1..=3).contains(&n)
}

/// Longer emoji run: displayed as text (bounds the work per run).
const MAX_RUN: usize = 32;
/// Resolved runs kept; beyond that, the runs not drawn in the current frame are dropped.
const MAX_RUNS: usize = 1024;
/// Rasterised emojis (64 KB each, kept by egui); beyond that, those not drawn in the current
/// frame are released. Received SMS are arbitrary: memory stays bounded.
const MAX_IMAGES: usize = 512;
/// Rasterisation height: one texture per emoji, scaled down with mipmaps.
const RASTER_H: u32 = 128;

/// Twemoji SVG of an emoji sequence; VS16 is absent from the keys of single characters (`❤`)
/// but present inside ZWJ sequences (`👨🏻‍❤️‍👨🏻`), so both spellings are tried.
fn asset(e: &str) -> Option<&'static SvgTwemojiAsset> {
    SvgTwemojiAsset::from_emoji(e).or_else(|| e.contains('\u{FE0F}').then(|| SvgTwemojiAsset::from_emoji(&e.replace('\u{FE0F}', ""))).flatten())
}

/// Twemoji images of a grapheme: the sequence itself, otherwise each part of a ZWJ sequence
/// that Twemoji does not draw (as a font would); `None` if a part is missing.
fn assets(g: &str, out: &mut Vec<&'static SvgTwemojiAsset>) -> Option<()> {
    if let Some(a) = asset(g) {
        out.push(a);
        return Some(());
    }
    if !g.contains('\u{200D}') {
        return None;
    }
    for part in g.split('\u{200D}').filter(|p| !p.is_empty()) {
        out.push(asset(part)?);
    }
    Some(())
}

/// Bytes URI of `a`, from its code points (`bytes://twemoji/1f44b-1f3fd.svg`).
fn uri(a: &SvgTwemojiAsset) -> String {
    let hex: Vec<String> = a.emoji.chars().map(|c| format!("{:x}", u32::from(c))).collect();
    format!("bytes://twemoji/{}.svg", hex.join("-"))
}

/// URI and SVG of each emoji of a run.
type Svgs = Vec<(String, &'static [u8])>;

/// Twemoji images of emoji runs, rasterised by egui's SVG loader (`theme::apply` installs it).
#[derive(Default)]
pub struct Emojis {
    /// Images of each run (`None`: an emoji has no Twemoji image) and last frame drawn.
    runs: HashMap<String, (Option<Svgs>, u64)>,
    /// URIs rasterised by egui and last frame drawn (see `MAX_IMAGES`).
    loaded: HashMap<String, u64>,
}

/// Textures of `svgs`; `None` (text) while loading, or if this frame alone exceeds `MAX_IMAGES`.
fn textures(loaded: &mut HashMap<String, u64>, ctx: &egui::Context, svgs: &Svgs, frame: u64) -> Option<Vec<SizedTexture>> {
    let opts = TextureOptions { mipmap_mode: Some(TextureFilter::Linear), ..TextureOptions::LINEAR };
    let mut out = Vec::with_capacity(svgs.len());
    for (uri, svg) in svgs {
        if !loaded.contains_key(uri) {
            if loaded.len() >= MAX_IMAGES {
                // Rasterised again if they come back on screen.
                loaded.retain(|u, used| {
                    *used == frame || {
                        ctx.forget_image(u);
                        false
                    }
                });
                if loaded.len() >= MAX_IMAGES {
                    return None;
                }
            }
            // `forget_image` also drops the bytes: given again (the first call wins).
            ctx.include_bytes(uri.clone(), Bytes::Static(svg));
        }
        match ctx.try_load_texture(uri, opts, SizeHint::Height(RASTER_H)) {
            Ok(TexturePoll::Ready { texture }) => {
                match loaded.get_mut(uri) {
                    Some(used) => *used = frame,
                    None => {
                        loaded.insert(uri.clone(), frame);
                    }
                }
                out.push(texture);
            }
            Ok(TexturePoll::Pending { .. }) => {
                ctx.request_repaint();
                return None;
            }
            Err(e) => {
                tracing::debug!("emoji {uri}: {e}");
                return None;
            }
        }
    }
    Some(out)
}

impl Emojis {
    /// Images of the `run` (one per emoji); `None` (text) if an emoji has no Twemoji image,
    /// for a run that is too long or while an image loads.
    pub fn images(&mut self, ctx: &egui::Context, run: &str) -> Option<Vec<SizedTexture>> {
        if run.chars().nth(MAX_RUN).is_some() {
            return None;
        }
        let frame = ctx.cumulative_frame_nr();
        if let Some((svgs, used)) = self.runs.get_mut(run) {
            *used = frame;
            return textures(&mut self.loaded, ctx, svgs.as_ref()?, frame);
        }
        let mut found = vec![];
        let svgs = run
            .graphemes(true)
            .try_for_each(|g| assets(g, &mut found))
            .map(|()| found.iter().map(|a| (uri(a), a.data.0.as_bytes())).collect::<Svgs>());
        let out = svgs.as_ref().and_then(|s| textures(&mut self.loaded, ctx, s, frame));
        if self.runs.len() >= MAX_RUNS {
            self.runs.retain(|_, (_, used)| *used == frame);
        }
        if self.runs.len() < MAX_RUNS {
            self.runs.insert(run.to_owned(), (svgs, frame));
        }
        out
    }

    /// Width of the `run` at height `h`; `None` if it is displayed as text.
    pub fn width(&mut self, ctx: &egui::Context, run: &str, h: f32) -> Option<f32> {
        self.images(ctx, run).map(|imgs| imgs.iter().map(|t| width(t, h)).sum())
    }

    /// `text` at `size` and `ink`, colour emojis and clickable web links; wraps like a label.
    pub fn label(&mut self, ui: &mut egui::Ui, text: &str, size: f32, ink: Color32) -> egui::Response {
        if !any(text) && !link::any(text) {
            return ui.label(w::text(text, size, ink));
        }
        ui.vertical(|ui| {
            // One line per paragraph: a multi-line label would break the flow.
            for line in text.lines() {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    // Empty line: its height, as in a label.
                    for p in if line.is_empty() { vec![Piece::Text(" ")] } else { split(line) } {
                        let imgs = match p {
                            Piece::Emoji(e) => self.images(ui.ctx(), e),
                            Piece::Text(_) => None,
                        };
                        if let Some(imgs) = imgs {
                            for t in &imgs {
                                ui.add(image(t, size * EMOJI_SCALE, ink));
                            }
                        } else if let Piece::Text(s) = p {
                            for span in link::spans(s) {
                                match span {
                                    // Hyperlink colour of the theme (accent).
                                    Span::Link(url) => ui.hyperlink_to(egui::RichText::new(url).size(size), url),
                                    Span::Text(s) => ui.label(w::text(s, size, ink)),
                                };
                            }
                        } else {
                            let (Piece::Text(s) | Piece::Emoji(s)) = p;
                            ui.label(w::text(s, size, ink));
                        }
                    }
                });
            }
        })
        .response
    }

    /// A line placed left-aligned at `p` (vertically centred), colour emojis at full opacity
    /// (recognisable even in greyed text), cut at `max_w`; `false` without emoji, the caller
    /// then places it as text.
    pub fn paint_line(&mut self, ui: &egui::Ui, text: &str, p: Pos2, font: &FontId, c: Color32, max_w: f32) -> bool {
        self.paint_marked(ui, text, p, (font, c), max_w, None)
    }

    /// [`Self::paint_line`] with the bytes `mark` of `text` in another style (search hit in bold).
    pub fn paint_marked(&mut self, ui: &egui::Ui, text: &str, p: Pos2, (font, c): (&FontId, Color32), max_w: f32, mark: Option<&(Range<usize>, &FontId, Color32)>) -> bool {
        // A single line never shows more than this: the rest of a long message is not split.
        let text = text.get(..text.floor_char_boundary(PAINT_MAX)).unwrap_or(text);
        if !any(text) {
            return false;
        }
        let h = font.size * EMOJI_SCALE;
        let (mut x, end, mut off) = (p.x, p.x + max_w, 0);
        for piece in split(text) {
            let (Piece::Text(s) | Piece::Emoji(s)) = piece;
            let start = off;
            off += s.len();
            let imgs = match piece {
                Piece::Emoji(e) => self.images(ui.ctx(), e),
                Piece::Text(_) => None,
            };
            if let Some(imgs) = imgs {
                for t in &imgs {
                    let w = width(t, h);
                    if x + w > end {
                        ui.painter().text(egui::pos2(x, p.y), egui::Align2::LEFT_CENTER, "…", font.clone(), c);
                        return true;
                    }
                    image(t, h, Color32::WHITE).paint_at(ui, egui::Rect::from_min_size(egui::pos2(x, p.y - h / 2.0), egui::vec2(w, h)));
                    x += w;
                }
            } else {
                let mut job = LayoutJob::default();
                // Piece cut around the mark (bounds relative to the piece, kept on char boundaries by `split`).
                let (a, b) = mark.map_or((s.len(), s.len()), |(m, ..)| {
                    let a = m.start.clamp(start, off) - start;
                    (a, m.end.clamp(start, off) - start)
                });
                let marked = mark.map_or_else(|| TextFormat::simple(font.clone(), c), |(_, f, mc)| TextFormat::simple((*f).clone(), *mc));
                for (part, fmt) in [(&s[..a], TextFormat::simple(font.clone(), c)), (&s[a..b], marked), (&s[b..], TextFormat::simple(font.clone(), c))] {
                    if !part.is_empty() {
                        job.append(part, 0.0, fmt);
                    }
                }
                job.wrap = TextWrapping::truncate_at_width((end - x).max(0.0));
                let g = ui.painter().layout_job(job);
                let (w, elided) = (g.size().x, g.elided);
                ui.painter().galley(egui::pos2(x, p.y - g.size().y / 2.0), g, c);
                x += w;
                if elided {
                    return true;
                }
            }
        }
        true
    }

    /// Input field (`w::field`) of width `max_w`, with colour emojis: each run is laid out
    /// transparent, widened to its images, which are painted over it.
    pub fn field(&mut self, ui: &mut egui::Ui, text: &mut String, hint: &str, max_w: f32) -> egui::Response {
        let font = &w::regular(font::BODY);
        let h = font.size * EMOJI_SCALE;
        // Colour of `TextEdit` when none is given.
        let ink = ui.visuals().override_text_color.unwrap_or_else(|| ui.visuals().widgets.inactive.text_color());
        let mut lay = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
            let mut job = LayoutJob::default();
            for piece in split(buf.as_str()) {
                let fmt = TextFormat::simple(font.clone(), ink);
                match piece {
                    Piece::Emoji(e) if let Some(w) = self.width(ui.ctx(), e, h) => {
                        let plain = ui.fonts_mut(|f| f.layout_no_wrap(e.to_owned(), font.clone(), ink)).size().x;
                        // The run takes exactly `w`: widened, or shrunk when egui lays out each part of
                        // a sequence (family, ZWJ) as its own glyph.
                        let (lead, size) = if plain > w { (0.0, font.size * w / plain) } else { (w - plain, font.size) };
                        let font_id = FontId::new(size, font.family.clone());
                        job.append(e, lead, TextFormat { font_id, color: Color32::TRANSPARENT, ..fmt });
                    }
                    Piece::Text(s) | Piece::Emoji(s) => job.append(s, 0.0, fmt),
                }
            }
            job.wrap.max_width = wrap;
            ui.fonts_mut(|f| f.layout_job(job))
        };
        let out = w::field(text, hint).desired_width(max_w).layouter(&mut lay).show(ui);
        let painter = ui.painter_at(out.text_clip_rect);
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        let mut end = 0;
        for piece in split(out.galley.text()) {
            let (Piece::Text(s) | Piece::Emoji(s)) = piece;
            end += s.chars().count();
            let Piece::Emoji(e) = piece else { continue };
            let Some(imgs) = self.images(ui.ctx(), e) else { continue };
            // The run, as wide as its images (laid out as above), ends at `r`.
            let r = out.galley.pos_from_cursor(egui::text::CCursor::new(end)).translate(out.galley_pos.to_vec2());
            let mut x = r.max.x - imgs.iter().map(|t| width(t, h)).sum::<f32>();
            for t in &imgs {
                let w = width(t, h);
                painter.image(t.id, egui::Rect::from_min_size(egui::pos2(x, r.center().y - h / 2.0), egui::vec2(w, h)), uv, Color32::WHITE);
                x += w;
            }
        }
        out.response.response
    }

    /// Button of the emoji `emoji` at height `h`, in colour if Twemoji has it (otherwise as text).
    pub fn button(&mut self, ctx: &egui::Context, emoji: &str, h: f32, ink: Color32) -> egui::Button<'static> {
        match self.images(ctx, emoji) {
            Some(imgs) if imgs.len() == 1 => egui::Button::image(image(&imgs[0], h, ink)),
            _ => egui::Button::new(w::text(emoji, h, ink)),
        }
    }

    /// Draws `run` centred on `center`, height `h`, each emoji rotated by `angle` around its
    /// centre; `false` if it is displayed as text (to be drawn by the caller).
    pub fn paint(&mut self, ui: &egui::Ui, run: &str, center: egui::Pos2, h: f32, angle: f32, ink: Color32) -> bool {
        let Some(imgs) = self.images(ui.ctx(), run) else { return false };
        let mut x = center.x - imgs.iter().map(|t| width(t, h)).sum::<f32>() / 2.0;
        for t in &imgs {
            let w = width(t, h);
            let r = egui::Rect::from_center_size(egui::pos2(x + w / 2.0, center.y), egui::vec2(w, h));
            image(t, h, ink).rotate(angle, egui::Vec2::splat(0.5)).paint_at(ui, r);
            x += w;
        }
        true
    }
}

/// Bytes of text looked at by `paint_line` (far more than a line can hold).
const PAINT_MAX: usize = 1024;

/// Height of an emoji in text, relative to the font size: the image has its margins.
const EMOJI_SCALE: f32 = 1.25;

fn width(t: &SizedTexture, h: f32) -> f32 {
    h * t.size.x / t.size.y
}

/// Image of an emoji at height `h` (width per its proportions), with the opacity of `ink`.
fn image(t: &SizedTexture, h: f32, ink: Color32) -> egui::Image<'static> {
    egui::Image::new((t.id, egui::vec2(width(t, h), h))).tint(Color32::from_white_alpha(ink.a()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_hyperlinks() {
        use egui::{OutputCommand, accesskit::Role};
        use egui_kittest::kittest::Queryable;
        // egui exposes a selectable link as `Role::Label`: check the behaviour (click opens the URL).
        let mut emojis = Emojis::default();
        for text in ["see https://example.com/a now", "👋 https://example.com/a"] {
            let mut harness = egui_kittest::Harness::new_ui(|ui| {
                emojis.label(ui, text, 14.0, Color32::WHITE);
            });
            harness.get_by_label("https://example.com/a").click();
            harness.step();
            let opened = harness.output().platform_output.commands.iter().any(|c| matches!(c, OutputCommand::OpenUrl(u) if u.url == "https://example.com/a"));
            assert!(opened, "{text}");
        }
        let harness = egui_kittest::Harness::new_ui(|ui| {
            emojis.label(ui, "mail a@example.com", 14.0, Color32::WHITE);
        });
        assert!(harness.query_by_label("a@example.com").is_none());
        assert!(harness.query_by_role(Role::Link).is_none());
    }

    #[test]
    fn emoji_only_messages() {
        for t in ["😊", " 👍🏽 ", "❤️", "🎉🎉🎉", "👨‍👩‍👧", "✅", "🇫🇷🇫🇷🇫🇷", "‼️", "↩️", "1️⃣", "☝🏽"] {
            assert!(only(t), "{t}");
        }
        for t in ["", "  ", "ok 👍", "😀😀😀😀", "A aimé « 😊 »", "1", "→", "⌘", "©", "☀", "☀\u{FE0E}", "#"] {
            assert!(!only(t), "{t}");
        }
    }

    #[test]
    fn splits_text_and_emoji_runs() {
        assert_eq!(split("Salut 👋🏽 ça va ❤️!"), [Piece::Text("Salut "), Piece::Emoji("👋🏽"), Piece::Text(" ça va "), Piece::Emoji("❤️"), Piece::Text("!")]);
        assert_eq!(split("👨‍👩‍👧"), [Piece::Emoji("👨‍👩‍👧")]);
        // Modifier with no emoji before it: stays text.
        assert_eq!(split("\u{FE0F}a"), [Piece::Text("\u{FE0F}a")]);
        // Text by default without VS16 (☀, ©), emoji with it (☀️).
        assert_eq!(split("☀ © ☀️"), [Piece::Text("☀ © "), Piece::Emoji("☀️")]);
        assert!(split("").is_empty());
    }

    #[test]
    fn twemoji_covers_sequences() {
        // ZWJ, skin tone, flag, keycap, VS16 absent from the key, VS16 inside a ZWJ key, unknown ZWJ.
        for (g, n) in [("👨‍👩‍👧", 1), ("👋🏽", 1), ("🇫🇷", 1), ("1️⃣", 1), ("❤️", 1), ("👨🏻‍❤️‍👨🏻", 1), ("😀\u{200D}😀", 2)] {
            let mut out = vec![];
            assert!(assets(g, &mut out).is_some(), "{g}");
            assert_eq!(out.len(), n, "{g}");
        }
        assert!(assets("a", &mut vec![]).is_none());
        assert_eq!(uri(asset("👋🏽").unwrap()), "bytes://twemoji/1f44b-1f3fd.svg");
    }
}
