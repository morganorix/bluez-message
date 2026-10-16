//! Couleurs de la barre (template matugen `colors.plm`), police Nerd Font et mesures de ui/*.plm.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Visuals, vec2};

const FONT: &str = "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf";
const FONT_BOLD: &str = "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Bold.ttf";
/// Famille des titres et badges (`weight: 700` de la barre).
pub const BOLD: &str = "bold";
/// Arrondi commun des cartes, boutons et champs (`menuCorner` de control-bar.plm).
pub const CORNER: u8 = 4;

/// Tailles de police de ui/*.plm.
pub mod font {
    /// Message fait d'emojis seuls, affiché sans bulle.
    pub const EMOJI: f32 = 44.0;
    /// Icônes d'en-tête, de carte et de bouton.
    pub const ICON: f32 = 16.0;
    /// Titres (gras) ; icônes de ligne.
    pub const TITLE: f32 = 15.0;
    /// Libellés de ligne et de carte.
    pub const LABEL: f32 = 13.0;
    /// Texte courant, champs, bandeaux.
    pub const BODY: f32 = 12.0;
    /// Sous-lignes, indications, boutons.
    pub const SMALL: f32 = 11.0;
    /// Badges et horodatages.
    pub const TINY: f32 = 10.0;
}

/// Mesures communes de ui/*.plm.
pub mod size {
    /// `MenuHeader` / `ViewHeader`.
    pub const HEADER_H: f32 = 44.0;
    /// `NearCard` : ligne à sous-ligne, carte d'appel.
    pub const CARD_H: f32 = 48.0;
    /// `Row` sans sous-ligne.
    pub const ROW_H: f32 = 36.0;
    /// Bandeaux d'information et d'erreur.
    pub const BANNER_H: f32 = 32.0;
    /// Champ de saisie (mot de passe wifi) et bouton voisin.
    pub const FIELD_H: f32 = 34.0;
    /// Bouton des cartes.
    pub const BUTTON_W: f32 = 120.0;
    pub const BUTTON_H: f32 = 26.0;
    /// Retrait du contenu au bord d'une carte ou d'un panneau.
    pub const EDGE: f32 = 14.0;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub accent: Color32,
    pub on_accent: Color32,
    pub accent2: Color32,
    pub fg: Color32,
    pub bg: Color32,
    pub danger: Color32,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            accent: Color32::from_rgb(0xb0, 0xd1, 0x8b),
            on_accent: Color32::from_rgb(0x1e, 0x37, 0x02),
            accent2: Color32::from_rgb(0xbf, 0xcb, 0xad),
            fg: Color32::from_rgb(0xe2, 0xe3, 0xd8),
            bg: Color32::from_rgb(0x12, 0x14, 0x0e),
            danger: Color32::from_rgb(0xff, 0xb4, 0xab),
        }
    }
}

fn hex(s: &str) -> Option<Color32> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let [_, r, g, b] = u32::from_str_radix(h, 16).ok()?.to_be_bytes();
    Some(Color32::from_rgb(r, g, b))
}

impl Palette {
    /// Lignes `let nom = #rrggbb` ; les noms inconnus ou valeurs invalides gardent le défaut.
    #[must_use]
    pub fn parse(src: &str) -> Self {
        let mut p = Self::default();
        for line in src.lines() {
            let Some(rest) = line.trim().strip_prefix("let ") else { continue };
            let Some((name, val)) = rest.split_once('=') else { continue };
            let Some(c) = hex(val.trim()) else { continue };
            match name.trim() {
                "accent" => p.accent = c,
                "on_accent" => p.on_accent = c,
                "accent2" => p.accent2 = c,
                "fg" => p.fg = c,
                "bg" => p.bg = c,
                "danger" => p.danger = c,
                _ => {}
            }
        }
        p
    }

    #[must_use]
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path).map(|s| Self::parse(&s)).unwrap_or_default()
    }

    /// `fg` posé sur `bg` à `t` (opaque).
    #[must_use]
    pub fn surface(&self, t: f32) -> Color32 {
        self.bg.lerp_to_gamma(self.fg, t)
    }

    /// `fg` à l'opacité `a` : textes secondaires, fonds de cartes.
    #[must_use]
    pub fn dim(&self, a: f32) -> Color32 {
        self.fg.gamma_multiply(a)
    }

    /// Texte secondaire : `fg` à 40 % (sous-lignes, indications).
    #[must_use]
    pub fn muted(&self) -> Color32 {
        self.dim(0.4)
    }

    /// Bordure des menus de la barre : `mix(bg, accent, 0.2)`.
    #[must_use]
    pub fn line(&self) -> Color32 {
        self.bg.lerp_to_gamma(self.accent, 0.2)
    }

    #[must_use]
    pub fn visuals(&self) -> Visuals {
        let mut v = Visuals::dark();
        v.override_text_color = Some(self.fg);
        v.panel_fill = self.bg;
        v.window_fill = self.bg;
        // Champs de saisie : `fg` à 10 % comme le mot de passe wifi.
        v.extreme_bg_color = self.surface(0.10);
        v.faint_bg_color = self.surface(0.03);
        v.weak_text_color = Some(self.dim(0.4));
        v.hyperlink_color = self.accent;
        v.error_fg_color = self.danger;
        v.warn_fg_color = self.accent2;
        v.selection.bg_fill = self.accent.gamma_multiply(0.35);
        v.selection.stroke = Stroke::new(1.0, self.accent);
        v.window_corner_radius = CornerRadius::same(CORNER);
        v.menu_corner_radius = CornerRadius::same(CORNER);
        v.window_stroke = Stroke::new(1.0, self.line());
        let w = &mut v.widgets;
        // Filets de section : `fg` à 10 %.
        w.noninteractive.bg_stroke = Stroke::new(1.0, self.surface(0.10));
        for (s, t) in [(&mut w.noninteractive, 0.0), (&mut w.inactive, 0.10), (&mut w.hovered, 0.18), (&mut w.active, 0.25), (&mut w.open, 0.12)] {
            if t > 0.0 {
                s.bg_fill = self.surface(t);
                s.weak_bg_fill = self.surface(t);
            }
            s.corner_radius = CornerRadius::same(CORNER);
        }
        w.inactive.bg_stroke = Stroke::NONE;
        w.hovered.bg_stroke = Stroke::new(1.0, self.line());
        w.active.bg_stroke = Stroke::new(1.0, self.accent);
        v
    }
}

/// JetBrainsMono Nerd Font en tête des familles si présente ; sinon polices par défaut.
/// La famille `BOLD` existe toujours (repli sur la police normale).
#[must_use]
pub fn fonts() -> FontDefinitions {
    let mut f = FontDefinitions::default();
    if let Ok(bytes) = std::fs::read(FONT) {
        f.font_data.insert("jbm".into(), Arc::new(FontData::from_owned(bytes)));
        for fam in [FontFamily::Proportional, FontFamily::Monospace] {
            f.families.entry(fam).or_default().insert(0, "jbm".into());
        }
    }
    let mut bold = f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    if let Ok(bytes) = std::fs::read(FONT_BOLD) {
        f.font_data.insert("jbm-bold".into(), Arc::new(FontData::from_owned(bytes)));
        bold.insert(0, "jbm-bold".into());
    }
    f.families.insert(FontFamily::Name(BOLD.into()), bold);
    f
}

/// Tailles de la barre : 15 titre, 13 libellé, 12 texte, 11 bouton et sous-ligne.
fn style(s: &mut egui::Style) {
    s.text_styles = [
        (TextStyle::Heading, FontId::new(font::TITLE, FontFamily::Name(BOLD.into()))),
        (TextStyle::Body, FontId::proportional(font::BODY)),
        (TextStyle::Monospace, FontId::monospace(font::BODY)),
        (TextStyle::Button, FontId::proportional(font::SMALL)),
        (TextStyle::Small, FontId::proportional(font::TINY)),
    ]
    .into();
    s.spacing.item_spacing = vec2(8.0, 4.0);
    s.spacing.button_padding = vec2(12.0, 5.0);
    s.spacing.interact_size.y = size::BUTTON_H;
    s.spacing.scroll.bar_width = 6.0;
    s.spacing.scroll.floating = true;
}

/// `colors.plm` de la barre (sortie du template matugen `control-bar`).
#[must_use]
pub fn colors_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("control-bar/colors.plm"))
}

/// Surveille `colors.plm` : quand matugen le régénère (nouveau fond d'écran, nouveau schéma), les couleurs
/// sont relues et appliquées sans relancer la fenêtre.
pub struct Watch {
    path: Option<PathBuf>,
    stamp: Option<SystemTime>,
    next: Instant,
}

const WATCH_EVERY: Duration = Duration::from_secs(1);

impl Default for Watch {
    fn default() -> Self {
        Self::new()
    }
}

impl Watch {
    pub fn new() -> Self {
        let path = colors_path();
        let stamp = path.as_deref().and_then(modified);
        Self { path, stamp, next: Instant::now() + WATCH_EVERY }
    }

    /// Palette rechargée et appliquée à `ctx` si le fichier a changé ; à appeler à chaque image.
    pub fn poll(&mut self, ctx: &egui::Context) -> Option<Palette> {
        ctx.request_repaint_after(WATCH_EVERY);
        if Instant::now() < self.next {
            return None;
        }
        self.next = Instant::now() + WATCH_EVERY;
        let path = self.path.as_deref()?;
        let stamp = modified(path);
        if stamp.is_none() || stamp == self.stamp {
            return None;
        }
        self.stamp = stamp;
        let p = Palette::load(path);
        ctx.set_visuals_of(egui::Theme::Dark, p.visuals());
        ctx.request_repaint();
        Some(p)
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub fn apply(ctx: &egui::Context, p: &Palette) {
    ctx.set_fonts(fonts());
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, p.visuals());
    ctx.all_styles_mut(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plm() {
        let p = Palette::parse("library Colors {\n    let accent    = #102030\n    let bg = #zzzzzz\n    let other = #ffffff\n    let fg = #+12345\n}");
        assert_eq!(p.accent, Color32::from_rgb(0x10, 0x20, 0x30));
        assert_eq!(p.bg, Palette::default().bg);
        assert_eq!(p.fg, Palette::default().fg);
    }

    #[test]
    fn missing_file_gives_default() {
        assert_eq!(Palette::load(Path::new("/nonexistent/colors.plm")), Palette::default());
    }
}
