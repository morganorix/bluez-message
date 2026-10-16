//! Saisie du message, comme le mot de passe wifi : champ 34 de haut, `fg` 10 %, texte 12 à 10 du bord ;
//! choix d'emojis ; bouton 120 × 34. Ne fait qu'éditer le brouillon : l'envoi revient à la fenêtre.

use bluez_message::gui::emoji::Emojis;
use bluez_message::gui::theme::{Palette, size};
use bluez_message::gui::widgets as w;
use bluez_message::t;
use eframe::egui::{self, Key};

const EMOJIS: [&str; 40] = [
    "😀", "😂", "😊", "😍", "😘", "😉", "😎", "🤔", "😅", "😢", "😭", "😡", "😴", "🙄", "😮", "🥳", "🤗", "🙏", "👍", "👎",
    "👌", "👏", "💪", "🤝", "👋", "❤\u{FE0F}", "💔", "🔥", "✨", "🎉", "🎂", "☀\u{FE0F}", "🌙", "⭐", "✅", "❌", "⚠\u{FE0F}", "📞", "📍", "🚗",
];

/// Demande de la saisie à la fenêtre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerAction {
    /// Envoyer le brouillon (Entrée ou bouton).
    Send,
    /// Échap : retour à la recherche et à la liste.
    Leave,
}

#[derive(Default)]
pub struct Composer {
    pub draft: String,
    /// Curseur à placer dans la saisie à la prochaine image.
    pub focus: bool,
}

impl Composer {
    pub fn show(&mut self, ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis) -> Option<ComposerAction> {
        ui.spacing_mut().interact_size.y = size::FIELD_H;
        ui.horizontal(|ui| {
            let mut act = None;
            let w_field = ui.available_width() - size::BUTTON_W - 40.0 - 2.0 * ui.spacing().item_spacing.x;
            let r = ui.add(w::field(&mut self.draft, t!("Message, then Enter  (Esc: list)")).desired_width(w_field.max(100.0)));
            if std::mem::take(&mut self.focus) {
                r.request_focus();
            }
            if r.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                act = Some(ComposerAction::Leave);
            }
            let pick = w::icon_button(ui, pal, "󰞅").on_hover_text(t!("Emoji"));
            egui::Popup::from_toggle_button_response(&pick).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                egui::Grid::new("emojis").spacing(egui::vec2(2.0, 2.0)).show(ui, |ui| {
                    for (i, e) in EMOJIS.iter().enumerate() {
                        if ui.add(emojis.button(ui.ctx(), e, 22.0, pal.fg).frame(false).min_size(egui::vec2(30.0, 30.0))).clicked() {
                            self.draft.push_str(e);
                            self.focus = true;
                        }
                        if i % 10 == 9 {
                            ui.end_row();
                        }
                    }
                });
            });
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if w::button(ui, pal, t!("SEND"), w::Tone::Accent).clicked() || enter {
                act = Some(ComposerAction::Send);
                r.request_focus();
            }
            act
        })
        .inner
    }
}
