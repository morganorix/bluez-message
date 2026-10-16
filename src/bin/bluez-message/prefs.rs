//! Vue Paramètres (panneau de droite) : langue, taille du texte, notifications, appareil Bluetooth, historique.
//! Les réglages appartiennent au démon : chaque changement part en commande, l'affichage suit sa réponse.

use std::time::{Duration, Instant};

use bluez_message::addr::BtAddr;
use bluez_message::app::state::Prefs;
use bluez_message::engine::Cmd;
use bluez_message::gui::theme::{CORNER, Palette, font, size};
use bluez_message::gui::widgets as w;
use bluez_message::i18n;
use bluez_message::settings::{RETENTIONS, Settings, TextSize};
use bluez_message::t;
use eframe::egui::{self, Key, ScrollArea};

/// Délai pendant lequel une action à confirmer attend son second déclenchement.
const ARM: Duration = Duration::from_secs(5);
/// Second déclenchement trop rapproché du premier (double clic involontaire) : ignoré.
const ARM_MIN: Duration = Duration::from_millis(300);

/// Action à confirmer (second clic ou seconde Entrée).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Armed {
    Device(BtAddr),
    Clear,
}

#[derive(Default)]
pub struct View {
    /// Ligne choisie au clavier ; `None` : souris.
    cursor: Option<usize>,
    armed: Option<(Armed, Instant)>,
    /// Liste des langues ouverte, avec sa ligne choisie au clavier.
    lang_menu: Option<usize>,
}

/// Ligne de la vue : son rendu et ce qu'elle déclenche.
struct Item<'a> {
    row: w::Row<'a>,
    ctl: Ctl,
    act: Act,
}

/// Commande affichée à droite de la ligne.
enum Ctl {
    None,
    /// Interrupteur et son état.
    Switch(bool),
    /// Choix de la taille du texte et la taille en vigueur.
    Size(TextSize),
}

/// Rubrique : titre et lignes.
type Section<'a> = (&'a str, Vec<Item<'a>>);

#[derive(Clone)]
enum Act {
    LangMenu,
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

/// Touche enfoncée à cette image, répétition automatique exclue : maintenir Entrée ne confirme rien.
fn pressed(ui: &egui::Ui, key: Key) -> bool {
    ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { key: k, pressed: true, repeat: false, .. } if *k == key)))
}

impl View {
    /// Rendu ; renvoie les commandes à transmettre au démon et `true` si la vue doit se fermer.
    pub fn show(&mut self, ui: &mut egui::Ui, pal: &Palette, prefs: Option<&Prefs>, links: (bool, bool, bool)) -> (Vec<Cmd>, bool) {
        if self.armed.as_ref().is_some_and(|(_, t)| t.elapsed() > ARM) {
            self.armed = None;
        }
        if self.armed.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        // Clavier seulement si aucun champ n'a le focus.
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

        // Choix de langue : système, puis chaque langue disponible sous son nom.
        let choices: Vec<(Option<&str>, &str)> =
            std::iter::once((None, t!("System language"))).chain(p.langs.iter().map(|c| (Some(c.as_str()), i18n::native_name(c)))).collect();
        let current = choices.iter().position(|(c, _)| *c == s.lang.as_deref()).unwrap_or(0);
        let sections = self.sections(p, choices[current].1, links);

        let mut fire: Option<Act> = None;
        let mut pick_lang: Option<usize> = None;
        // Liste des langues ouverte : elle prend le clavier ; Échap la referme sans fermer la vue.
        if let Some(c) = self.lang_menu.as_mut() {
            if down {
                *c = (*c + 1).min(choices.len() - 1);
            }
            if up {
                *c = c.saturating_sub(1);
            }
            if go {
                pick_lang = Some(*c);
            }
            if esc {
                self.lang_menu = None;
            }
        } else {
            close |= esc;
            let total: usize = sections.iter().map(|(_, v)| v.len()).sum();
            if down || up {
                let c = self.cursor.map_or(0, |c| if down { (c + 1).min(total - 1) } else { c.saturating_sub(1) });
                self.cursor = Some(c);
            }
            if go && let Some(c) = self.cursor {
                fire = sections.iter().flat_map(|(_, v)| v).nth(c).map(|i| i.act.clone());
            }
        }
        let moved = self.lang_menu.is_none() && (down || up);
        let anchor = self.rows(ui, pal, &sections, moved, &mut fire);
        if anchor.is_none() {
            self.lang_menu = None;
        }
        if let (Some(c), Some(a)) = (self.lang_menu, anchor) {
            let (clicked, inside) = lang_list(ui, pal, a, &choices, c, current);
            if clicked.is_some() {
                pick_lang = clicked;
            } else if !inside && fire.is_none() && ui.input(|i| i.pointer.any_click()) {
                // Clic ailleurs : la liste se referme.
                self.lang_menu = None;
            }
        }
        let mut cmds = vec![];
        if let Some(i) = pick_lang {
            self.lang_menu = None;
            if i != current {
                let mut n = s.clone();
                n.lang = choices[i].0.map(str::to_owned);
                cmds.push(Cmd::Settings { settings: n });
            }
        }
        match fire {
            Some(Act::LangMenu) => self.lang_menu = if self.lang_menu.is_some() { None } else { Some(current) },
            Some(a) => {
                self.lang_menu = None;
                cmds.extend(self.run(a, s, &p.device));
            }
            None => {}
        }
        (cmds, close)
    }

    /// Lignes par rubrique, dans l'ordre d'affichage (cible du curseur clavier) ; `lang` : langue choisie.
    fn sections<'a>(&self, p: &'a Prefs, lang: &'a str, links: (bool, bool, bool)) -> Vec<Section<'a>> {
        let s = &p.settings;
        let mut sections: Vec<Section> = vec![];
    // Langue choisie à droite, avant le chevron.
    let lang_row = w::Row { hint: lang, ..row("󰗊", t!("Language"), "", self.lang_menu.is_some()) };
    sections.push((t!("Language"), vec![Item { row: lang_row, ctl: Ctl::None, act: Act::LangMenu }]));
    // Clic sur la ligne ou Entrée : taille suivante ; clic sur un segment : cette taille.
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

    /// Rubriques et lignes ; un clic renseigne `fire`. Rend la ligne Langue si elle est visible (point
    /// d'attache de la liste des langues).
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
                let here = self.cursor == Some(idx) && self.lang_menu.is_none();
                let mut r = w::Row { selected: it.row.selected || here, ..it.row };
                let lang = matches!(it.act, Act::LangMenu);
                if lang {
                    // Place du chevron : la langue choisie se décale vers la gauche.
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
                if lang {
                    let y = resp.rect.center().y;
                    let chevron = if self.lang_menu.is_some() { "󰅃" } else { "󰅀" };
                    ui.painter().text(egui::pos2(resp.rect.right() - size::EDGE, y), egui::Align2::RIGHT_CENTER, chevron, w::regular(14.0), pal.dim(0.5));
                    ui.painter().text(egui::pos2(resp.rect.right() - 36.0, y), egui::Align2::RIGHT_CENTER, it.row.hint, w::regular(font::BODY), pal.dim(0.7));
                    // Ligne sortie de la zone visible : la liste n'a plus de point d'attache.
                    anchor = ui.clip_rect().intersects(resp.rect).then_some(resp.rect);
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
            Act::LangMenu => vec![],
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

    /// Premier déclenchement : armé ; second dans le délai (et pas trop tôt) : commande.
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

/// Liste déroulante des langues sous `anchor` (menu de la barre : surface, bordure `line`), au premier plan.
/// Renvoie la ligne cliquée et si le pointeur est dessus.
fn lang_list(ui: &egui::Ui, pal: &Palette, anchor: egui::Rect, choices: &[(Option<&str>, &str)], cursor: usize, current: usize) -> (Option<usize>, bool) {
    let area = egui::Area::new(ui.id().with("lang_list")).order(egui::Order::Foreground).fixed_pos(anchor.left_bottom() + egui::vec2(0.0, 4.0));
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

/// Profils actifs (MAP, HFP, PBAP), sigles non traduits.
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
