//! Composants génériques, calqués valeur pour valeur sur le menu de la barre
//! (ui/menu.plm, ui/menuview.plm, ui/common.plm, ui/wifi.plm, ui/energy.plm).

use std::sync::Arc;

use eframe::egui::text::{LayoutJob, TextWrapping};
use eframe::egui::{
    Align, Align2, Color32, CursorIcon, FontFamily, FontId, Galley, Label, Layout, Margin, Pos2, Rect, Response, RichText, Sense, Sides,
    TextEdit, TextFormat, Ui, UiBuilder, vec2,
};

use super::emoji::Emojis;
use super::theme::{BOLD, CORNER, Palette, font, size};

/// Écart entre les lignes d'une liste (`gap: 2` de `rows`).
pub const ROW_GAP: f32 = 2.0;
/// Écart entre les lignes d'une vue (`viewGap`).
pub const GAP: f32 = 4.0;
/// Marge latérale du contenu dans le panneau (liste à 688 dans un menu à 668).
pub const PAD: i8 = 20;
/// Marge haute du contenu (liste à 60 sous un panneau à 48).
pub const TOP: i8 = 12;
/// Centre des icônes de ligne et d'en-tête.
const ICON_X: f32 = 22.0;
/// Abscisse des libellés, après l'icône.
const TEXT_X: f32 = 44.0;

#[must_use]
pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

#[must_use]
pub fn bold_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD.into()))
}

/// Texte de taille `size`, couleur `c`.
pub fn text(s: impl Into<String>, size: f32, c: Color32) -> RichText {
    RichText::new(s).size(size).color(c)
}

/// Une ligne tronquée par « … » à `max_w`.
fn line(ui: &Ui, s: &str, font: FontId, c: Color32, max_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(s.to_owned(), TextFormat::simple(font, c));
    job.wrap = TextWrapping::truncate_at_width(max_w.max(0.0));
    ui.painter().layout_job(job)
}

/// Pose `g` ancré à gauche (ou à droite) et centré verticalement sur `p`.
fn put(ui: &Ui, g: Arc<Galley>, p: Pos2, right: bool, c: Color32) -> f32 {
    let w = g.size().x;
    let x = if right { p.x - w } else { p.x };
    ui.painter().galley(Pos2::new(x, p.y - g.size().y / 2.0), g, c);
    w
}

/// Filet : `fg` à 10 %, 1 px, toute la largeur.
pub fn rule(ui: &mut Ui, pal: &Palette) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0, pal.dim(0.10));
}

/// `MenuHeader` / `ViewHeader` (44 de haut) : icône 16 à 22 (50 %), titre 15 gras à 44.
pub fn header(ui: &mut Ui, pal: &Palette, icon: &str, title: &str) -> Rect {
    header_with(ui, pal, icon, title, 200.0)
}

/// [`header`] dont le titre laisse `reserve` à droite (indicateurs, boutons).
pub fn header_with(ui: &mut Ui, pal: &Palette, icon: &str, title: &str, reserve: f32) -> Rect {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), size::HEADER_H), Sense::hover());
    let y = rect.center().y;
    ui.painter().text(Pos2::new(rect.left() + ICON_X, y), Align2::CENTER_CENTER, icon, regular(font::ICON), pal.dim(0.5));
    let g = line(ui, title, bold_font(font::TITLE), pal.fg, rect.width() - TEXT_X - reserve);
    put(ui, g, Pos2::new(rect.left() + TEXT_X, y), false, pal.fg);
    rect
}

/// Bouton carré (`icon_button`) à droite d'un en-tête rendu par `header`, à 14 du bord.
pub fn header_button(ui: &mut Ui, pal: &Palette, header: Rect, glyph: &str) -> Response {
    let zone = Rect::from_center_size(header.right_center() - vec2(size::EDGE + 20.0, 0.0), vec2(40.0, 32.0));
    ui.scope_builder(UiBuilder::new().max_rect(zone), |ui| icon_button(ui, pal, glyph)).inner
}

/// `NearHeader` (38 de haut) : libellé 13 à 50 % centré à 22, action 30 × 28 à droite (glyphe 16, 60 → 100 %),
/// filet à 36.
pub fn section_action(ui: &mut Ui, pal: &Palette, label: &str, glyph: &str) -> Response {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
    let g = line(ui, label, regular(font::LABEL), pal.dim(0.5), rect.width() - 40.0);
    put(ui, g, Pos2::new(rect.left(), rect.top() + 22.0), false, pal.dim(0.5));
    let l = Rect::from_min_size(Pos2::new(rect.left(), rect.top() + 36.0), vec2(rect.width(), 1.0));
    ui.painter().rect_filled(l, 0, pal.dim(0.10));
    let zone = Rect::from_min_size(Pos2::new(rect.right() - 34.0, rect.top() + 8.0), vec2(30.0, 28.0));
    let r = ui.interact(zone, ui.id().with(label), Sense::click());
    let a = if r.hovered() { 1.0 } else { 0.6 };
    ui.painter().text(zone.center(), Align2::CENTER_CENTER, glyph, regular(font::ICON), pal.dim(a));
    r.on_hover_cursor(CursorIcon::PointingHand)
}

/// `Hint` (16 de haut) : texte 12 à 40 %.
pub fn hint(ui: &mut Ui, pal: &Palette, label: &str) {
    ui.label(text(label, font::BODY, pal.muted()));
}

/// Une ligne de liste.
pub struct Row<'a> {
    pub icon: &'a str,
    pub label: &'a str,
    /// Sous-ligne 11 à 40 % (ligne de 48, comme `NearCard`) ; sans elle, ligne de 36 (`Row`).
    pub sub: Option<&'a str>,
    /// À droite, 11 à 40 %.
    pub hint: &'a str,
    /// Compteur en pastille, sur la sous-ligne.
    pub badge: usize,
    pub selected: bool,
    pub strong: bool,
}

/// `Row` de ui/menu.plm : fond accent 15 % si choisie ou survolée, icône 15 à 22 (60 → 100 %, vire à l'accent),
/// libellé 13 à 44 (70 → 100 %), indication 11 à 40 % à droite (w − 14).
pub fn row(ui: &mut Ui, pal: &Palette, r: &Row) -> Response {
    row_with(ui, pal, r, None)
}

/// [`row`] dont la sous-ligne montre ses emojis en couleur.
pub fn row_with(ui: &mut Ui, pal: &Palette, r: &Row, emojis: Option<&mut Emojis>) -> Response {
    let h = if r.sub.is_some() { size::CARD_H } else { size::ROW_H };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::click());
    if ui.is_rect_visible(rect) {
        let on = if r.selected || resp.hovered() { 1.0 } else { 0.0 };
        if on > 0.0 {
            ui.painter().rect_filled(rect, CORNER, pal.accent.gamma_multiply(0.15));
        }
        let (x, w) = (rect.left(), rect.width());
        let ink = pal.fg.lerp_to_gamma(pal.accent, on);
        let icon_c = ink.gamma_multiply(0.6 + 0.4 * on);
        ui.painter().text(Pos2::new(x + ICON_X, rect.center().y), Align2::CENTER_CENTER, r.icon, regular(font::TITLE), icon_c);
        let (y1, y2) = if r.sub.is_some() { (rect.top() + 16.0, rect.top() + 33.0) } else { (rect.center().y, 0.0) };
        let hint_w = put(ui, line(ui, r.hint, regular(font::SMALL), pal.muted(), 120.0), Pos2::new(x + w - size::EDGE, y1), true, pal.muted());
        let lc = pal.fg.gamma_multiply(0.7 + 0.3 * on);
        let lf = if r.strong { bold_font(font::LABEL) } else { regular(font::LABEL) };
        put(ui, line(ui, r.label, lf, lc, w - TEXT_X - size::EDGE - hint_w - 12.0), Pos2::new(x + TEXT_X, y1), false, lc);
        if let Some(sub) = r.sub {
            let badge_w = if r.badge > 0 { badge_at(ui, pal, &r.badge.to_string(), Pos2::new(x + w - size::EDGE, y2)) + 8.0 } else { 0.0 };
            let sc = pal.dim(if r.strong { 0.6 } else { 0.4 });
            let (p, max_w) = (Pos2::new(x + TEXT_X, y2), w - TEXT_X - size::EDGE - badge_w);
            if !emojis.is_some_and(|e| e.paint_line(ui, sub, p, &regular(font::SMALL), sc, max_w)) {
                put(ui, line(ui, sub, regular(font::SMALL), sc, max_w), p, false, sc);
            }
        }
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// Pastille (badge de `KnownCard`) : 18 de haut, rayon 4, accent 15 %, texte 10 gras ; ancrée à droite sur `p`.
fn badge_at(ui: &Ui, pal: &Palette, label: &str, p: Pos2) -> f32 {
    let g = ui.painter().layout_no_wrap(label.to_owned(), bold_font(font::TINY), pal.accent);
    let w = g.size().x + 12.0;
    let rect = Rect::from_min_size(Pos2::new(p.x - w, p.y - 9.0), vec2(w, 18.0));
    ui.painter().rect_filled(rect, CORNER, pal.accent.gamma_multiply(0.15));
    ui.painter().galley(rect.center() - g.size() / 2.0, g, pal.accent);
    w
}

/// Champ de saisie comme le mot de passe wifi : texte 12 à 10 du bord (hauteur : `interact_size.y`).
pub fn field<'t>(t: &'t mut String, hint: &str) -> TextEdit<'t> {
    TextEdit::singleline(t).hint_text(hint).font(regular(font::BODY)).margin(Margin::symmetric(10, 10))
}

/// Contenu aligné à droite dans `rect`, centré verticalement.
pub fn right_of<R>(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope_builder(UiBuilder::new().max_rect(rect).layout(Layout::right_to_left(Align::Center)), add).inner
}

/// Carte d'appel (`NearCard` de l'appareil choisi) : 48 de haut, accent 7 %, icône 16 centrée à 18 en `tint`,
/// titre 13 `fg` à 34, sous-ligne 11 en `sub_c`. Rend la carte pour y poser des boutons.
pub fn call_card(ui: &mut Ui, pal: &Palette, icon: &str, tint: Color32, title: &str, sub: &str, sub_c: Color32) -> Rect {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), size::CARD_H), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CORNER, pal.accent.gamma_multiply(0.07));
    let at = |x: f32, y: f32| rect.left_top() + vec2(x, y);
    p.text(at(18.0, size::CARD_H / 2.0), Align2::CENTER_CENTER, icon, regular(font::ICON), tint);
    p.text(at(34.0, 16.0), Align2::LEFT_CENTER, title, regular(font::LABEL), pal.fg);
    p.text(at(34.0, 33.0), Align2::LEFT_CENTER, sub, regular(font::SMALL), sub_c);
    rect
}

/// Bandeau d'information ou d'erreur (comme `wifi_error`) : 32 de haut, icône et texte 12 en `color` tronqués,
/// croix à droite. `true` : fermé.
pub fn banner(ui: &mut Ui, pal: &Palette, icon: &str, msg: &str, color: Color32) -> bool {
    let mut close = false;
    Sides::new().shrink_left().height(size::BANNER_H).show(
        ui,
        |ui| {
            ui.add_space(size::EDGE);
            ui.add(Label::new(text(format!("{icon}  {msg}"), font::BODY, color)).truncate())
        },
        |ui| close = icon_button(ui, pal, "󰅖").on_hover_text(crate::t!("Dismiss")).clicked(),
    );
    close
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Boutons des notifications : `fg` 10 % (+15 %), texte `fg`. Seule la popin s'en sert.
    Neutral,
    /// `KnownCard` connecté : accent 10 % (+15 %), texte accent.
    Accent,
    /// OUBLIER / ANNULER : danger 30 % (+20 %), texte `fg`.
    Danger,
}

/// Bouton des cartes : 120 × `interact_size.y` (26 ; 34 à côté d'un champ), rayon 4, libellé 11.
pub fn button(ui: &mut Ui, pal: &Palette, label: &str, tone: Tone) -> Response {
    let (tint, ink, base, hover) = match tone {
        Tone::Neutral => (pal.fg, pal.fg, 0.10, 0.15),
        Tone::Accent => (pal.accent, pal.accent, 0.10, 0.15),
        Tone::Danger => (pal.danger, pal.fg, 0.30, 0.20),
    };
    let g = ui.painter().layout_no_wrap(label.to_owned(), regular(font::SMALL), ink);
    let dim = vec2((g.size().x + 24.0).max(size::BUTTON_W), ui.spacing().interact_size.y);
    let (rect, r) = ui.allocate_exact_size(dim, Sense::click());
    if ui.is_rect_visible(rect) {
        let a = base + if r.hovered() { hover } else { 0.0 };
        ui.painter().rect_filled(rect, CORNER, tint.gamma_multiply(a));
        ui.painter().galley(rect.center() - g.size() / 2.0, g, ink);
    }
    r.on_hover_cursor(CursorIcon::PointingHand)
}

/// Bouton carré de `PowerBar` : 40 × 32, `fg` 6 % (+12 % au survol), glyphe 16 à 55 → 100 %.
pub fn icon_button(ui: &mut Ui, pal: &Palette, glyph: &str) -> Response {
    let (rect, r) = ui.allocate_exact_size(vec2(40.0, 32.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let h = if r.hovered() { 1.0 } else { 0.0 };
        ui.painter().rect_filled(rect, CORNER, pal.dim(0.06 + 0.12 * h));
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, glyph, regular(font::ICON), pal.dim(0.55 + 0.45 * h));
    }
    r.on_hover_cursor(CursorIcon::PointingHand)
}

/// `HeadSwitch` de ui/menuview.plm, peint à droite d'une ligne (w − 14) : 50 × 30, fond `mix(mix(bg, fg, 0.2),
/// accent, pw)`, pastille 9 `fg` (40 → 100 %) glissant de 15 à 35 ; `pw` animé. La ligne porte le clic.
/// Animation liée à l'identifiant de la ligne, pas à sa position : stable pendant le défilement.
pub fn switch(ui: &mut Ui, pal: &Palette, row: &Response, on: bool) {
    let pw = ui.ctx().animate_bool(row.id.with("switch"), on);
    let row = row.rect;
    let rect = Rect::from_min_size(Pos2::new(row.right() - size::EDGE - 50.0, row.center().y - 15.0), vec2(50.0, 30.0));
    let off = pal.bg.lerp_to_gamma(pal.fg, 0.2);
    ui.painter().rect_filled(rect, CORNER, off.lerp_to_gamma(pal.accent, pw));
    let c = Pos2::new(rect.left() + 15.0 + 20.0 * pw, rect.center().y);
    ui.painter().circle_filled(c, 9.0, pal.fg.gamma_multiply(0.4 + 0.6 * pw));
}

/// Choix segmenté sur sa propre ligne (34 de haut), calé sur le bord gauche des icônes de ligne : pastilles de 26
/// (rayon `CORNER`), `on` plein accent, les autres `fg` 10 % (accent 25 % au survol), texte 11. Renvoie
/// l'option cliquée.
pub fn segments(ui: &mut Ui, pal: &Palette, labels: &[&str], on: usize) -> Option<usize> {
    let (line, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    let (mut x, y) = (line.left() + ICON_X - font::TITLE / 2.0, line.center().y);
    let mut hit = None;
    for (i, label) in labels.iter().enumerate() {
        let sel = i == on;
        let ink = if sel { pal.bg } else { pal.fg.gamma_multiply(0.8) };
        let g = ui.painter().layout_no_wrap((*label).to_owned(), regular(font::SMALL), ink);
        let rect = Rect::from_min_max(Pos2::new(x, y - 13.0), Pos2::new(x + g.size().x + 20.0, y + 13.0));
        x = rect.right() + 4.0;
        let r = ui.interact(rect, resp.id.with(i), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        let fill = match (sel, r.hovered()) {
            (true, _) => pal.accent,
            (false, true) => pal.accent.gamma_multiply(0.25),
            (false, false) => pal.bg.lerp_to_gamma(pal.fg, 0.1),
        };
        ui.painter().rect_filled(rect, CORNER, fill);
        ui.painter().galley(rect.center() - g.size() / 2.0, g, ink);
        if r.clicked() {
            hit = Some(i);
        }
    }
    hit
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Off,
    Pending,
    On,
}

/// Point d'état de `KnownCard` (rond 4.5 ; halo 8 accent 22 % si relié ; éteint `fg` 25 %, en cours accent2),
/// icône (Nerd Font) 16 à 40 % à sa gauche.
pub fn status_dot(ui: &mut Ui, pal: &Palette, icon: &str, state: Link) -> Response {
    let g = ui.painter().layout_no_wrap(icon.to_owned(), regular(font::ICON), pal.muted());
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 8.0 + 16.0, 30.0), Sense::hover());
    let y = rect.center().y;
    put(ui, g, Pos2::new(rect.left(), y), false, pal.muted());
    let c = Pos2::new(rect.right() - 8.0, y);
    let dot = match state {
        Link::Off => pal.dim(0.25),
        Link::Pending => pal.accent2,
        Link::On => pal.accent,
    };
    if state == Link::On {
        ui.painter().circle_filled(c, 8.0, pal.accent.gamma_multiply(0.22));
    }
    ui.painter().circle_filled(c, 4.5, dot);
    resp
}
