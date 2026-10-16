//! Settings view (right panel): language, text size, notifications, Bluetooth device, history.
//! The settings belong to the daemon: each change goes out as a command, the display follows its reply.

use std::time::{Duration, Instant};

use bluez_message::addr::BtAddr;
use bluez_message::app::state::Prefs;
use bluez_message::engine::Cmd;
use bluez_message::gui::theme::{CORNER, Palette, font, size};
use bluez_message::gui::widgets as w;
use bluez_message::i18n;
use bluez_message::numbering;
use bluez_message::settings::{RETENTIONS, Settings, TextSize};
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

#[derive(Default)]
pub struct View {
    /// Row chosen with the keyboard; `None`: mouse.
    cursor: Option<usize>,
    armed: Option<(Armed, Instant)>,
    /// Drop-down list open, with its row chosen with the keyboard.
    menu: Option<(Menu, usize)>,
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

#[derive(Clone)]
enum Act {
    Menu(Menu),
    Size(TextSize),
    Sms,
    Calls,
    Preview,
    Device(BtAddr),
    Reconnect,
    AutoConnect,
    Retention(Option<u32>),
    Purge,
    Clear,
}

/// Key pressed at this frame, auto-repeat excluded: holding Enter confirms nothing.
fn pressed(ui: &egui::Ui, key: Key) -> bool {
    ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { key: k, pressed: true, repeat: false, .. } if *k == key)))
}

impl View {
    /// Rendering; returns the commands to send to the daemon and `true` if the view must close.
    pub fn show(&mut self, ui: &mut egui::Ui, pal: &Palette, prefs: Option<&Prefs>, links: (bool, bool, bool)) -> (Vec<Cmd>, bool) {
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
        let rect = w::header_with(ui, pal, "󰒓", t!("SETTINGS"), 60.0);
        let mut close = w::header_button(ui, pal, rect, "󰅖").on_hover_text(t!("Close (Esc)")).clicked();
        w::rule(ui, pal);
        ui.add_space(12.0);
        let Some(p) = prefs else {
            w::hint(ui, pal, t!("Settings unavailable: daemon not connected"));
            return (vec![], close || esc);
        };
        let s = &p.settings;

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
            let (clicked, inside) = list(ui, pal, a, choices, c, current);
            if let Some(i) = clicked {
                pick = Some((m, i));
            } else if !inside && fire.is_none() && ui.input(|i| i.pointer.any_click()) {
                // Click elsewhere: the list closes.
                self.menu = None;
            }
        }
        let mut cmds = vec![];
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
            Some(Act::Menu(m)) => self.menu = if self.menu.is_some_and(|(o, _)| o == m) { None } else { Some((m, lists(m).1)) },
            Some(a) => {
                self.menu = None;
                cmds.extend(self.run(a, s, &p.device));
            }
            None => {}
        }
        (cmds, close)
    }

    /// Rows per section, in display order (keyboard cursor target); `lang`: chosen language;
    /// `country`: country national numbers are read with (landlines stay out of SMS).
    fn sections<'a>(&self, p: &'a Prefs, lang: &'a str, country: &'a str, links: (bool, bool, bool)) -> Vec<Section<'a>> {
        let s = &p.settings;
        let mut sections: Vec<Section> = vec![];
        // Chosen language on the right, before the chevron.
        let open = |m: Menu| self.menu.is_some_and(|(o, _)| o == m);
        let lang_row = w::Row { hint: lang, ..row("󰗊", t!("Language"), "", open(Menu::Lang)) };
        let country_row = w::Row { sub: Some(t!("Landlines stay out of SMS")), hint: country, ..row("󰇧", t!("Country of phone numbers"), "", open(Menu::Country)) };
        sections.push((
            t!("Language"),
            vec![
                Item { row: lang_row, ctl: Ctl::None, act: Act::Menu(Menu::Lang) },
                Item { row: country_row, ctl: Ctl::None, act: Act::Menu(Menu::Country) },
            ],
        ));
        // Click on the row or Enter: next size; click on a segment: that size.
        let size_row = Item { row: row("󰛖", t!("Text size"), "", false), ctl: Ctl::Size(s.text_size), act: Act::Size(s.text_size.next()) };
        sections.push((t!("Display"), vec![size_row]));
        sections.push((
            t!("Notifications"),
            vec![
                Item { row: row("󰍡", t!("New SMS"), "", false), ctl: Ctl::Switch(s.notify_sms), act: Act::Sms },
                Item { row: row("󰏲", t!("Incoming calls"), "", false), ctl: Ctl::Switch(s.notify_calls), act: Act::Calls },
                Item { row: row("󰈉", t!("Hide message text"), "", false), ctl: Ctl::Switch(s.hide_preview), act: Act::Preview },
            ],
        ));
        let mut devs = vec![];
        for d in &p.devices {
            let current = d.addr == p.device;
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
            devs.push(Item { row: row("󰏳", &d.name, hint, current), ctl: Ctl::None, act: Act::Device(d.addr.clone()) });
        }
        if !p.devices.iter().any(|d| d.addr == p.device) {
            devs.insert(0, Item { row: row("󰏳", p.device.as_str(), t!("not paired"), true), ctl: Ctl::None, act: Act::Device(p.device.clone()) });
        }
        devs.push(Item { row: row("󰑐", t!("Reconnect"), "", false), ctl: Ctl::None, act: Act::Reconnect });
        devs.push(Item { row: row("󰂱", t!("Connect automatically"), "", false), ctl: Ctl::Switch(s.auto_connect), act: Act::AutoConnect });
        sections.push((t!("Bluetooth device"), devs));
        let mut hist = vec![Item { row: row("󰋚", t!("Keep everything"), "", s.retention_days.is_none()), ctl: Ctl::None, act: Act::Retention(None) }];
        for d in RETENTIONS {
            let label = match d {
                30 => t!("Keep 30 days"),
                90 => t!("Keep 90 days"),
                _ => t!("Keep 1 year"),
            };
            hist.push(Item { row: row("󰋚", label, "", s.retention_days == Some(d)), ctl: Ctl::None, act: Act::Retention(Some(d)) });
        }
        hist.push(Item {
            row: w::Row { sub: Some(t!("Erase their text from this PC")), ..row("󰃢", t!("Purge deleted messages"), "", false) },
            ctl: Ctl::None,
            act: Act::Purge,
        });
        let clear_hint = if matches!(self.armed, Some((Armed::Clear, _))) { t!("Press again to confirm") } else { t!("Messages and calls on this PC") };
        hist.push(Item { row: w::Row { sub: Some(clear_hint), ..row("󰆴", t!("Clear history"), "", false) }, ctl: Ctl::None, act: Act::Clear });
        sections.push((t!("History"), hist));
        sections
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
                        let chevron = if open { "󰅃" } else { "󰅀" };
                        ui.painter().text(egui::pos2(resp.rect.right() - size::EDGE, y), egui::Align2::RIGHT_CENTER, chevron, w::regular(14.0), pal.dim(0.5));
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

    fn run(&mut self, act: Act, s: &Settings, device: &BtAddr) -> Vec<Cmd> {
        let with = |f: &dyn Fn(&mut Settings)| {
            let mut n = s.clone();
            f(&mut n);
            vec![Cmd::Settings { settings: n }]
        };
        match act {
            Act::Menu(_) => vec![],
            Act::Size(z) if z == s.text_size => vec![],
            Act::Size(z) => with(&|n| n.text_size = z),
            Act::Sms => with(&|n| n.notify_sms = !n.notify_sms),
            Act::Calls => with(&|n| n.notify_calls = !n.notify_calls),
            Act::Preview => with(&|n| n.hide_preview = !n.hide_preview),
            Act::Retention(d) => with(&|n| n.retention_days = d),
            Act::Reconnect => vec![Cmd::Reconnect],
            Act::AutoConnect => with(&|n| n.auto_connect = !n.auto_connect),
            Act::Purge => vec![Cmd::PurgeHidden],
            Act::Device(a) if a == *device => vec![],
            Act::Device(a) => self.confirm(Armed::Device(a.clone()), Cmd::SetDevice { addr: a }),
            Act::Clear => self.confirm(Armed::Clear, Cmd::ClearHistory),
        }
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

/// Drop-down list under `anchor` (bar menu: surface, `line` border), in the foreground.
/// Returns the clicked row and whether the pointer is over it.
fn list(ui: &egui::Ui, pal: &Palette, anchor: egui::Rect, choices: &[(Option<&str>, &str)], cursor: usize, current: usize) -> (Option<usize>, bool) {
    let area = egui::Area::new(ui.id().with("menu_list")).order(egui::Order::Foreground).fixed_pos(anchor.left_bottom() + egui::vec2(0.0, 4.0));
    let r = area.show(ui.ctx(), |ui| {
        egui::Frame::new()
            .fill(pal.surface(0.06))
            .stroke(egui::Stroke::new(1.0, pal.line()))
            .corner_radius(CORNER)
            .inner_margin(egui::Margin::same(4))
            .show(ui, |ui| {
                ui.set_width(anchor.width() - 8.0);
                ui.spacing_mut().item_spacing.y = w::ROW_GAP;
                let mut hit = None;
                for (i, (code, name)) in choices.iter().enumerate() {
                    let icon = if i == current { "󰄬" } else { "" };
                    let r = w::row(ui, pal, &w::Row { selected: i == cursor, ..row(icon, name, code.unwrap_or(""), i == current) });
                    if r.clicked() {
                        hit = Some(i);
                    }
                }
                hit
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

fn row<'a>(icon: &'a str, label: &'a str, hint: &'a str, selected: bool) -> w::Row<'a> {
    w::Row { icon, label, sub: None, hint, badge: 0, selected, strong: selected }
}
