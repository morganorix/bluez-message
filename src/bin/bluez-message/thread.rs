//! Fil d'une conversation : en-tête (titre, numéro, Appeler), bulles, lignes d'appel, réactions.
//! Ne modifie ni l'état ni le démon : rend les actions demandées.

use std::borrow::Cow;

use bluez_message::app::state::{Conv, Delivery, Msg};
use bluez_message::app::tapback::{self, Badge};
use bluez_message::engine::{CallKind, Item};
use bluez_message::gui::emoji::{self, Emojis};
use bluez_message::gui::theme::{CORNER, Palette, font};
use bluez_message::gui::widgets as w;
use bluez_message::t;
use eframe::egui::{self, Align, Align2, Frame, Label, Layout, Margin, ScrollArea};

/// Action demandée dans le fil, appliquée par la fenêtre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadAction {
    Resend(String),
    React(&'static str, String),
    Delete(Item),
    Dial,
}

/// Fil de la conversation `c`.
pub fn conv(ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis, c: &Conv) -> Vec<ThreadAction> {
    let (title, sub) = if c.name.is_empty() { (c.number.as_str(), "") } else { (c.name.as_str(), c.number.as_str()) };
    let dial = header(ui, pal, title, sub);
    let max_w = ui.available_width() * 0.7;
    let mut acts = vec![];
    // Première image du fil : les emojis déjà présents ne s'animent pas, seuls les nouveaux.
    let now = ui.input(|i| i.time);
    let opened = ui.data_mut(|d| *d.get_temp_mut_or_insert_with(egui::Id::new(("thread", &c.key)), || now));
    ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = w::GAP;
        // Réactions repliées en badge sur le message visé.
        let fold = tapback::fold(&c.msgs);
        // Rang de chaque message d'emojis parmi ceux de même texte et de même sens : identité stable
        // malgré une suppression, un historique chargé avant ou une bulle en attente remplacée à l'envoi.
        let mut rank: std::collections::HashMap<(&str, bool), usize> = std::collections::HashMap::new();
        for (i, m) in c.msgs.iter().enumerate() {
            if fold.folded[i] {
                continue;
            }
            if let Some((kind, secs)) = m.call {
                call_line(ui, pal, m, kind, secs, &mut acts);
                continue;
            }
            if emoji::only(&m.text) {
                let n = rank.entry((m.text.as_str(), m.outgoing)).or_default();
                let id = egui::Id::new(("emoji", &c.key, &m.text, m.outgoing, *n));
                *n += 1;
                let seen = ui.data_mut(|d| *d.get_temp_mut_or_insert_with(id, || now));
                big(ui, pal, emojis, m, &fold.badges[i], (seen > opened).then_some(now - seen), &mut acts);
                continue;
            }
            bubble(ui, pal, emojis, m, &fold.badges[i], max_w, &mut acts);
        }
    });
    if dial {
        acts.push(ThreadAction::Dial);
    }
    acts
}

/// Contact ou numéro sans conversation : en-tête seul ; `true` si Appeler est cliqué.
pub fn contact(ui: &mut egui::Ui, pal: &Palette, name: &str, number: &str) -> bool {
    let (title, sub) = if name.is_empty() { (number, "") } else { (name, number) };
    let dial = header(ui, pal, title, sub);
    placeholder(ui, pal, t!("No messages yet: write below or call"));
    dial
}

/// Texte centré dans la zone du fil.
pub fn placeholder(ui: &mut egui::Ui, pal: &Palette, msg: &str) {
    ui.centered_and_justified(|ui| ui.label(w::text(msg, font::BODY, pal.muted())));
}

/// Ligne d'appel centrée, comme un `Hint` : 11 à 40 % (manqué en danger) ; suppression au clic droit.
fn call_line(ui: &mut egui::Ui, pal: &Palette, m: &Msg, kind: CallKind, secs: u64, acts: &mut Vec<ThreadAction>) {
    let color = if kind == CallKind::Missed { pal.danger } else { pal.muted() };
    let r = ui.vertical_centered(|ui| {
        ui.add(Label::new(w::text(format!("󰏲  {}  ·  {}", call_label(kind, secs), short_stamp(&m.timestamp)), font::SMALL, color)).sense(egui::Sense::click()))
    });
    r.inner.context_menu(|ui| {
        if ui.button(format!("󰆴  {}", t!("Delete"))).clicked() {
            acts.push(ThreadAction::Delete(m.item()));
        }
    });
}

/// Bulle d'un SMS : en attente (barre de progression), en échec (bouton de renvoi), menu au clic droit
/// (réagir, renvoyer, copier, supprimer) ; réactions en badges sur le coin haut.
fn bubble(ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis, m: &Msg, badges: &[Badge], max_w: f32, acts: &mut Vec<ThreadAction>) {
    if !badges.is_empty() {
        ui.add_space(BADGE_R);
    }
    let layout = if m.outgoing { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
    // Hauteur 0 : la ligne prend la hauteur de la bulle, pas celle de la zone défilante.
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 0.0), layout, |ui| {
        // Encart de MapInfo (`fg` 4 %, 12 de marge) ; les miens en accent 15 %, comme les puces.
        let fill = match (m.outgoing, m.delivery) {
            (_, Delivery::Failed) => pal.danger.gamma_multiply(0.15),
            (true, _) => pal.accent.gamma_multiply(0.15),
            (false, _) => pal.dim(0.04),
        };
        let ink = if m.delivery == Delivery::Pending { pal.fg.gamma_multiply(0.6) } else { pal.fg };
        let frame = Frame::new().fill(fill).corner_radius(CORNER).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
            ui.set_max_width(max_w);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                let t = emojis.label(ui, &m.text, font::BODY, ink);
                match m.delivery {
                    Delivery::Pending => progress(ui, pal, t.rect.width().max(90.0)),
                    Delivery::Failed => drop(ui.label(w::text(t!("Not sent"), font::TINY, pal.danger))),
                    Delivery::Done => drop(ui.label(w::text(short_stamp(&m.timestamp), font::TINY, pal.muted()))),
                }
            });
        });
        actions(ui, pal, emojis, m, &frame.response, badges, acts);
    });
}

/// Après la bulle (ou les emojis) de `m` : badges, bouton de renvoi en échec, menu au clic droit
/// (réagir, renvoyer, copier, supprimer) ; aucun menu pendant l'envoi.
fn actions(ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis, m: &Msg, shown: &egui::Response, badges: &[Badge], acts: &mut Vec<ThreadAction>) {
    badges_on(ui, pal, emojis, shown.rect, m.outgoing, badges);
    if m.delivery == Delivery::Failed && w::icon_button(ui, pal, "󰑐").on_hover_text(t!("Resend")).clicked() {
        acts.push(ThreadAction::Resend(m.text.clone()));
    }
    if m.delivery == Delivery::Pending {
        return;
    }
    shown.interact(egui::Sense::click()).context_menu(|ui| {
        if !m.outgoing {
            ui.horizontal(|ui| {
                for (emoji, _) in tapback::KINDS {
                    if ui.add(emojis.button(ui.ctx(), emoji, 20.0, pal.fg).frame(false)).on_hover_text(tapback::label(emoji)).clicked() {
                        acts.push(ThreadAction::React(emoji, m.text.clone()));
                        ui.close();
                    }
                }
            });
            ui.separator();
        }
        if m.outgoing && ui.button(format!("󰑐  {}", t!("Resend"))).clicked() {
            acts.push(ThreadAction::Resend(m.text.clone()));
        }
        if ui.button(format!("󰆏  {}", t!("Copy"))).clicked() {
            ui.ctx().copy_text(m.text.clone());
        }
        if ui.button(format!("󰆴  {}", t!("Delete"))).clicked() {
            acts.push(ThreadAction::Delete(m.item()));
        }
    });
}

/// Durée de l'apparition d'un message d'emojis (rebond puis balancement), en secondes.
const POP: f64 = 1.2;

/// Message d'emojis seuls : grands, sans cadre, horodatage (ou état d'envoi) dessous ;
/// `age` : secondes depuis son arrivée à l'écran, `None` s'il était là à l'ouverture du fil.
fn big(ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis, m: &Msg, badges: &[Badge], age: Option<f64>, acts: &mut Vec<ThreadAction>) {
    if !badges.is_empty() {
        ui.add_space(BADGE_R);
    }
    let layout = if m.outgoing { Layout::right_to_left(Align::Min) } else { Layout::left_to_right(Align::Min) };
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 0.0), layout, |ui| {
        let ink = if m.delivery == Delivery::Pending { pal.fg.gamma_multiply(0.6) } else { pal.fg };
        // Emojis en couleur collés (espaces retirées) ; sans la police, le texte tel quel.
        let compact: String = m.text.split_whitespace().collect();
        let text = m.text.trim();
        let col = Layout::top_down(if m.outgoing { Align::Max } else { Align::Min });
        let shown = ui
            .with_layout(col, |ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                let wide = emojis.width(ui.ctx(), &compact, font::EMOJI);
                let wide = wide.unwrap_or_else(|| ui.painter().layout_no_wrap(text.to_owned(), w::regular(font::EMOJI), ink).size().x);
                // Hauteur du glyphe, sans l'interligne de la police : horodatage près des emojis.
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(wide, font::EMOJI), egui::Sense::hover());
                pop(ui, emojis, rect, &compact, text, ink, age);
                match m.delivery {
                    Delivery::Pending => progress(ui, pal, rect.width().max(40.0)),
                    Delivery::Failed => drop(ui.label(w::text(t!("Not sent"), font::TINY, pal.danger))),
                    Delivery::Done => drop(ui.label(w::text(short_stamp(&m.timestamp), font::TINY, pal.muted()))),
                }
                resp
            })
            .inner;
        actions(ui, pal, emojis, m, &shown, badges, acts);
    });
}

/// Dessine `compact` en emojis couleur (à défaut `text`) en grand au centre de `rect` ; pendant [`POP`] après son arrivée : surgit en
/// dépassant un peu sa taille, puis se balance en s'amortissant.
fn pop(ui: &egui::Ui, emojis: &mut Emojis, rect: egui::Rect, compact: &str, text: &str, ink: egui::Color32, age: Option<f64>) {
    let (scale, angle) = match age {
        Some(a) if a < POP => {
            ui.ctx().request_repaint();
            let a = a as f32;
            // easeOutBack sur 0,45 s : de 0 à 1 en passant par ~1,1.
            let (c1, x) = (1.701_58_f32, (a / 0.45).min(1.0) - 1.0);
            let grow = 1.0 + (c1 + 1.0) * x.powi(3) + c1 * x.powi(2);
            let damp = 1.0 - a / POP as f32;
            (grow.max(0.1), 0.2 * (a * std::f32::consts::TAU * 2.5).sin() * damp)
        }
        _ => (1.0, 0.0),
    };
    if emojis.paint(ui, compact, rect.center(), font::EMOJI * scale, angle, ink) {
        return;
    }
    // Taille arrondie à 2 px : peu de tailles distinctes à tramer dans l'atlas des glyphes.
    let size = (font::EMOJI * scale / 2.0).round().max(1.0) * 2.0;
    let g = ui.painter().layout_no_wrap(text.to_owned(), w::regular(size), ink);
    // Rotation autour du centre : TextShape tourne autour de son coin haut gauche.
    let pos = rect.center() - egui::emath::Rot2::from_angle(angle) * (g.size() / 2.0);
    ui.painter().add(egui::epaint::TextShape::new(pos, g, ink).with_angle(angle));
}

const BADGE_R: f32 = 11.0;
/// Taille d'un emoji de réaction en couleur (un peu sous le diamètre de la pastille).
const BADGE_EMOJI: f32 = 18.0;

/// Badges de réaction à cheval sur le coin haut de la bulle, comme Messages : à droite sur un message
/// reçu, à gauche sur un message envoyé ; emoji en couleur sans pastille, ou en texte sur une pastille
/// (les miens en accent, ceux du correspondant en surface).
fn badges_on(ui: &egui::Ui, pal: &Palette, emojis: &mut Emojis, bubble: egui::Rect, outgoing: bool, badges: &[Badge]) {
    let p = ui.painter();
    let surface = pal.bg.lerp_to_gamma(pal.fg, 0.18);
    for (k, b) in badges.iter().enumerate() {
        // Plusieurs badges se chevauchent un peu vers l'intérieur de la bulle.
        let dx = 4.0 + k as f32 * (BADGE_R * 2.0 - 4.0);
        let c = egui::pos2(if outgoing { bubble.left() + dx } else { bubble.right() - dx }, bubble.top() - 2.0);
        let (fill, ink) = if b.mine { (pal.accent, pal.on_accent) } else { (surface, pal.fg) };
        let a = if b.pending { 0.5 } else { 1.0 };
        // Emoji en couleur posé tel quel ; en texte (sans la police), sur une pastille pour rester lisible.
        if !emojis.paint(ui, b.emoji, c, BADGE_EMOJI, 0.0, ink.gamma_multiply(a)) {
            p.circle(c, BADGE_R, fill.gamma_multiply(a), egui::Stroke::new(2.0, pal.bg));
            p.text(c, egui::Align2::CENTER_CENTER, b.emoji, egui::FontId::proportional(font::BODY), ink.gamma_multiply(a));
        }
    }
}

/// Barre d'envoi indéterminée (3 de haut, accent) : un segment qui défile tant que le transfert dure.
fn progress(ui: &mut egui::Ui, pal: &Palette, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 3.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 1.5, pal.accent.gamma_multiply(0.2));
    let t = (ui.input(|i| i.time) * 0.8).fract() as f32;
    let seg = rect.width() * 0.35;
    let x0 = rect.left() + (rect.width() + seg) * t - seg;
    let bar = egui::Rect::from_x_y_ranges(x0.max(rect.left())..=(x0 + seg).min(rect.right()), rect.y_range());
    p.rect_filled(bar, 1.5, pal.accent);
}

/// En-tête du fil, puis filet : titre, numéro et bouton Appeler à droite ; `true` si ce bouton est cliqué.
fn header(ui: &mut egui::Ui, pal: &Palette, title: &str, sub: &str) -> bool {
    let rect = w::header(ui, pal, "󰭹", title);
    let dial = w::header_button(ui, pal, rect, "󰏲").on_hover_text(t!("Call"));
    ui.painter().text(dial.rect.left_center() - egui::vec2(12.0, 0.0), Align2::RIGHT_CENTER, sub, w::regular(font::SMALL), pal.muted());
    w::rule(ui, pal);
    ui.add_space(12.0);
    dial.clicked()
}

/// Aperçu d'une conversation : appel ou réaction en libellé traduit, sinon première ligne du texte.
pub fn preview(m: &Msg) -> Cow<'_, str> {
    if let Some((k, s)) = m.call {
        return Cow::Owned(call_label(k, s));
    }
    match tapback::parse(&m.text) {
        Some((e, q, cut)) => Cow::Owned(t!("{0} “{1}”", tapback::label(e), if cut { format!("{q}…") } else { q.to_owned() })),
        None => Cow::Borrowed(m.text.lines().next().unwrap_or("")),
    }
}

/// « Appel reçu · 2 min 13 », « Appel manqué ».
fn call_label(kind: CallKind, secs: u64) -> String {
    let what = match kind {
        CallKind::In => t!("Received call"),
        CallKind::Out => t!("Placed call"),
        CallKind::Missed => return t!("Missed call").to_owned(),
    };
    match (secs / 60, secs % 60) {
        (0, s) => t!("{0} · {1} s", what, s),
        (m, s) => t!("{0} · {1} min {2}", what, m, format!("{s:02}")),
    }
}

/// `20261015T103000` → `15/10 10:30` ; autre format rendu tel quel.
pub fn short_stamp(t: &str) -> String {
    if t.len() >= 13 && t.is_ascii() && t.as_bytes()[8] == b'T' {
        // Jour {0}, mois {1}, heure {2}, minutes {3} : ordre selon la langue.
        t!("{1}/{0} {2}:{3}", &t[6..8], &t[4..6], &t[9..11], &t[11..13])
    } else {
        t.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Attendus construits avec `t!` : les tests passent quelle que soit la locale.
    fn stamps() {
        assert_eq!(short_stamp("20261015T103000"), t!("{1}/{0} {2}:{3}", "15", "10", "10", "30"));
        assert_eq!(short_stamp("hier"), "hier");
    }

    #[test]
    fn call_labels() {
        assert_eq!(call_label(CallKind::In, 133), t!("{0} · {1} min {2}", t!("Received call"), 2, "13"));
        assert_eq!(call_label(CallKind::Out, 9), t!("{0} · {1} s", t!("Placed call"), 9));
        assert_eq!(call_label(CallKind::Missed, 0), t!("Missed call"));
    }
}
