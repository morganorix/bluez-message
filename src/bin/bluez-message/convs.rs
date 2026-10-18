//! Left column: search, conversations, contacts and free number; keyboard navigation.
//! Changes neither the state nor the daemon: returns the chosen actions.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bluez_message::app::state::{Conv, Hits, Rev, State, matches, shown_number};
use bluez_message::engine::{ConvFlags, Hit, dial_number};
use bluez_message::numbering::{self, Id};
use bluez_message::phone::excerpt;
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
    /// Confirmed deletion of the calls of a conversation, its SMS kept.
    DeleteCalls(String),
    /// Search through the whole history (typed text, settled).
    Search(String),
    /// Following page of the history search.
    More,
    /// Opens the conversation of a history search result on it.
    Jump(Hit),
    /// New muted / archived state of a conversation.
    Flags(String, ConvFlags),
}

/// Pause after the last key before searching the whole history.
const SEARCH_DELAY: Duration = Duration::from_millis(300);
/// Characters kept on each side of the match in a history search result.
const AROUND: usize = 24;

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
    /// Last change of the search, not yet sent to the history search.
    typed: Option<Instant>,
    /// "Archived" section unfolded.
    archived: bool,
}

impl Default for ConvList {
    fn default() -> Self {
        Self { query: String::new(), cursor: 0, kbd: false, focus: true, found: None, tab: Tab::Sms, typed: None, archived: false }
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
        let field = w::field(&mut self.query, t!("Search: SMS or contact  (Ctrl+F)")).margin(Margin { left: 32, ..Margin::symmetric(10, 10) });
        let r = ui.add(field.desired_width(f32::INFINITY));
        w::paint_icon(ui, Icon::Magnify, r.rect.left_center() + vec2(16.0, 0.0), font::ICON, pal.muted());
        if std::mem::take(&mut self.focus) {
            r.request_focus();
        }
        if r.changed() {
            (self.cursor, self.typed) = (0, Some(Instant::now()));
        }
        if r.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
            self.query.clear();
            (self.cursor, self.typed) = (0, Some(Instant::now()));
        }
        r
    }

    /// History search of `q` once typing has settled for [`SEARCH_DELAY`] (at once when too short,
    /// to drop the results); a repaint is planned meanwhile.
    fn settled(&mut self, ctx: &egui::Context, q: &str) -> Option<ListAction> {
        let wait = SEARCH_DELAY.saturating_sub(self.typed?.elapsed());
        if q.chars().count() < 2 || wait.is_zero() {
            self.typed = None;
            return Some(ListAction::Search(q.to_owned()));
        }
        ctx.request_repaint_after(wait);
        None
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

    /// Header of the "Archived" section, unfolds or folds it.
    fn archived_header(&mut self, ui: &mut egui::Ui, pal: &Palette, n: usize) {
        let icon = if self.archived { Icon::ChevronUp } else { Icon::ChevronDown };
        if w::section_action(ui, pal, &t!("Archived ({0})", n), icon).clicked() {
            self.archived = !self.archived;
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
        let search = self.settled(ui.ctx(), &q);
        let calls = self.tab == Tab::Calls;
        // History search results (SMS only), for the query shown.
        let hits = Some(&state.hits).filter(|h| !calls && h.query == q && (h.busy || !h.items.is_empty()));
        let home = state.home();
        let (mut convs, archived): (Vec<_>, Vec<_>) = state
            .convs
            .iter()
            .filter(|c| self.tab.lists(c, home))
            .filter(|c| matches(&q, &c.name, &c.number))
            // SMS tab: archived conversations set apart, except in a search.
            .partition(|c| calls || !q.is_empty() || !state.flags(&c.key).archived);
        // Folded: neither shown nor reachable with the keyboard.
        let folded = archived.len();
        if self.archived {
            convs.extend(archived);
        }
        let first_archived = convs.len() - if self.archived { folded } else { 0 };
        if calls {
            // Most recent call first (the conversations are sorted by their last item).
            convs.sort_by(|a, b| b.last_call().map(|m| &m.timestamp).cmp(&a.last_call().map(|m| &m.timestamp)));
        }
        // Contacts without a displayed conversation (those are already listed); free number if it can be
        // dialled (and, for SMS, is not a landline).
        let contacts = self.contacts(state, &q, self.tab, home);
        let free = (!q.is_empty() && dial_number(&q).is_ok() && self.tab.offers(&q, None, home) && convs.is_empty() && contacts.is_empty()).then_some(q.as_str());
        if convs.is_empty() && folded == 0 && contacts.is_empty() && free.is_none() && hits.is_none() {
            let empty = if calls { t!("No calls") } else { t!("No conversations") };
            w::hint(ui, pal, if q.is_empty() { empty } else { t!("No results") });
            return search.into_iter().collect();
        }
        // Rows in display order (conversations, contacts, free number): keyboard cursor target, built only when picked.
        let rows = convs.len() + contacts.len() + usize::from(free.is_some());
        let row_target = |i: usize| match i.checked_sub(convs.len()) {
            None => Target::Conv(convs[i].key.clone()),
            Some(j) => contacts.get(j).map_or_else(
                || Target::Contact { name: String::new(), number: free.unwrap_or_default().to_owned() },
                |(name, number)| Target::Contact { name: name.clone(), number: number.clone() },
            ),
        };
        // The search keeps focus during navigation: typing can continue.
        let moved = self.navigate(rows, nav && down, nav && up);
        // Cursor visible during a search or keyboard navigation.
        let cursor = (nav && (r.has_focus() || r.lost_focus() || self.kbd || !q.is_empty())).then_some(self.cursor);
        let mut pick = (nav && enter && rows > 0 && self.enter_opens(r.lost_focus(), r.has_focus())).then(|| row_target(self.cursor));
        let (mut jump, mut more) = (None, false);
        let mut act: Option<ListAction> = None;
        let mut idx = 0usize;
        let mut line = |ui: &mut egui::Ui, emojis: &mut Emojis, row: &w::Row| {
            let here = cursor == Some(idx);
            // Previews: colour emojis, recognisable.
            let resp = w::row_with(ui, pal, &w::Row { selected: row.selected || here, ..*row }, Some(emojis));
            if here && moved {
                resp.scroll_to_me(None);
            }
            idx += 1;
            resp
        };
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = w::ROW_GAP;
            for (i, c) in convs.into_iter().enumerate() {
                if i == first_archived {
                    self.archived_header(ui, pal, folded);
                }
                let flags = state.flags(&c.key);
                let title = if c.name.is_empty() { shown_number(Some(&c.number)) } else { &c.name };
                let last = if calls { c.last_call() } else { c.msgs.last() };
                let preview = last.map(preview).unwrap_or_default();
                let stamp = last.map(|m| short_stamp(&m.timestamp)).unwrap_or_default();
                let unread = if calls { state.unread_calls(c) } else { c.unread };
                let row = w::Row {
                    icon: Some(if calls {
                        Icon::Phone
                    } else if flags.muted {
                        Icon::BellOff
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
                let resp = line(ui, emojis, &row);
                if resp.clicked() {
                    pick = Some(Target::Conv(c.key.clone()));
                }
                // In the Calls tab, only the calls go: the SMS of the conversation stay.
                let label = if calls { t!("Delete call history…") } else { t!("Delete conversation…") };
                resp.context_menu(|ui| {
                    // Calls always ring: muting and archiving are for the SMS.
                    if !calls {
                        let (icon, label) = if flags.muted { (Icon::Bell, t!("Unmute")) } else { (Icon::BellOff, t!("Mute")) };
                        if w::menu_item(ui, icon, label).clicked() {
                            act = Some(ListAction::Flags(c.key.clone(), ConvFlags { muted: !flags.muted, ..flags }));
                        }
                        let (icon, label) = if flags.archived { (Icon::Unarchive, t!("Unarchive")) } else { (Icon::Archive, t!("Archive")) };
                        if w::menu_item(ui, icon, label).clicked() {
                            act = Some(ListAction::Flags(c.key.clone(), ConvFlags { archived: !flags.archived, ..flags }));
                        }
                    }
                    ui.menu_image_text_button(w::small_icon(Icon::Delete, ui.visuals().text_color()), label, |ui| {
                        if ui.button(t!("Confirm deletion")).clicked() {
                            let key = c.key.clone();
                            act = Some(if calls { ListAction::DeleteCalls(key) } else { ListAction::Delete(key) });
                        }
                    });
                });
            }
            if folded > 0 && !self.archived {
                self.archived_header(ui, pal, folded);
            }
            if !contacts.is_empty() || free.is_some() {
                ui.add_space(w::GAP * 2.0);
                ui.label(w::text(t!("Phonebook"), font::SMALL, pal.muted()));
                ui.add_space(w::GAP);
            }
            for (name, number) in contacts.iter() {
                let row = w::Row { icon: Some(Icon::Account), label: name, sub: Some(number), hint: "", badge: 0, selected: false, strong: false };
                if line(ui, emojis, &row).clicked() {
                    pick = Some(Target::Contact { name: name.to_owned(), number: number.to_owned() });
                }
            }
            if let Some(n) = free {
                let row = w::Row { icon: Some(Icon::Plus), label: n, sub: Some(t!("New number")), hint: "", badge: 0, selected: false, strong: false };
                if line(ui, emojis, &row).clicked() {
                    pick = Some(Target::Contact { name: String::new(), number: n.to_owned() });
                }
            }
            if let Some(h) = hits {
                (jump, more) = found_messages(ui, pal, emojis, state, h, &q);
            }
        });
        let found = jump.map(ListAction::Jump).into_iter().chain(more.then_some(ListAction::More));
        search.into_iter().chain(act).chain(pick.map(ListAction::Open)).chain(found).collect()
    }
}

/// "Messages" section: history search results (contact, match in bold, date), then "More results" or
/// the page on its way. Returns the result clicked and whether the next page is asked.
fn found_messages(ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis, state: &State, h: &Hits, q: &str) -> (Option<Hit>, bool) {
    ui.add_space(w::GAP * 2.0);
    ui.label(w::text(t!("Messages"), font::SMALL, pal.muted()));
    ui.add_space(w::GAP);
    let mut jump = None;
    for hit in &h.items {
        let name = state.name_of(&hit.number);
        let (sub, mark) = excerpt(&hit.text, q, AROUND);
        let stamp = short_stamp(&hit.timestamp);
        let label = name.as_deref().unwrap_or_else(|| shown_number(Some(&hit.number)));
        let row = w::Row { icon: Some(Icon::MessageText), label, sub: Some(&sub), hint: &stamp, badge: 0, selected: false, strong: false };
        if w::hit_row(ui, pal, &row, mark, emojis).clicked() {
            jump = Some(hit.clone());
        }
    }
    ui.add_space(w::GAP);
    let more = if h.busy {
        w::hint(ui, pal, t!("Searching…"));
        false
    } else {
        h.next.is_some() && w::button(ui, pal, t!("More results"), w::Tone::Accent).clicked()
    };
    (jump, more)
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
        let mut l = ConvList { query: "ma".into(), cursor: 2, kbd: true, focus: false, found: None, tab: Tab::Calls, typed: None, archived: false };
        l.opened(&Target::Conv("k".into()));
        assert_eq!((l.query.as_str(), l.cursor, l.kbd), ("ma", 0, false));
        l.opened(&Target::Contact { name: "Ma".into(), number: "+331".into() });
        assert_eq!(l.query, "");
    }

    #[test]
    fn found_messages_section_asks_more() {
        use bluez_message::engine::Cursor;
        use egui_kittest::kittest::Queryable;
        let hit = Hit { id: 1, key: "600000001".into(), number: "+33600000001".into(), timestamp: "20261015T103000".into(), text: "☀️ Un bel été 🌊".into(), outgoing: false };
        let h = Hits { query: "ete".into(), items: vec![hit], next: Some(Cursor { ts: "20261015T103000".into(), id: 1 }), busy: false };
        let state = State::default();
        let (mut more, mut jump) = (false, None);
        // Fonts set at the first frame are bound from the next one only (`bold` family of the match).
        let mut fonts = false;
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            if !std::mem::replace(&mut fonts, true) {
                ui.ctx().set_fonts(bluez_message::gui::theme::fonts());
                ui.ctx().request_repaint();
                return;
            }
            let (j, m) = found_messages(ui, &Palette::default(), &mut Emojis::default(), &state, &h, "ete");
            more |= m;
            if j.is_some() {
                jump = j;
            }
        });
        harness.get_by_label(t!("Messages"));
        harness.get_by_label(t!("More results")).click();
        harness.run();
        harness.get_by_label("+33600000001").click();
        harness.run();
        drop(harness);
        assert!(more);
        assert_eq!(jump.map(|j| j.id), Some(1));
    }

    #[test]
    fn archived_section_and_flags_menu() {
        use bluez_message::engine::Out;
        use bluez_message::map::{Folder, Sms};
        use egui_kittest::kittest::Queryable;
        let mut state = State::default();
        for (handle, address) in [("1", "+33600000001"), ("2", "+33600000002")] {
            let sms = Sms { handle: handle.into(), address: address.into(), timestamp: format!("20261015T10000{handle}"), text: "a".into(), read: true, ..Sms::default() };
            state.apply(Out::Sms { sms, folder: Folder::Inbox, new: false });
        }
        let (open, kept) = (state.key("+33600000001"), state.key("+33600000002"));
        state.apply(Out::Conv { key: kept.clone(), flags: ConvFlags { muted: false, archived: true } });
        let title = |k: &str| shown_number(Some(&state.conv(k).unwrap().number)).to_owned();
        let (open_title, kept_title) = (title(&open), title(&kept));
        let (mut list, mut emojis, mut acts) = (ConvList::default(), Emojis::default(), vec![]);
        let mut fonts = false;
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            if !std::mem::replace(&mut fonts, true) {
                ui.ctx().set_fonts(bluez_message::gui::theme::fonts());
                ui.ctx().request_repaint();
                return;
            }
            acts.extend(list.show(ui, &Palette::default(), &mut emojis, &state, &Target::None, true));
        });
        harness.get_by_label(&open_title);
        assert!(harness.query_by_label(&kept_title).is_none(), "archived: folded");
        harness.get_by_label(&t!("Archived ({0})", 1)).click();
        harness.run();
        harness.get_by_label(&kept_title).click_secondary();
        harness.run();
        harness.get_by_label(t!("Unarchive"));
        harness.get_by_label(t!("Mute")).click();
        harness.run();
        drop(harness);
        assert!(acts.contains(&ListAction::Flags(kept, ConvFlags { muted: true, archived: true })), "{acts:?}");
    }

    #[test]
    fn found_messages_shows_the_page_on_its_way() {
        use egui_kittest::kittest::Queryable;
        let h = Hits { query: "ete".into(), busy: true, ..Hits::default() };
        let state = State::default();
        let harness = egui_kittest::Harness::new_ui(|ui| {
            found_messages(ui, &Palette::default(), &mut Emojis::default(), &state, &h, "ete");
        });
        harness.get_by_label(t!("Searching…"));
        assert!(harness.query_by_label(t!("More results")).is_none());
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
