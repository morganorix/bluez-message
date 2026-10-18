//! Settings view (right panel), in tabs: General (language, text size), Notifications, Devices, History,
//! Replies (quick replies of declined calls).
//! The settings belong to the daemon: each change goes out as a command, the display follows its reply.

use std::time::{Duration, Instant};

use bluez_message::addr::BtAddr;
use bluez_message::app::state::Prefs;
use bluez_message::engine::Cmd;
use bluez_message::gui::icons::Icon;
use bluez_message::gui::theme::{CORNER, Palette, font, size};
use bluez_message::gui::widgets as w;
use bluez_message::i18n;
use bluez_message::numbering;
use bluez_message::settings::{QUICK_REPLIES_MAX, QUICK_REPLY_LEN, RETENTIONS, Settings, TextSize, clean_reply};
use bluez_message::t;
use eframe::egui::{self, Key, ScrollArea};

/// Time during which an action awaiting confirmation waits for its second trigger.
const ARM: Duration = Duration::from_secs(5);
/// Second trigger too close to the first (accidental double click): ignored.
const ARM_MIN: Duration = Duration::from_millis(300);

/// Action to confirm (second click or second Enter).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Armed {
    Device(BtAddr),
    Clear,
}

/// What the window does with the view after this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Stay,
    Close,
    /// Setup wizard requested (Devices tab).
    Setup,
}

#[derive(Default)]
pub struct View {
    /// Tab shown (index in `tabs()`).
    tab: usize,
    /// Row chosen with the keyboard; `None`: mouse.
    cursor: Option<usize>,
    armed: Option<(Armed, Instant)>,
    /// Drop-down list open, with its row chosen with the keyboard.
    menu: Option<(Menu, usize)>,
    /// The list scrolls to its keyboard row (just opened or moved).
    follow: bool,
    /// Quick reply being edited (Replies tab): its index (`None`: new one), the typed text and the original one.
    edit: Option<(Option<usize>, String, String)>,
    /// The editor field takes the focus at its first frame.
    focus_edit: bool,
}

/// Index of the Replies tab in `tabs()`.
const REPLIES: usize = 4;

/// Editor button pressed.
enum Edit {
    Save,
    First,
    Delete,
    Cancel,
}

/// Settings chosen from a drop-down list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Menu {
    Lang,
    Country,
}

/// Choices of a list: stored value (`None`: automatic) and label.
type Choices<'a> = Vec<(Option<&'a str>, &'a str)>;

/// View row: its rendering and what it triggers.
struct Item<'a> {
    row: w::Row<'a>,
    ctl: Ctl,
    act: Act,
}

/// Command shown on the right of the row.
enum Ctl {
    None,
    /// Switch and its state.
    Switch(bool),
    /// Text size choice and the size in effect.
    Size(TextSize),
}

/// Section: title and rows.
type Section<'a> = (&'a str, Vec<Item<'a>>);

/// Tab labels, in display order.
fn tabs() -> [&'static str; 5] {
    [t!("General"), t!("Notifications"), t!("Devices"), t!("History"), t!("Replies")]
}

#[derive(Clone)]
enum Act {
    Menu(Menu),
    Size(TextSize),
    LinkPreviews,
    Sms,
    Calls,
    Preview,
    Device(BtAddr),
    Reconnect,
    AutoConnect,
    Retention(Option<u32>),
    Purge,
    Clear,
    Setup,
    /// Edits this quick reply (`None`: adds one).
    Reply(Option<usize>),
}

/// Key pressed at this frame, auto-repeat excluded: holding Enter confirms nothing.
fn pressed(ui: &egui::Ui, key: Key) -> bool {
    ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { key: k, pressed: true, repeat: false, .. } if *k == key)))
}

impl View {
    /// Rendering; returns the commands to send to the daemon and what the window does next.
    pub fn show(&mut self, ui: &mut egui::Ui, pal: &Palette, prefs: Option<&Prefs>, links: (bool, bool, bool)) -> (Vec<Cmd>, Nav) {
        if self.armed.as_ref().is_some_and(|(_, t)| t.elapsed() > ARM) {
            self.armed = None;
        }
        if self.armed.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        // Keyboard only if no field has focus.
        let free = ui.ctx().memory(egui::Memory::focused).is_none() && !egui::Popup::is_any_open(ui.ctx());
        let (down, up, go, esc) = if free {
            let (d, u) = ui.input(|i| (i.key_pressed(Key::ArrowDown), i.key_pressed(Key::ArrowUp)));
            (d, u, pressed(ui, Key::Enter) || pressed(ui, Key::Space), pressed(ui, Key::Escape))
        } else {
            (false, false, false, false)
        };
        let (left, right) = if free && self.menu.is_none() { ui.input(|i| (i.key_pressed(Key::ArrowLeft), i.key_pressed(Key::ArrowRight))) } else { (false, false) };
        let rect = w::header_with(ui, pal, Icon::Cog, t!("SETTINGS"), 60.0);
        let mut close = w::header_button(ui, pal, rect, Icon::Close).on_hover_text(t!("Close (Esc)")).clicked();
        w::rule(ui, pal);
        ui.add_space(12.0);
        let Some(p) = prefs else {
            w::hint(ui, pal, t!("Settings unavailable: daemon not connected"));
            return (vec![], if close || esc { Nav::Close } else { Nav::Stay });
        };
        let labels = tabs();
        // Click: mouse mode; arrows: keyboard cursor on the first row of the tab.
        let switch = match w::tabs(ui, pal, &labels, self.tab) {
            Some(i) => Some((i, None)),
            None if left => Some((self.tab.saturating_sub(1), Some(0))),
            None if right => Some(((self.tab + 1).min(labels.len() - 1), Some(0))),
            None => None,
        };
        if let Some((i, cursor)) = switch {
            (self.tab, self.cursor, self.armed, self.menu, self.edit) = (i, cursor, None, None, None);
        }
        ui.add_space(w::GAP);
        let s = &p.settings;
        let mut cmds = if self.tab == REPLIES { self.editor(ui, pal, s) } else { vec![] };

        // Language choice: system, then each available language under its name.
        let langs: Choices =
            std::iter::once((None, t!("System language"))).chain(p.langs.iter().map(|c| (Some(c.as_str()), i18n::native_name(c)))).collect();
        let lang = langs.iter().position(|(c, _)| *c == s.lang.as_deref()).unwrap_or(0);
        // Country choice: automatic (the one of the chosen language, else of the system locale, as the daemon
        // resolves it), then each country.
        let auto = p.auto.map_or_else(|| t!("Automatic").to_owned(), |id| t!("Automatic ({0})", numbering::label(id)));
        let countries: Choices = std::iter::once((None, auto.as_str())).chain(numbering::countries().iter().map(|(id, l)| (Some(id.as_ref()), l.as_str()))).collect();
        let country = countries.iter().position(|(c, _)| c.zip(s.country.as_deref()).is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))).unwrap_or(0);
        let lists = |m: Menu| match m {
            Menu::Lang => (&langs, lang),
            Menu::Country => (&countries, country),
        };
        let sections = self.sections(p, langs[lang].1, countries[country].1, links);

        let mut fire: Option<Act> = None;
        let mut pick: Option<(Menu, usize)> = None;
        // List open: it takes the keyboard; Escape closes it without closing the view.
        if let Some((m, c)) = self.menu.as_mut() {
            if down {
                *c = (*c + 1).min(lists(*m).0.len().saturating_sub(1));
            }
            if up {
                *c = c.saturating_sub(1);
            }
            self.follow |= down || up;
            if go {
                pick = Some((*m, *c));
            }
            if esc {
                self.menu = None;
            }
        } else {
            close |= esc;
            let total: usize = sections.iter().map(|(_, v)| v.len()).sum();
            if down || up {
                let c = self.cursor.map_or(0, |c| if down { (c + 1).min(total.saturating_sub(1)) } else { c.saturating_sub(1) });
                self.cursor = Some(c);
            }
            if go && let Some(c) = self.cursor {
                fire = sections.iter().flat_map(|(_, v)| v).nth(c).map(|i| i.act.clone());
            }
        }
        let moved = self.menu.is_none() && (down || up);
        let anchor = self.rows(ui, pal, &sections, moved, &mut fire);
        if anchor.is_none() {
            self.menu = None;
        }
        if let (Some((m, c)), Some(a)) = (self.menu, anchor) {
            let (choices, current) = lists(m);
            let (clicked, inside) = list(ui, pal, a, choices, c, current, std::mem::take(&mut self.follow));
            if let Some(i) = clicked {
                pick = Some((m, i));
            } else if !inside && fire.is_none() && ui.input(|i| i.pointer.any_click()) {
                // Click elsewhere: the list closes.
                self.menu = None;
            }
        }
        if let Some((m, i)) = pick {
            self.menu = None;
            let (choices, current) = lists(m);
            if i != current {
                let mut n = s.clone();
                let value = choices[i].0.map(str::to_owned);
                match m {
                    // The automatic country follows the new language.
                    Menu::Lang => (n.lang, n.country) = (value, None),
                    Menu::Country => n.country = value.map(|c| c.to_ascii_lowercase()),
                }
                cmds.push(Cmd::Settings { settings: n });
            }
        }
        match fire {
            Some(Act::Menu(m)) => {
                self.menu = if self.menu.is_some_and(|(o, _)| o == m) { None } else { Some((m, lists(m).1)) };
                self.follow = self.menu.is_some();
            }
            Some(Act::Setup) => return (cmds, Nav::Setup),
            Some(a) => {
                self.menu = None;
                cmds.extend(self.run(a, s, p.device.as_ref()));
            }
            None => {}
        }
        (cmds, if close { Nav::Close } else { Nav::Stay })
    }

    /// Rows per section of the open tab, in display order (keyboard cursor target); `lang`: chosen
    /// language; `country`: country national numbers are read with (landlines stay out of SMS).
    fn sections<'a>(&self, p: &'a Prefs, lang: &'a str, country: &'a str, links: (bool, bool, bool)) -> Vec<Section<'a>> {
        let s = &p.settings;
        // Each section with its tab.
        let mut sections: Vec<(usize, Section)> = vec![];
        // Chosen language on the right, before the chevron.
        let open = |m: Menu| self.menu.is_some_and(|(o, _)| o == m);
        let lang_row = w::Row { hint: lang, ..row(Some(Icon::Translate), t!("Language"), "", open(Menu::Lang)) };
        let country_row = w::Row { sub: Some(t!("Landlines stay out of SMS")), hint: country, ..row(Some(Icon::Earth), t!("Country of phone numbers"), "", open(Menu::Country)) };
        let language = vec![
            Item { row: lang_row, ctl: Ctl::None, act: Act::Menu(Menu::Lang) },
            Item { row: country_row, ctl: Ctl::None, act: Act::Menu(Menu::Country) },
        ];
        sections.push((0, (t!("Language"), language)));
        // Click on the row or Enter: next size; click on a segment: that size.
        let size_row = Item { row: row(Some(Icon::Font), t!("Text size"), "", false), ctl: Ctl::Size(s.text_size), act: Act::Size(s.text_size.next()) };
        let previews = Item { row: w::Row { sub: Some(t!("Contacts the site of the link")), ..row(Some(Icon::Link), t!("Link previews"), "", false) }, ctl: Ctl::Switch(s.link_previews), act: Act::LinkPreviews };
        sections.push((0, (t!("Display"), vec![size_row, previews])));
        let notify = vec![
            Item { row: row(Some(Icon::Message), t!("New SMS"), "", false), ctl: Ctl::Switch(s.notify_sms), act: Act::Sms },
            Item { row: row(Some(Icon::Phone), t!("Incoming calls"), "", false), ctl: Ctl::Switch(s.notify_calls), act: Act::Calls },
            Item { row: row(Some(Icon::EyeOff), t!("Hide message text"), "", false), ctl: Ctl::Switch(s.hide_preview), act: Act::Preview },
        ];
        sections.push((1, (t!("Notifications"), notify)));
        let mut devs = vec![];
        for d in &p.devices {
            let current = p.device.as_ref() == Some(&d.addr);
            let hint = match (current, &self.armed) {
                (false, Some((Armed::Device(a), _))) if *a == d.addr => t!("Confirm: daemon restarts"),
                (true, _) => match profiles(links) {
                    "" if d.connected => t!("connected, no profile"),
                    "" => t!("disconnected"),
                    up => up,
                },
                (false, _) if d.connected => t!("connected"),
                _ => "",
            };
            devs.push(Item { row: row(Some(Icon::PhoneBluetooth), &d.name, hint, current), ctl: Ctl::None, act: Act::Device(d.addr.clone()) });
        }
        if let Some(dev) = &p.device
            && !p.devices.iter().any(|d| d.addr == *dev)
        {
            devs.insert(0, Item { row: row(Some(Icon::PhoneBluetooth), dev.as_str(), t!("not paired"), true), ctl: Ctl::None, act: Act::Device(dev.clone()) });
        }
        devs.push(Item { row: row(Some(Icon::Refresh), t!("Reconnect"), "", false), ctl: Ctl::None, act: Act::Reconnect });
        devs.push(Item { row: row(Some(Icon::BluetoothConnect), t!("Connect automatically"), "", false), ctl: Ctl::Switch(s.auto_connect), act: Act::AutoConnect });
        devs.push(Item { row: row(Some(Icon::Bluetooth), t!("Set up a device"), "", false), ctl: Ctl::None, act: Act::Setup });
        sections.push((2, (t!("Bluetooth device"), devs)));
        let mut hist = vec![Item { row: row(Some(Icon::History), t!("Keep everything"), "", s.retention_days.is_none()), ctl: Ctl::None, act: Act::Retention(None) }];
        for d in RETENTIONS {
            let label = match d {
                30 => t!("Keep 30 days"),
                90 => t!("Keep 90 days"),
                _ => t!("Keep 1 year"),
            };
            hist.push(Item { row: row(Some(Icon::History), label, "", s.retention_days == Some(d)), ctl: Ctl::None, act: Act::Retention(Some(d)) });
        }
        hist.push(Item {
            row: w::Row { sub: Some(t!("Erase their text from this PC")), ..row(Some(Icon::Broom), t!("Purge deleted messages"), "", false) },
            ctl: Ctl::None,
            act: Act::Purge,
        });
        let clear_hint = if matches!(self.armed, Some((Armed::Clear, _))) { t!("Press again to confirm") } else { t!("Messages and calls on this PC") };
        hist.push(Item { row: w::Row { sub: Some(clear_hint), ..row(Some(Icon::Delete), t!("Clear history"), "", false) }, ctl: Ctl::None, act: Act::Clear });
        sections.push((3, (t!("History"), hist)));
        let editing = |i: Option<usize>| self.edit.as_ref().is_some_and(|(a, ..)| *a == i);
        let mut replies: Vec<Item> = s
            .quick_replies
            .iter()
            .enumerate()
            .map(|(i, r)| Item { row: row(Some(Icon::MessageText), r, if i == 0 { t!("default") } else { "" }, editing(Some(i))), ctl: Ctl::None, act: Act::Reply(Some(i)) })
            .collect();
        if replies.len() < QUICK_REPLIES_MAX {
            let add = w::Row { sub: Some(t!("The default one is sent by “Decline + SMS”")), ..row(Some(Icon::Plus), t!("Add a reply"), "", editing(None)) };
            replies.push(Item { row: add, ctl: Ctl::None, act: Act::Reply(None) });
        }
        sections.push((REPLIES, (t!("Quick replies"), replies)));
        sections.into_iter().filter(|(t, _)| *t == self.tab).map(|(_, s)| s).collect()
    }

    /// Sections and rows; a click fills `fire`. Returns the row of the open list if it is visible
    /// (anchor point of the list).
    fn rows(&mut self, ui: &mut egui::Ui, pal: &Palette, sections: &[Section], moved: bool, fire: &mut Option<Act>) -> Option<egui::Rect> {
        let mut anchor: Option<egui::Rect> = None;
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = w::ROW_GAP;
            let mut idx = 0;
            for (title, items) in sections {
                ui.add_space(w::GAP * 2.0);
                ui.label(w::text(*title, font::SMALL, pal.muted()));
                ui.add_space(w::GAP);
                for it in items {
                    let here = self.cursor == Some(idx) && self.menu.is_none();
                    let mut r = w::Row { selected: it.row.selected || here, ..it.row };
                    let menu = if let Act::Menu(m) = it.act { Some(m) } else { None };
                    if menu.is_some() {
                        // Chevron position: the chosen language shifts to the left.
                        r.hint = "";
                    }
                    let resp = w::row(ui, pal, &r);
                    match it.ctl {
                        Ctl::None => {}
                        Ctl::Switch(on) => w::switch(ui, pal, &resp, on),
                        Ctl::Size(cur) => {
                            let labels = TextSize::ALL.map(TextSize::label);
                            let on = TextSize::ALL.iter().position(|z| *z == cur).unwrap_or(0);
                            if let Some(i) = w::segments(ui, pal, &labels, on) {
                                self.cursor = None;
                                *fire = Some(Act::Size(TextSize::ALL[i]));
                            }
                        }
                    }
                    if let Some(m) = menu {
                        let y = resp.rect.center().y;
                        let open = self.menu.is_some_and(|(o, _)| o == m);
                        let chevron = if open { Icon::ChevronUp } else { Icon::ChevronDown };
                        w::paint_icon(ui, chevron, egui::pos2(resp.rect.right() - size::EDGE - 7.0, y), 14.0, pal.dim(0.5));
                        ui.painter().text(
                            egui::pos2(resp.rect.right() - 36.0, y),
                            egui::Align2::RIGHT_CENTER,
                            it.row.hint,
                            w::regular(font::BODY),
                            pal.dim(0.7),
                        );
                        // Row scrolled out of the visible area: the list has no anchor point anymore.
                        if open {
                            anchor = ui.clip_rect().intersects(resp.rect).then_some(resp.rect);
                        }
                    }
                    if here && moved {
                        resp.scroll_to_me(None);
                    }
                    if resp.clicked() {
                        self.cursor = None;
                        *fire = Some(it.act.clone());
                    }
                    idx += 1;
                }
            }
            ui.add_space(w::GAP * 2.0);
        });
        anchor
    }

    fn run(&mut self, act: Act, s: &Settings, device: Option<&BtAddr>) -> Vec<Cmd> {
        let with = |f: &dyn Fn(&mut Settings)| {
            let mut n = s.clone();
            f(&mut n);
            vec![Cmd::Settings { settings: n }]
        };
        match act {
            Act::Menu(_) | Act::Setup => vec![],
            Act::Size(z) if z == s.text_size => vec![],
            Act::Size(z) => with(&|n| n.text_size = z),
            Act::Sms => with(&|n| n.notify_sms = !n.notify_sms),
            Act::Calls => with(&|n| n.notify_calls = !n.notify_calls),
            Act::Preview => with(&|n| n.hide_preview = !n.hide_preview),
            Act::LinkPreviews => with(&|n| n.link_previews = !n.link_previews),
            Act::Retention(d) => with(&|n| n.retention_days = d),
            Act::Reconnect => vec![Cmd::Reconnect],
            Act::AutoConnect => with(&|n| n.auto_connect = !n.auto_connect),
            Act::Purge => vec![Cmd::PurgeHidden],
            Act::Device(a) if device == Some(&a) => vec![],
            Act::Device(a) => self.confirm(Armed::Device(a.clone()), Cmd::SetDevice { addr: a }),
            Act::Clear => self.confirm(Armed::Clear, Cmd::ClearHistory),
            Act::Reply(i) => {
                let text = i.and_then(|i| s.quick_replies.get(i)).cloned().unwrap_or_default();
                (self.edit, self.focus_edit) = (Some((i, text.clone(), text)), true);
                vec![]
            }
        }
    }

    /// Field and buttons of the quick reply being edited; the new list goes out as a command.
    fn editor(&mut self, ui: &mut egui::Ui, pal: &Palette, s: &Settings) -> Vec<Cmd> {
        let Some((at, text, orig)) = self.edit.as_mut() else { return vec![] };
        // The list may have changed since (other window, daemon reply): the edited text is looked up again,
        // and becomes a new one if it is gone.
        let at = at.and_then(|i| if s.quick_replies.get(i) == Some(orig) { Some(i) } else { s.quick_replies.iter().position(|r| r == orig) });
        let r = ui.add(w::field(text, t!("Quick reply")).char_limit(QUICK_REPLY_LEN).desired_width(f32::INFINITY));
        if std::mem::take(&mut self.focus_edit) {
            r.request_focus();
        }
        let (enter, esc) = if r.lost_focus() { ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape))) } else { (false, false) };
        ui.add_space(w::GAP);
        let mut act = enter.then_some(Edit::Save).or(esc.then_some(Edit::Cancel));
        ui.horizontal_wrapped(|ui| {
            if w::button(ui, pal, t!("Save"), w::Tone::Accent).clicked() {
                act = Some(Edit::Save);
            }
            if let Some(i) = at {
                if i > 0 && w::button(ui, pal, t!("Make default"), w::Tone::Neutral).clicked() {
                    act = Some(Edit::First);
                }
                if w::button(ui, pal, t!("Delete"), w::Tone::Danger).clicked() {
                    act = Some(Edit::Delete);
                }
            }
            if w::button(ui, pal, t!("Cancel"), w::Tone::Neutral).clicked() {
                act = Some(Edit::Cancel);
            }
        });
        ui.add_space(w::GAP);
        let Some(act) = act else { return vec![] };
        let mut n = s.clone();
        let list = &mut n.quick_replies;
        match (act, at) {
            (Edit::Save, _) => {
                let clean = clean_reply(text);
                if clean.is_empty() {
                    // Nothing to keep: the field stays open.
                    self.focus_edit = true;
                    return vec![];
                }
                match at {
                    Some(i) => list[i] = clean,
                    None if list.len() < QUICK_REPLIES_MAX => list.push(clean),
                    None => {}
                }
            }
            (Edit::First, Some(i)) => {
                let r = list.remove(i);
                list.insert(0, r);
            }
            (Edit::Delete, Some(i)) => drop(list.remove(i)),
            _ => {}
        }
        self.edit = None;
        if n.quick_replies == s.quick_replies { vec![] } else { vec![Cmd::Settings { settings: n }] }
    }

    /// First trigger: armed; second within the delay (and not too early): command.
    fn confirm(&mut self, what: Armed, cmd: Cmd) -> Vec<Cmd> {
        match &self.armed {
            Some((a, t)) if *a == what && t.elapsed() < ARM_MIN => vec![],
            Some((a, _)) if *a == what => {
                self.armed = None;
                vec![cmd]
            }
            _ => {
                self.armed = Some((what, Instant::now()));
                vec![]
            }
        }
    }
}

/// Drop-down list under `anchor` (bar menu: surface, `line` border), in the foreground, as high as
/// the window allows below it then scrolling; `follow`: brings the `cursor` row into view.
/// Returns the clicked row and whether the pointer is over it.
fn list(ui: &egui::Ui, pal: &Palette, anchor: egui::Rect, choices: &[(Option<&str>, &str)], cursor: usize, current: usize, follow: bool) -> (Option<usize>, bool) {
    let top = anchor.left_bottom() + egui::vec2(0.0, 4.0);
    let max_h = (ui.ctx().content_rect().bottom() - top.y - 16.0).max(120.0);
    let area = egui::Area::new(ui.id().with("menu_list")).order(egui::Order::Foreground).fixed_pos(top);
    let r = area.show(ui.ctx(), |ui| {
        egui::Frame::new()
            .fill(pal.surface(0.06))
            .stroke(egui::Stroke::new(1.0, pal.line()))
            .corner_radius(CORNER)
            .inner_margin(egui::Margin::same(4))
            .show(ui, |ui| {
                ui.set_width(anchor.width() - 8.0);
                ScrollArea::vertical()
                    .max_height(max_h)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = w::ROW_GAP;
                        let mut hit = None;
                        for (i, (code, name)) in choices.iter().enumerate() {
                            let icon = (i == current).then_some(Icon::Check);
                            let r = w::row(ui, pal, &w::Row { selected: i == cursor, ..row(icon, name, code.unwrap_or(""), i == current) });
                            if follow && i == cursor {
                                r.scroll_to_me(None);
                            }
                            if r.clicked() {
                                hit = Some(i);
                            }
                        }
                        hit
                    })
                    .inner
            })
            .inner
    });
    (r.inner, r.response.contains_pointer())
}

/// Active profiles (MAP, HFP, PBAP), acronyms not translated.
fn profiles((map, hfp, pbap): (bool, bool, bool)) -> &'static str {
    match (map, hfp, pbap) {
        (true, true, true) => "MAP · HFP · PBAP",
        (true, true, false) => "MAP · HFP",
        (true, false, true) => "MAP · PBAP",
        (false, true, true) => "HFP · PBAP",
        (true, false, false) => "MAP",
        (false, true, false) => "HFP",
        (false, false, true) => "PBAP",
        (false, false, false) => "",
    }
}

fn row<'a>(icon: Option<Icon>, label: &'a str, hint: &'a str, selected: bool) -> w::Row<'a> {
    w::Row { icon, label, sub: None, hint, badge: 0, selected, strong: selected }
}
