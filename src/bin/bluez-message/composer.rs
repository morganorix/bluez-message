//! Message input, like the wifi password: field 34 high, `fg` 10 %, text 12 to 10 from the edge;
//! emoji picker; button 120 × 34. Only edits the draft: sending goes back to the window.

use bluez_message::gui::emoji::Emojis;
use bluez_message::gui::icons::Icon;
use bluez_message::gui::theme::{Palette, size};
use bluez_message::gui::widgets as w;
use bluez_message::t;
use eframe::egui::{self, Key};

const EMOJIS: [&str; 40] = [
    "😀", "😂", "😊", "😍", "😘", "😉", "😎", "🤔", "😅", "😢", "😭", "😡", "😴", "🙄", "😮", "🥳", "🤗", "🙏", "👍", "👎",
    "👌", "👏", "💪", "🤝", "👋", "❤\u{FE0F}", "💔", "🔥", "✨", "🎉", "🎂", "☀\u{FE0F}", "🌙", "⭐", "✅", "❌", "⚠\u{FE0F}", "📞", "📍", "🚗",
];

/// Input request to the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerAction {
    /// Send the draft (Enter or button).
    Send,
    /// Escape: back to the search and the list.
    Leave,
}

#[derive(Default)]
pub struct Composer {
    pub draft: String,
    /// Cursor to place in the input at the next frame.
    pub focus: bool,
}

impl Composer {
    pub fn show(&mut self, ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis) -> Option<ComposerAction> {
        ui.spacing_mut().interact_size.y = size::FIELD_H;
        ui.horizontal(|ui| {
            let mut act = None;
            let w_field = ui.available_width() - size::BUTTON_W - 40.0 - 2.0 * ui.spacing().item_spacing.x;
            let r = emojis.field(ui, &mut self.draft, t!("Message, then Enter  (Esc: list)"), w_field.max(100.0));
            if std::mem::take(&mut self.focus) {
                r.request_focus();
            }
            if r.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                act = Some(ComposerAction::Leave);
            }
            let pick = w::icon_button(ui, pal, Icon::Emoji).on_hover_text(t!("Emoji"));
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
