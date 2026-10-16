//! Colour emojis. egui only rasterises monochrome glyphs: the PNG images of the
//! Noto Color Emoji font (CBDT table) are loaded as textures, one per glyph after shaping
//! (ZWJ sequences, skin tones, flags). Without the font, or for an emoji it lacks, the text
//! is displayed as is.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, OnceLock};

use eframe::egui::text::{LayoutJob, TextWrapping};
use eframe::egui::{self, Color32, ColorImage, FontId, Pos2, TextFormat, TextureFilter, TextureHandle, TextureOptions};
use rustybuzz::ttf_parser::{GlyphId, RasterImageFormat};
use unicode_properties::{EmojiStatus, UnicodeEmoji};
use unicode_segmentation::UnicodeSegmentation;

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
/// Shaped runs kept; beyond that, the cache starts from zero.
const MAX_RUNS: usize = 1024;
/// Loaded images (~70 KB each); beyond that, new emojis stay text. Received SMS
/// are arbitrary: memory stays bounded.
const MAX_GLYPHS: usize = 512;

type Font = Option<rustybuzz::Face<'static>>;

/// Colour emoji font, loaded in the background at the first emoji, and images already loaded.
#[derive(Default)]
pub struct Emojis {
    /// `None`: loading not started yet; empty: in progress; `Some(None)`: font absent.
    font: Option<Arc<OnceLock<Font>>>,
    /// Glyphs of each run after shaping; `None`: an emoji is missing from the font.
    runs: HashMap<String, Option<Vec<GlyphId>>>,
    /// `None`: glyph without a PNG image.
    glyphs: HashMap<GlyphId, Option<TextureHandle>>,
}

/// System locations of fontconfig's `fc-match`.
const FC_MATCH: [&str; 2] = ["/usr/bin/fc-match", "/bin/fc-match"];

/// Looks for Noto Color Emoji through fontconfig (sometimes slow: off the interface thread).
fn find_font() -> Font {
    // Absolute path: a `fc-match` placed earlier in the PATH is not run.
    let bytes = FC_MATCH
        .iter()
        .find(|p| std::path::Path::new(p).is_file())
        .and_then(|p| Command::new(p).args(["-f", "%{file}", "Noto Color Emoji"]).output().ok())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        // fontconfig returns a fallback font if it is missing.
        .filter(|p| p.contains("Emoji"))
        .and_then(|p| std::fs::read(p).ok());
    let Some(bytes) = bytes else {
        tracing::info!("Noto Color Emoji font not found: emojis rendered as text");
        return None;
    };
    // Read once, kept for the whole life of the process.
    rustybuzz::Face::from_slice(Box::leak(bytes.into_boxed_slice()), 0)
}

impl Emojis {
    /// Loaded font, otherwise starts (once) its loading; `None` while it is missing.
    fn face<'a>(font: &'a mut Option<Arc<OnceLock<Font>>>, ctx: &egui::Context) -> Option<&'a rustybuzz::Face<'static>> {
        let slot = font.get_or_insert_with(|| {
            let slot = Arc::new(OnceLock::new());
            let (s, ctx) = (Arc::clone(&slot), ctx.clone());
            std::thread::spawn(move || {
                s.get_or_init(find_font);
                ctx.request_repaint();
            });
            slot
        });
        slot.get()?.as_ref()
    }

    /// Images of the `run` (one per glyph); `None` (text) without the font, if an emoji is missing from it,
    /// for a run that is too long or once the image cap is reached.
    pub fn images(&mut self, ctx: &egui::Context, run: &str) -> Option<Vec<TextureHandle>> {
        if run.chars().nth(MAX_RUN).is_some() {
            return None;
        }
        let face = Self::face(&mut self.font, ctx)?;
        if !self.runs.contains_key(run) {
            if self.runs.len() >= MAX_RUNS {
                self.runs.clear();
            }
            self.runs.insert(run.to_owned(), shape(face, run));
        }
        let ids = self.runs.get(run)?.as_ref()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if !self.glyphs.contains_key(id) {
                if self.glyphs.len() >= MAX_GLYPHS {
                    return None;
                }
                self.glyphs.insert(*id, load(ctx, face, *id));
            }
            out.push(self.glyphs.get(id)?.clone()?);
        }
        Some(out)
    }

    /// Width of the `run` at height `h`; `None` if it is displayed as text.
    pub fn width(&mut self, ctx: &egui::Context, run: &str, h: f32) -> Option<f32> {
        self.images(ctx, run).map(|imgs| imgs.iter().map(|t| width(t, h)).sum())
    }

    /// `text` at `size` and `ink`, colour emojis; wraps like a label.
    pub fn label(&mut self, ui: &mut egui::Ui, text: &str, size: f32, ink: Color32) -> egui::Response {
        let absent = self.font.as_ref().is_some_and(|f| matches!(f.get(), Some(None)));
        if absent || !any(text) {
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
        // A single line never shows more than this: the rest of a long message is not split.
        let text = text.get(..text.floor_char_boundary(PAINT_MAX)).unwrap_or(text);
        if !any(text) {
            return false;
        }
        let h = font.size * EMOJI_SCALE;
        let (mut x, end) = (p.x, p.x + max_w);
        for piece in split(text) {
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
                let (Piece::Text(s) | Piece::Emoji(s)) = piece;
                let mut job = LayoutJob::single_section(s.to_owned(), TextFormat::simple(font.clone(), c));
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
                painter.image(t.id(), egui::Rect::from_min_size(egui::pos2(x, r.center().y - h / 2.0), egui::vec2(w, h)), uv, Color32::WHITE);
                x += w;
            }
        }
        out.response.response
    }

    /// Button of the emoji `emoji` at height `h`, in colour if the font has it (otherwise as text).
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

fn width(t: &TextureHandle, h: f32) -> f32 {
    let s = t.size_vec2();
    h * s.x / s.y
}

/// Image of an emoji at height `h` (width per its proportions), with the opacity of `ink`.
fn image(t: &TextureHandle, h: f32, ink: Color32) -> egui::Image<'static> {
    egui::Image::new((t.id(), egui::vec2(width(t, h), h))).tint(Color32::from_white_alpha(ink.a()))
}

/// Glyphs of `run` after shaping (ZWJ ligatures, skin tones, flags); `None` if a glyph
/// is missing (`.notdef`).
fn shape(face: &rustybuzz::Face, run: &str) -> Option<Vec<GlyphId>> {
    let mut buf = rustybuzz::UnicodeBuffer::new();
    buf.push_str(run);
    let shaped = rustybuzz::shape(face, &[], buf);
    let mut ids = vec![];
    for g in shaped.glyph_infos() {
        let id = GlyphId(u16::try_from(g.glyph_id).ok()?);
        if id.0 == 0 {
            return None;
        }
        // Glyph without an image (variation selector left alone): ignored.
        if face.glyph_raster_image(id, u16::MAX).is_some() {
            ids.push(id);
        }
    }
    (!ids.is_empty()).then_some(ids)
}

/// PNG image of glyph `id` (the largest in the font), loaded as a texture.
fn load(ctx: &egui::Context, face: &rustybuzz::Face, id: GlyphId) -> Option<TextureHandle> {
    let img = face.glyph_raster_image(id, u16::MAX)?;
    if img.format != RasterImageFormat::PNG {
        return None;
    }
    let rgba = image::load_from_memory_with_format(img.data, image::ImageFormat::Png).ok()?.to_rgba8();
    let size = [usize::try_from(rgba.width()).ok()?, usize::try_from(rgba.height()).ok()?];
    let color = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    let opts = TextureOptions { mipmap_mode: Some(TextureFilter::Linear), ..TextureOptions::LINEAR };
    Some(ctx.load_texture(format!("emoji {}", id.0), color, opts))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
