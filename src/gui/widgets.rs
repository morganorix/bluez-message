//! Generic components, modelled value for value on the bar's menu
//! (ui/menu.plm, ui/menuview.plm, ui/common.plm, ui/wifi.plm, ui/energy.plm).

use std::sync::Arc;

use eframe::egui::text::{LayoutJob, TextWrapping};
use eframe::egui::{
    Align, Align2, Button, Color32, CursorIcon, FontFamily, FontId, Galley, Image, Label, Layout, Margin, Pos2, Rect, Response, RichText, Sense, Sides,
    TextEdit, TextFormat, Ui, UiBuilder, Vec2, vec2,
};

use super::emoji::Emojis;
use super::icons::Icon;
use super::theme::{BOLD, CORNER, Palette, font, size};

/// Gap between the rows of a list (`gap: 2` of `rows`).
pub const ROW_GAP: f32 = 2.0;
/// Gap between the rows of a view (`viewGap`).
pub const GAP: f32 = 4.0;
/// Side margin of the content in the panel (list at 688 in a menu at 668).
pub const PAD: i8 = 20;
/// Top margin of the content (list at 60 under a panel at 48).
pub const TOP: i8 = 12;
/// Centre of the row and header icons.
const ICON_X: f32 = 22.0;
/// X position of the labels, after the icon.
const TEXT_X: f32 = 44.0;

#[must_use]
pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

#[must_use]
pub fn bold_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD.into()))
}

/// Text of size `size`, colour `c`.
pub fn text(s: impl Into<String>, size: f32, c: Color32) -> RichText {
    RichText::new(s).size(size).color(c)
}

/// `icon` of side `size` centred on `center`, tinted `c`.
pub fn paint_icon(ui: &Ui, icon: Icon, center: Pos2, size: f32, c: Color32) {
    icon.image().tint(c).paint_at(ui, Rect::from_center_size(center, Vec2::splat(size)));
}

/// `icon` of side `size` tinted `c`, placed in the layout.
pub fn icon(ui: &mut Ui, icon: Icon, size: f32, c: Color32) -> Response {
    ui.add(icon.image().tint(c).fit_to_exact_size(Vec2::splat(size)))
}

/// Icon of a button or menu entry with text: 14, tinted `c`.
pub fn small_icon(icon: Icon, c: Color32) -> Image<'static> {
    icon.image().tint(c).fit_to_exact_size(Vec2::splat(14.0))
}

/// Menu entry: icon then `label`, both in the text colour.
pub fn menu_item(ui: &mut Ui, icon: Icon, label: &str) -> Response {
    ui.add(Button::image_and_text(small_icon(icon, ui.visuals().text_color()), label))
}

/// A line truncated with "…" at `max_w`.
fn line(ui: &Ui, s: &str, font: FontId, c: Color32, max_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(s.to_owned(), TextFormat::simple(font, c));
    job.wrap = TextWrapping::truncate_at_width(max_w.max(0.0));
    ui.painter().layout_job(job)
}

/// Places `g` anchored left (or right) and vertically centred on `p`.
fn put(ui: &Ui, g: Arc<Galley>, p: Pos2, right: bool, c: Color32) -> f32 {
    let w = g.size().x;
    let x = if right { p.x - w } else { p.x };
    ui.painter().galley(Pos2::new(x, p.y - g.size().y / 2.0), g, c);
    w
}

/// Rule: `fg` at 10 %, 1 px, full width.
pub fn rule(ui: &mut Ui, pal: &Palette) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0, pal.dim(0.10));
}

/// `MenuHeader` / `ViewHeader` (44 high): icon 16 at 22 (50 %), title 15 bold at 44.
pub fn header(ui: &mut Ui, pal: &Palette, icon: Icon, title: &str) -> Rect {
    header_with(ui, pal, icon, title, 200.0)
}

/// [`header`] whose title leaves `reserve` on the right (indicators, buttons).
pub fn header_with(ui: &mut Ui, pal: &Palette, icon: Icon, title: &str, reserve: f32) -> Rect {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), size::HEADER_H), Sense::hover());
    let y = rect.center().y;
    paint_icon(ui, icon, Pos2::new(rect.left() + ICON_X, y), font::ICON, pal.dim(0.5));
    let g = line(ui, title, bold_font(font::TITLE), pal.fg, rect.width() - TEXT_X - reserve);
    put(ui, g, Pos2::new(rect.left() + TEXT_X, y), false, pal.fg);
    rect
}

/// Square button (`icon_button`) to the right of a header drawn by `header`, 14 from the edge.
pub fn header_button(ui: &mut Ui, pal: &Palette, header: Rect, icon: Icon) -> Response {
    let zone = Rect::from_center_size(header.right_center() - vec2(size::EDGE + 20.0, 0.0), vec2(40.0, 32.0));
    ui.scope_builder(UiBuilder::new().max_rect(zone), |ui| icon_button(ui, pal, icon)).inner
}

/// `NearHeader` (38 high): label 13 at 50 % centred at 22, action 30 × 28 on the right (icon 16, 60 → 100 %),
/// rule at 36.
pub fn section_action(ui: &mut Ui, pal: &Palette, label: &str, icon: Icon) -> Response {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
    let g = line(ui, label, regular(font::LABEL), pal.dim(0.5), rect.width() - 40.0);
    put(ui, g, Pos2::new(rect.left(), rect.top() + 22.0), false, pal.dim(0.5));
    let l = Rect::from_min_size(Pos2::new(rect.left(), rect.top() + 36.0), vec2(rect.width(), 1.0));
    ui.painter().rect_filled(l, 0, pal.dim(0.10));
    let zone = Rect::from_min_size(Pos2::new(rect.right() - 34.0, rect.top() + 8.0), vec2(30.0, 28.0));
    let r = ui.interact(zone, ui.id().with(label), Sense::click());
    let a = if r.hovered() { 1.0 } else { 0.6 };
    paint_icon(ui, icon, zone.center(), font::ICON, pal.dim(a));
    r.on_hover_cursor(CursorIcon::PointingHand)
}

/// `Hint` (16 high): text 12 at 40 %.
pub fn hint(ui: &mut Ui, pal: &Palette, label: &str) {
    ui.label(text(label, font::BODY, pal.muted()));
}

/// A list row.
pub struct Row<'a> {
    /// Without icon: empty space (unticked choice).
    pub icon: Option<Icon>,
    pub label: &'a str,
    /// Sub-line 11 at 40 % (row of 48, like `NearCard`); without it, row of 36 (`Row`).
    pub sub: Option<&'a str>,
    /// On the right, 11 at 40 %.
    pub hint: &'a str,
    /// Counter in a badge, on the sub-line.
    pub badge: usize,
    pub selected: bool,
    pub strong: bool,
}

/// `Row` of ui/menu.plm: accent background 15 % if chosen or hovered, icon 15 at 22 (60 → 100 %, turns accent),
/// label 13 at 44 (70 → 100 %), hint 11 at 40 % on the right (w − 14).
pub fn row(ui: &mut Ui, pal: &Palette, r: &Row) -> Response {
    row_with(ui, pal, r, None)
}

/// [`row`] whose sub-line shows its emojis in colour.
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
        if let Some(icon) = r.icon {
            paint_icon(ui, icon, Pos2::new(x + ICON_X, rect.center().y), font::TITLE, icon_c);
        }
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

/// Badge (`KnownCard` badge): 18 high, radius 4, accent 15 %, text 10 bold; anchored right on `p`.
fn badge_at(ui: &Ui, pal: &Palette, label: &str, p: Pos2) -> f32 {
    let g = ui.painter().layout_no_wrap(label.to_owned(), bold_font(font::TINY), pal.accent);
    let w = g.size().x + 12.0;
    let rect = Rect::from_min_size(Pos2::new(p.x - w, p.y - 9.0), vec2(w, 18.0));
    ui.painter().rect_filled(rect, CORNER, pal.accent.gamma_multiply(0.15));
    ui.painter().galley(rect.center() - g.size() / 2.0, g, pal.accent);
    w
}

/// Input field like the wifi password: text 12 at 10 from the edge (height: `interact_size.y`).
pub fn field<'t>(t: &'t mut String, hint: &str) -> TextEdit<'t> {
    TextEdit::singleline(t).hint_text(hint).font(regular(font::BODY)).margin(Margin::symmetric(10, 10))
}

/// Content right-aligned in `rect`, vertically centred.
pub fn right_of<R>(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.scope_builder(UiBuilder::new().max_rect(rect).layout(Layout::right_to_left(Align::Center)), add).inner
}

/// Call card (`NearCard` of the chosen device): 48 high, accent 7 %, icon 16 centred at 18 in `tint`,
/// title 13 `fg` at 34, sub-line 11 in `sub_c`. Returns the card so buttons can be placed on it.
pub fn call_card(ui: &mut Ui, pal: &Palette, icon: Icon, tint: Color32, title: &str, sub: &str, sub_c: Color32) -> Rect {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), size::CARD_H), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CORNER, pal.accent.gamma_multiply(0.07));
    let at = |x: f32, y: f32| rect.left_top() + vec2(x, y);
    paint_icon(ui, icon, at(18.0, size::CARD_H / 2.0), font::ICON, tint);
    p.text(at(34.0, 16.0), Align2::LEFT_CENTER, title, regular(font::LABEL), pal.fg);
    p.text(at(34.0, 33.0), Align2::LEFT_CENTER, sub, regular(font::SMALL), sub_c);
    rect
}

/// Information or error banner (like `wifi_error`): 32 high, icon and text 12 in `color` truncated,
/// cross on the right. `true`: closed.
pub fn banner(ui: &mut Ui, pal: &Palette, icon: Icon, msg: &str, color: Color32) -> bool {
    let mut close = false;
    Sides::new().shrink_left().height(size::BANNER_H).show(
        ui,
        |ui| {
            ui.add_space(size::EDGE);
            self::icon(ui, icon, font::ICON, color);
            ui.add(Label::new(text(msg, font::BODY, color)).truncate())
        },
        |ui| close = icon_button(ui, pal, Icon::Close).on_hover_text(crate::t!("Dismiss")).clicked(),
    );
    close
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Notification buttons: `fg` 10 % (+15 %), text `fg`. Only the popup uses them.
    Neutral,
    /// Connected `KnownCard`: accent 10 % (+15 %), accent text.
    Accent,
    /// FORGET / CANCEL: danger 30 % (+20 %), text `fg`.
    Danger,
}

/// Card button: 120 × `interact_size.y` (26; 34 next to a field), radius 4, label 11.
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

/// Square `PowerBar` button: 40 × 32, `fg` 6 % (+12 % on hover), icon 16 at 55 → 100 %.
pub fn icon_button(ui: &mut Ui, pal: &Palette, icon: Icon) -> Response {
    let (rect, r) = ui.allocate_exact_size(vec2(40.0, 32.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let h = if r.hovered() { 1.0 } else { 0.0 };
        ui.painter().rect_filled(rect, CORNER, pal.dim(0.06 + 0.12 * h));
        paint_icon(ui, icon, rect.center(), font::ICON, pal.dim(0.55 + 0.45 * h));
    }
    r.on_hover_cursor(CursorIcon::PointingHand)
}

/// `HeadSwitch` of ui/menuview.plm, painted on the right of a row (w − 14): 50 × 30, background `mix(mix(bg, fg, 0.2),
/// accent, pw)`, 9 `fg` knob (40 → 100 %) sliding from 15 to 35; `pw` animated. The row carries the click.
/// Animation tied to the row's id, not its position: stable while scrolling.
pub fn switch(ui: &mut Ui, pal: &Palette, row: &Response, on: bool) {
    let pw = ui.ctx().animate_bool(row.id.with("switch"), on);
    let row = row.rect;
    let rect = Rect::from_min_size(Pos2::new(row.right() - size::EDGE - 50.0, row.center().y - 15.0), vec2(50.0, 30.0));
    let off = pal.bg.lerp_to_gamma(pal.fg, 0.2);
    ui.painter().rect_filled(rect, CORNER, off.lerp_to_gamma(pal.accent, pw));
    let c = Pos2::new(rect.left() + 15.0 + 20.0 * pw, rect.center().y);
    ui.painter().circle_filled(c, 9.0, pal.fg.gamma_multiply(0.4 + 0.6 * pw));
}

/// Segmented choice on its own row (34 high), aligned to the left edge of the row icons: 26 pills
/// (radius `CORNER`), `on` solid accent, the others `fg` 10 % (accent 25 % on hover), text 11. Returns
/// the clicked option.
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

/// Tabs sharing the full width equally (30 high, 4 apart), same colours as [`segments`]. Returns the
/// clicked tab.
pub fn tabs(ui: &mut Ui, pal: &Palette, labels: &[&str], on: usize) -> Option<usize> {
    let (line, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
    let n = labels.len().max(1) as f32;
    let w = (line.width() - 4.0 * (n - 1.0)) / n;
    let mut hit = None;
    for (i, label) in labels.iter().enumerate() {
        let sel = i == on;
        let ink = if sel { pal.bg } else { pal.fg.gamma_multiply(0.8) };
        let left = line.left() + i as f32 * (w + 4.0);
        let rect = Rect::from_min_size(Pos2::new(left, line.top()), vec2(w, line.height()));
        let r = ui.interact(rect, resp.id.with(i), Sense::click()).on_hover_cursor(CursorIcon::PointingHand);
        let fill = match (sel, r.hovered()) {
            (true, _) => pal.accent,
            (false, true) => pal.accent.gamma_multiply(0.25),
            (false, false) => pal.bg.lerp_to_gamma(pal.fg, 0.1),
        };
        ui.painter().rect_filled(rect, CORNER, fill);
        let g = ui.painter().layout_no_wrap((*label).to_owned(), regular(font::SMALL), ink);
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

/// `KnownCard` status dot (circle 4.5; halo 8 accent 22 % if linked; off `fg` 25 %, in progress accent2),
/// icon 16 at 40 % to its left.
pub fn status_dot(ui: &mut Ui, pal: &Palette, icon: Icon, state: Link) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(font::ICON + 8.0 + 16.0, 30.0), Sense::hover());
    let y = rect.center().y;
    paint_icon(ui, icon, Pos2::new(rect.left() + font::ICON / 2.0, y), font::ICON, pal.muted());
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
