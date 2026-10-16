//! Emojis en couleur. egui ne trame que des glyphes monochromes : les images PNG de la police
//! Noto Color Emoji (table CBDT) sont chargées en textures, une par glyphe après mise en forme
//! (séquences ZWJ, teintes, drapeaux). Sans la police, ou pour un emoji qu'elle n'a pas, le texte
//! reste affiché tel quel.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, OnceLock};

use eframe::egui::text::{LayoutJob, TextWrapping};
use eframe::egui::{self, Color32, ColorImage, FontId, Pos2, TextFormat, TextureFilter, TextureHandle, TextureOptions};
use rustybuzz::ttf_parser::{GlyphId, RasterImageFormat};

use super::widgets as w;

/// Rôle d'un caractère dans une suite d'emojis.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Emoji (ou indicateur régional de drapeau).
    Base,
    /// Teinte, ZWJ, sélecteur de variante, keycap, tag : prolonge l'emoji précédent.
    Modifier,
    Other,
}

fn kind(c: char) -> Kind {
    match u32::from(c) {
        0x200D | 0x20E3 | 0xFE0E | 0xFE0F | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F => Kind::Modifier,
        // Seuls les symboles qui ont une forme emoji : une flèche ou ⌘ reste du texte.
        0x203C | 0x2049 | 0x2194..=0x2199 | 0x21A9 | 0x21AA | 0x231A | 0x231B | 0x2328 | 0x23CF | 0x23E9..=0x23F3 | 0x23F8..=0x23FA
        | 0x2600..=0x27BF | 0x2B05..=0x2B07 | 0x2B1B | 0x2B1C | 0x2B50 | 0x2B55 | 0x1F000..=0x1FAFF => Kind::Base,
        _ => Kind::Other,
    }
}

/// Morceau d'un texte : texte ordinaire ou suite d'emojis collés.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    Text(&'a str),
    Emoji(&'a str),
}

/// Découpe `text` en texte et suites d'emojis ; un modificateur prolonge la suite en cours.
#[must_use]
pub fn split(text: &str) -> Vec<Piece<'_>> {
    let piece = |emoji, s| if emoji { Piece::Emoji(s) } else { Piece::Text(s) };
    let (mut out, mut start, mut emoji) = (vec![], 0, false);
    for (i, c) in text.char_indices() {
        let e = match kind(c) {
            Kind::Base => true,
            Kind::Modifier => emoji,
            Kind::Other => false,
        };
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

/// Message fait de 1 à 3 emojis (et d'espaces) : affiché en grand, sans bulle, comme Messages.
/// Modificateurs non comptés ; un drapeau compte pour deux.
#[must_use]
pub fn only(text: &str) -> bool {
    let mut n = 0;
    for c in text.chars() {
        match kind(c) {
            Kind::Base => n += 1,
            Kind::Modifier => {}
            Kind::Other if c.is_whitespace() => {}
            Kind::Other => return false,
        }
    }
    (1..=3).contains(&n)
}

/// Suite d'emojis plus longue : affichée en texte (borne le travail par suite).
const MAX_RUN: usize = 32;
/// Suites mises en forme gardées ; au-delà, le cache repart de zéro.
const MAX_RUNS: usize = 1024;
/// Images chargées (~70 Ko chacune) ; au-delà, les nouveaux emojis restent en texte. Les SMS reçus
/// sont arbitraires : la mémoire reste bornée.
const MAX_GLYPHS: usize = 512;

type Font = Option<rustybuzz::Face<'static>>;

/// Police d'emojis en couleur, chargée en arrière-plan au premier emoji, et images déjà chargées.
#[derive(Default)]
pub struct Emojis {
    /// `None` : chargement pas encore lancé ; vide : en cours ; `Some(None)` : police absente.
    font: Option<Arc<OnceLock<Font>>>,
    /// Glyphes de chaque suite après mise en forme ; `None` : un emoji manque à la police.
    runs: HashMap<String, Option<Vec<GlyphId>>>,
    /// `None` : glyphe sans image PNG.
    glyphs: HashMap<GlyphId, Option<TextureHandle>>,
}

/// Cherche Noto Color Emoji par fontconfig (parfois lent : hors du fil de l'interface).
fn find_font() -> Font {
    let bytes = Command::new("fc-match")
        .args(["-f", "%{file}", "Noto Color Emoji"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        // fontconfig rend une police de repli si elle manque.
        .filter(|p| p.contains("Emoji"))
        .and_then(|p| std::fs::read(p).ok());
    let Some(bytes) = bytes else {
        tracing::info!("Noto Color Emoji font not found: emojis rendered as text");
        return None;
    };
    // Lue une fois, gardée toute la vie du processus.
    rustybuzz::Face::from_slice(Box::leak(bytes.into_boxed_slice()), 0)
}

impl Emojis {
    /// Police chargée, sinon lance (une fois) son chargement ; `None` tant qu'elle manque.
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

    /// Images de la suite `run` (une par glyphe) ; `None` (texte) sans police, si un emoji lui manque,
    /// pour une suite trop longue ou une fois le plafond d'images atteint.
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

    /// Largeur de la suite `run` à `h` de haut ; `None` si elle s'affiche en texte.
    pub fn width(&mut self, ctx: &egui::Context, run: &str, h: f32) -> Option<f32> {
        self.images(ctx, run).map(|imgs| imgs.iter().map(|t| width(t, h)).sum())
    }

    /// `text` en `size` et `ink`, emojis en couleur ; retour à la ligne comme un label.
    pub fn label(&mut self, ui: &mut egui::Ui, text: &str, size: f32, ink: Color32) -> egui::Response {
        let absent = self.font.as_ref().is_some_and(|f| matches!(f.get(), Some(None)));
        if absent || !text.chars().any(|c| kind(c) == Kind::Base) {
            return ui.label(w::text(text, size, ink));
        }
        ui.vertical(|ui| {
            // Une ligne par paragraphe : un label sur plusieurs lignes casserait l'enchaînement.
            for line in text.lines() {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    // Ligne vide : sa hauteur, comme dans un label.
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

    /// Une ligne posée à gauche sur `p` (centrée en hauteur), emojis en couleur et pleine opacité
    /// (reconnaissables même sur un texte grisé), coupée à `max_w` ; `false` sans emoji, l'appelant
    /// la pose alors en texte.
    pub fn paint_line(&mut self, ui: &egui::Ui, text: &str, p: Pos2, font: &FontId, c: Color32, max_w: f32) -> bool {
        if !text.chars().any(|c| kind(c) == Kind::Base) {
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

    /// Bouton de l'emoji `emoji` à `h` de haut, en couleur si la police l'a (sinon en texte).
    pub fn button(&mut self, ctx: &egui::Context, emoji: &str, h: f32, ink: Color32) -> egui::Button<'static> {
        match self.images(ctx, emoji) {
            Some(imgs) if imgs.len() == 1 => egui::Button::image(image(&imgs[0], h, ink)),
            _ => egui::Button::new(w::text(emoji, h, ink)),
        }
    }

    /// Dessine `run` centrée sur `center`, `h` de haut, chaque emoji tourné de `angle` autour de son
    /// centre ; `false` si elle s'affiche en texte (à dessiner par l'appelant).
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

/// Hauteur d'un emoji dans le texte, relative à la taille de police : l'image a ses marges.
const EMOJI_SCALE: f32 = 1.25;

fn width(t: &TextureHandle, h: f32) -> f32 {
    let s = t.size_vec2();
    h * s.x / s.y
}

/// Image d'un emoji à `h` de haut (largeur selon ses proportions), avec l'opacité de `ink`.
fn image(t: &TextureHandle, h: f32, ink: Color32) -> egui::Image<'static> {
    egui::Image::new((t.id(), egui::vec2(width(t, h), h))).tint(Color32::from_white_alpha(ink.a()))
}

/// Glyphes de `run` après mise en forme (ligatures ZWJ, teintes, drapeaux) ; `None` si un glyphe
/// manque (`.notdef`).
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
        // Glyphe sans image (sélecteur de variante resté seul) : ignoré.
        if face.glyph_raster_image(id, u16::MAX).is_some() {
            ids.push(id);
        }
    }
    (!ids.is_empty()).then_some(ids)
}

/// Image PNG du glyphe `id` (la plus grande de la police), chargée en texture.
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
        for t in ["😊", " 👍🏽 ", "❤️", "🎉🎉🎉", "👨‍👩‍👧", "✅", "🇫🇷", "‼️", "↩️"] {
            assert!(only(t), "{t}");
        }
        for t in ["", "  ", "ok 👍", "😀😀😀😀", "A aimé « 😊 »", "1", "→", "⌘"] {
            assert!(!only(t), "{t}");
        }
    }

    #[test]
    fn splits_text_and_emoji_runs() {
        assert_eq!(split("Salut 👋🏽 ça va ❤️!"), [Piece::Text("Salut "), Piece::Emoji("👋🏽"), Piece::Text(" ça va "), Piece::Emoji("❤️"), Piece::Text("!")]);
        assert_eq!(split("👨‍👩‍👧"), [Piece::Emoji("👨‍👩‍👧")]);
        // Modificateur sans emoji avant lui : reste du texte.
        assert_eq!(split("\u{FE0F}a"), [Piece::Text("\u{FE0F}a")]);
        assert!(split("").is_empty());
    }
}
