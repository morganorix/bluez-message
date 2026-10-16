//! Colonne de gauche : recherche, conversations, contacts et numéro libre ; parcours au clavier.
//! Ne modifie ni l'état ni le démon : rend les actions choisies.

use bluez_message::app::state::{State, matches};
use bluez_message::engine::dial_number;
use bluez_message::gui::emoji::Emojis;
use bluez_message::gui::theme::{Palette, font, size};
use bluez_message::gui::widgets as w;
use bluez_message::t;
use eframe::egui::{self, Key, ScrollArea};

use crate::thread::{preview, short_stamp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    None,
    Conv(String),
    /// Contact ou numéro sans conversation (choisi par la recherche) : écrire ou appeler.
    Contact { name: String, number: String },
}

/// Choix fait dans la liste, appliqué par la fenêtre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListAction {
    Open(Target),
    /// Suppression confirmée d'une conversation.
    Delete(String),
}

pub struct ConvList {
    /// Recherche (nom ou numéro).
    query: String,
    /// Ligne choisie au clavier (conversations, contacts, numéro libre).
    cursor: usize,
    /// Liste parcourue au clavier (curseur affiché, Entrée ouvre sa ligne) ; un clic l'interrompt.
    kbd: bool,
    /// Curseur à placer dans la recherche à la prochaine image.
    pub focus: bool,
}

impl Default for ConvList {
    fn default() -> Self {
        Self { query: String::new(), cursor: 0, kbd: false, focus: true }
    }
}

impl ConvList {
    /// Après l'ouverture d'une ligne : curseur en tête ; recherche vidée sauf pour une conversation.
    pub fn opened(&mut self, t: &Target) {
        if !matches!(t, Target::Conv(_)) {
            self.query.clear();
        }
        (self.cursor, self.kbd) = (0, false);
    }

    /// Flèches dans une liste de `len` lignes (non vide) ; `true` si le curseur a bougé.
    fn navigate(&mut self, len: usize, down: bool, up: bool) -> bool {
        let moved = down || up;
        if moved {
            self.cursor = if down { (self.cursor + 1).min(len - 1) } else { self.cursor.saturating_sub(1) };
            self.kbd = true;
        }
        self.cursor = self.cursor.min(len - 1);
        moved
    }

    /// Entrée ouvre la ligne du curseur : validée dans la recherche, ou liste parcourue au clavier.
    fn enter_opens(&self, search_left: bool, search_focused: bool) -> bool {
        search_left || (!search_focused && self.kbd)
    }

    /// Champ de recherche (34 de haut) : Échap le vide, toute saisie ramène le curseur en tête.
    fn search_field(&mut self, ui: &mut egui::Ui) -> egui::Response {
        ui.spacing_mut().interact_size.y = size::FIELD_H;
        let r = ui.add(w::field(&mut self.query, &format!("󰍉  {}", t!("Name or number  (Ctrl+F)"))).desired_width(f32::INFINITY));
        if std::mem::take(&mut self.focus) {
            r.request_focus();
        }
        if r.changed() {
            self.cursor = 0;
        }
        if r.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
            self.query.clear();
            self.cursor = 0;
        }
        r
    }

    /// `keys` : les flèches et Entrée sont pour la liste (pas de saisie de message, de menu ni de Paramètres).
    pub fn show(&mut self, ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis, state: &State, target: &Target, keys: bool) -> Vec<ListAction> {
        if w::section_action(ui, pal, t!("Conversations"), "󰐕").on_hover_text(t!("Write or call: search for a contact")).clicked() {
            self.focus = true;
        }
        ui.add_space(w::GAP);
        let r = self.search_field(ui);
        // Flèches : hors saisie du message (recherche ou aucun champ actif), elles parcourent la liste.
        let typing = ui.ctx().memory(egui::Memory::focused).is_some_and(|id| id != r.id);
        let nav = keys && !typing && !egui::Popup::is_any_open(ui.ctx());
        let (down, up, enter) = ui.input(|i| (i.key_pressed(Key::ArrowDown), i.key_pressed(Key::ArrowUp), i.key_pressed(Key::Enter)));
        ui.add_space(w::GAP);
        let q = self.query.trim().to_owned();
        let convs: Vec<_> = state.convs.iter().filter(|c| matches(&q, &c.name, &c.number)).collect();
        // Contacts sans conversation affichée (celles-ci sont déjà listées) ; numéro libre s'il est composable.
        let contacts: Vec<_> = state.search(&q, 50);
        let free = (!q.is_empty() && dial_number(&q).is_ok() && convs.is_empty() && contacts.is_empty()).then_some(q.as_str());
        if convs.is_empty() && contacts.is_empty() && free.is_none() {
            w::hint(ui, pal, if q.is_empty() { t!("No conversations") } else { t!("No results") });
            return vec![];
        }
        // Lignes dans l'ordre d'affichage : cible du curseur clavier.
        let items: Vec<Target> = convs
            .iter()
            .map(|c| Target::Conv(c.key.clone()))
            .chain(contacts.iter().map(|(name, number)| Target::Contact { name: (*name).to_owned(), number: (*number).to_owned() }))
            .chain(free.map(|n| Target::Contact { name: String::new(), number: n.to_owned() }))
            .collect();
        // La recherche garde le focus pendant le parcours : on peut continuer à taper.
        let moved = self.navigate(items.len(), nav && down, nav && up);
        // Curseur visible pendant une recherche ou un parcours au clavier.
        let cursor = (nav && (r.has_focus() || r.lost_focus() || self.kbd || !q.is_empty())).then_some(self.cursor);
        let mut pick = (nav && enter && self.enter_opens(r.lost_focus(), r.has_focus())).then(|| items[self.cursor].clone());
        let mut del: Option<String> = None;
        let mut idx = 0usize;
        let mut line = |ui: &mut egui::Ui, row: &w::Row| {
            let here = cursor == Some(idx);
            // Aperçus : emojis en couleur, reconnaissables.
            let resp = w::row_with(ui, pal, &w::Row { selected: row.selected || here, ..*row }, Some(&mut *emojis));
            if here && moved {
                resp.scroll_to_me(None);
            }
            idx += 1;
            resp
        };
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = w::ROW_GAP;
            for c in convs {
                let title = if c.name.is_empty() { &c.number } else { &c.name };
                let last = c.msgs.last();
                let preview = last.map(preview).unwrap_or_default();
                let stamp = last.map(|m| short_stamp(&m.timestamp)).unwrap_or_default();
                let row = w::Row {
                    icon: if c.unread > 0 { "󰍡" } else { "󰭹" },
                    label: title,
                    sub: Some(&preview),
                    hint: &stamp,
                    badge: c.unread,
                    selected: matches!(target, Target::Conv(k) if *k == c.key),
                    strong: c.unread > 0,
                };
                let resp = line(ui, &row);
                if resp.clicked() {
                    pick = Some(Target::Conv(c.key.clone()));
                }
                resp.context_menu(|ui| {
                    ui.menu_button(format!("󰆴  {}", t!("Delete conversation…")), |ui| {
                        if ui.button(t!("Confirm deletion")).clicked() {
                            del = Some(c.key.clone());
                        }
                    });
                });
            }
            if !contacts.is_empty() || free.is_some() {
                ui.add_space(w::GAP * 2.0);
                ui.label(w::text(t!("Contacts"), font::SMALL, pal.muted()));
                ui.add_space(w::GAP);
            }
            for (name, number) in contacts {
                let row = w::Row { icon: "󰀄", label: name, sub: Some(number), hint: "", badge: 0, selected: false, strong: false };
                if line(ui, &row).clicked() {
                    pick = Some(Target::Contact { name: name.to_owned(), number: number.to_owned() });
                }
            }
            if let Some(n) = free {
                let row = w::Row { icon: "󰐕", label: n, sub: Some(t!("New number")), hint: "", badge: 0, selected: false, strong: false };
                if line(ui, &row).clicked() {
                    pick = Some(Target::Contact { name: String::new(), number: n.to_owned() });
                }
            }
        });
        del.map(ListAction::Delete).into_iter().chain(pick.map(ListAction::Open)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_stays_in_bounds() {
        let mut l = ConvList::default();
        assert!(!l.navigate(3, false, false));
        assert!(!l.kbd);
        assert!(l.navigate(3, false, true));
        assert_eq!(l.cursor, 0);
        for _ in 0..5 {
            l.navigate(3, true, false);
        }
        assert_eq!(l.cursor, 2);
        assert!(l.kbd);
        // Liste raccourcie (recherche) : curseur ramené sur la dernière ligne.
        assert!(!l.navigate(1, false, false));
        assert_eq!(l.cursor, 0);
    }

    #[test]
    fn enter_opens_from_search_or_keyboard() {
        let mut l = ConvList::default();
        assert!(l.enter_opens(true, false));
        assert!(!l.enter_opens(false, false));
        assert!(!l.enter_opens(false, true));
        l.navigate(2, true, false);
        assert!(l.enter_opens(false, false));
        // Recherche active sans validation : Entrée reste à la saisie.
        assert!(!l.enter_opens(false, true));
    }

    #[test]
    fn opening_resets_cursor_and_query() {
        let mut l = ConvList { query: "ma".into(), cursor: 2, kbd: true, focus: false };
        l.opened(&Target::Conv("k".into()));
        assert_eq!((l.query.as_str(), l.cursor, l.kbd), ("ma", 0, false));
        l.opened(&Target::Contact { name: "Ma".into(), number: "+331".into() });
        assert!(l.query.is_empty());
    }
}
