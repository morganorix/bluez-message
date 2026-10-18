//! Left column: search, conversations, contacts and free number; keyboard navigation.
//! Changes neither the state nor the daemon: returns the chosen actions.

use std::sync::Arc;

use bluez_message::app::state::{Conv, Rev, State, matches, shown_number};
use bluez_message::engine::dial_number;
use bluez_message::numbering::{self, Id};
use bluez_message::gui::emoji::Emojis;
use bluez_message::gui::icons::Icon;
use bluez_message::gui::theme::{Palette, font, size};
use bluez_message::gui::widgets as w;
use bluez_message::t;
use eframe::egui::{self, Key, Margin, ScrollArea, vec2};

use crate::thread::{preview, short_stamp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    None,
    Conv(String),
    /// Contact or number without a conversation (chosen through the search): write or call.
    Contact { name: String, number: String },
}

/// Choice made in the list, applied by the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListAction {
    Open(Target),
    /// Confirmed deletion of a conversation.
    Delete(String),
}

/// List shown: conversations able to receive an SMS, or the calls only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Sms,
    Calls,
}

impl Tab {
    /// `c` belongs to this list: calls for Calls, numbers able to receive an SMS for SMS.
    fn lists(self, c: &Conv, home: Option<Id>) -> bool {
        match self {
            Self::Sms => c.textable(home),
            Self::Calls => c.last_call().is_some(),
        }
    }

    /// Contact `number` offered by the search: its conversation is not listed here, and the SMS tab
    /// leaves out landlines.
    fn offers(self, number: &str, conv: Option<&Conv>, home: Option<Id>) -> bool {
        !conv.is_some_and(|c| self.lists(c, home)) && (self == Self::Calls || !numbering::is_landline(number, home))
    }
}

/// (number, name) pairs matching a search.
type Found = Arc<Vec<(String, String)>>;

pub struct ConvList {
    /// Search (name or number).
    query: String,
    /// Row chosen with the keyboard (conversations, contacts, free number).
    cursor: usize,
    /// List navigated with the keyboard (cursor shown, Enter opens its row); a click interrupts it.
    kbd: bool,
    /// Cursor to place in the search at the next frame.
    pub focus: bool,
    /// Contacts found for (query, state revision): the phonebook is not scanned again at each frame.
    found: Option<(String, Tab, Rev, Found)>,
    pub tab: Tab,
}

impl Default for ConvList {
    fn default() -> Self {
        Self { query: String::new(), cursor: 0, kbd: false, focus: true, found: None, tab: Tab::Sms }
    }
}

impl ConvList {
    /// After opening a row: cursor at the top; search cleared except for a conversation.
    pub fn opened(&mut self, t: &Target) {
        if !matches!(t, Target::Conv(_)) {
            self.query.clear();
        }
        (self.cursor, self.kbd) = (0, false);
    }

    /// Arrows in a list of `len` rows (non-empty); `true` if the cursor moved.
    fn navigate(&mut self, len: usize, down: bool, up: bool) -> bool {
        let moved = down || up;
        if moved {
            self.cursor = if down { (self.cursor + 1).min(len.saturating_sub(1)) } else { self.cursor.saturating_sub(1) };
            self.kbd = true;
        }
        self.cursor = self.cursor.min(len.saturating_sub(1));
        moved
    }

    /// Enter opens the cursor row: validated in the search, or list navigated with the keyboard.
    fn enter_opens(&self, search_left: bool, search_focused: bool) -> bool {
        search_left || (!search_focused && self.kbd)
    }

    /// Search field (34 high): Escape clears it, any input brings the cursor back to the top.
    fn search_field(&mut self, ui: &mut egui::Ui, pal: &Palette) -> egui::Response {
        ui.spacing_mut().interact_size.y = size::FIELD_H;
        let field = w::field(&mut self.query, t!("Name or number  (Ctrl+F)")).margin(Margin { left: 32, ..Margin::symmetric(10, 10) });
        let r = ui.add(field.desired_width(f32::INFINITY));
        w::paint_icon(ui, Icon::Magnify, r.rect.left_center() + vec2(16.0, 0.0), font::ICON, pal.muted());
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

    /// Contacts matching `q` offered in `tab` (see [`Tab::offers`]), cached per query, tab and revision.
    fn contacts(&mut self, state: &State, q: &str, tab: Tab, home: Option<Id>) -> Found {
        let rev = state.rev();
        match &self.found {
            Some((fq, ft, fr, hits)) if fq == q && *ft == tab && *fr == rev => Arc::clone(hits),
            _ => {
                let hits = state.search(q, 50, |n, c| tab.offers(n, c, home));
                let hits: Arc<Vec<_>> = Arc::new(hits.into_iter().map(|(a, b)| (a.to_owned(), b.to_owned())).collect());
                self.found = Some((q.to_owned(), tab, rev, Arc::clone(&hits)));
                hits
            }
        }
    }

    /// `keys`: arrows and Enter are for the list (no message input, menu or Settings).
    pub fn show(&mut self, ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis, state: &State, target: &Target, keys: bool) -> Vec<ListAction> {
        if w::section_action(ui, pal, t!("Contacts"), Icon::Plus).on_hover_text(t!("Write or call: search for a contact")).clicked() {
            self.focus = true;
        }
        let r = self.search_field(ui, pal);
        // Search under "Contacts +", set apart from the tabs that filter the list.
        ui.add_space(3.0 * w::GAP);
        let tabs = [Tab::Sms, Tab::Calls];
        let on = tabs.iter().position(|t| *t == self.tab).unwrap_or_default();
        if let Some(i) = w::tabs(ui, pal, &[t!("SMS"), t!("Calls")], on) {
            (self.tab, self.cursor) = (tabs[i], 0);
        }
        // Arrows: outside message input (search or no active field), they navigate the list.
        let typing = ui.ctx().memory(egui::Memory::focused).is_some_and(|id| id != r.id);
        let nav = keys && !typing && !egui::Popup::is_any_open(ui.ctx());
        let (down, up, enter) = ui.input(|i| (i.key_pressed(Key::ArrowDown), i.key_pressed(Key::ArrowUp), i.key_pressed(Key::Enter)));
        ui.add_space(w::GAP);
        let q = self.query.trim().to_owned();
        let calls = self.tab == Tab::Calls;
        let home = state.home();
        let mut convs: Vec<_> = state
            .convs
            .iter()
            .filter(|c| self.tab.lists(c, home))
            .filter(|c| matches(&q, &c.name, &c.number))
            .collect();
        if calls {
            // Most recent call first (the conversations are sorted by their last item).
            convs.sort_by(|a, b| b.last_call().map(|m| &m.timestamp).cmp(&a.last_call().map(|m| &m.timestamp)));
        }
        // Contacts without a displayed conversation (those are already listed); free number if it can be
        // dialled (and, for SMS, is not a landline).
        let contacts = self.contacts(state, &q, self.tab, home);
        let free = (!q.is_empty() && dial_number(&q).is_ok() && self.tab.offers(&q, None, home) && convs.is_empty() && contacts.is_empty()).then_some(q.as_str());
        if convs.is_empty() && contacts.is_empty() && free.is_none() {
            let empty = if calls { t!("No calls") } else { t!("No conversations") };
            w::hint(ui, pal, if q.is_empty() { empty } else { t!("No results") });
            return vec![];
        }
        // Rows in display order: keyboard cursor target.
        let items: Vec<Target> = convs
            .iter()
            .map(|c| Target::Conv(c.key.clone()))
            .chain(contacts.iter().map(|(name, number)| Target::Contact { name: (*name).clone(), number: (*number).clone() }))
            .chain(free.map(|n| Target::Contact { name: String::new(), number: n.to_owned() }))
            .collect();
        // The search keeps focus during navigation: typing can continue.
        let moved = self.navigate(items.len(), nav && down, nav && up);
        // Cursor visible during a search or keyboard navigation.
        let cursor = (nav && (r.has_focus() || r.lost_focus() || self.kbd || !q.is_empty())).then_some(self.cursor);
        let mut pick = (nav && enter && self.enter_opens(r.lost_focus(), r.has_focus())).then(|| items[self.cursor].clone());
        let mut del: Option<String> = None;
        let mut idx = 0usize;
        let mut line = |ui: &mut egui::Ui, row: &w::Row| {
            let here = cursor == Some(idx);
            // Previews: colour emojis, recognisable.
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
                let title = if c.name.is_empty() { shown_number(Some(&c.number)) } else { &c.name };
                let last = if calls { c.last_call() } else { c.msgs.last() };
                let preview = last.map(preview).unwrap_or_default();
                let stamp = last.map(|m| short_stamp(&m.timestamp)).unwrap_or_default();
                let unread = if calls { state.unread_calls(c) } else { c.unread };
                let row = w::Row {
                    icon: Some(if calls {
                        Icon::Phone
                    } else if unread > 0 {
                        Icon::Message
                    } else {
                        Icon::Chat
                    }),
                    label: title,
                    sub: Some(&preview),
                    hint: &stamp,
                    badge: unread,
                    selected: matches!(target, Target::Conv(k) if *k == c.key),
                    strong: unread > 0,
                };
                let resp = line(ui, &row);
                if resp.clicked() {
                    pick = Some(Target::Conv(c.key.clone()));
                }
                // Deleting a conversation would also delete its SMS: calls are deleted one by one in the thread.
                if !calls {
                    resp.context_menu(|ui| {
                        ui.menu_image_text_button(w::small_icon(Icon::Delete, ui.visuals().text_color()), t!("Delete conversation…"), |ui| {
                            if ui.button(t!("Confirm deletion")).clicked() {
                                del = Some(c.key.clone());
                            }
                        });
                    });
                }
            }
            if !contacts.is_empty() || free.is_some() {
                ui.add_space(w::GAP * 2.0);
                ui.label(w::text(t!("Phonebook"), font::SMALL, pal.muted()));
                ui.add_space(w::GAP);
            }
            for (name, number) in contacts.iter() {
                let row = w::Row { icon: Some(Icon::Account), label: name, sub: Some(number), hint: "", badge: 0, selected: false, strong: false };
                if line(ui, &row).clicked() {
                    pick = Some(Target::Contact { name: name.to_owned(), number: number.to_owned() });
                }
            }
            if let Some(n) = free {
                let row = w::Row { icon: Some(Icon::Plus), label: n, sub: Some(t!("New number")), hint: "", badge: 0, selected: false, strong: false };
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
        // Shortened list (search): cursor brought back to the last row.
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
        // Search active without validation: Enter stays with the input.
        assert!(!l.enter_opens(false, true));
    }

    #[test]
    fn opening_resets_cursor_and_query() {
        let mut l = ConvList { query: "ma".into(), cursor: 2, kbd: true, focus: false, found: None, tab: Tab::Calls };
        l.opened(&Target::Conv("k".into()));
        assert_eq!((l.query.as_str(), l.cursor, l.kbd), ("ma", 0, false));
        l.opened(&Target::Contact { name: "Ma".into(), number: "+331".into() });
        assert_eq!(l.query, "");
    }

    #[test]
    fn sms_tab_leaves_out_landlines() {
        let fr = Some(Id::FR);
        assert!(!Tab::Sms.offers("03 89 00 00 00", None, fr));
        assert!(Tab::Calls.offers("03 89 00 00 00", None, fr));
        assert!(Tab::Sms.offers("06 00 00 00 00", None, fr));
        assert!(Tab::Sms.offers("03 89 00 00 00", None, None), "home unknown: kept");
    }
}
