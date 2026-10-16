//! Fenêtre : en-tête et état des liens, bandeaux d'appel et d'erreur, conversations à gauche, fil et saisie à droite.

use std::sync::mpsc::Receiver;

use bluez_message::app::state::{Phase, State, number_key};
use bluez_message::app::tapback;
use bluez_message::engine::{Cmd, dial_number, validate_send};
use bluez_message::gui::emoji::Emojis;
use bluez_message::gui::theme::{self, Palette, size};
use bluez_message::gui::widgets::{self as w, right_of};
use bluez_message::ipc::Link;
use bluez_message::settings::TextSize;
use bluez_message::t;
use eframe::egui::{self, CentralPanel, Frame, Key, Margin, Panel};
use tokio::sync::mpsc::UnboundedSender;

use crate::composer::{Composer, ComposerAction};
use crate::convs::{ConvList, ListAction, Target};
use crate::thread::{self, ThreadAction};

/// Langue appliquée aux textes de la GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lang {
    /// Aucun réglage reçu du démon : langue du système.
    Unset,
    /// Réglage du démon (`None` : langue du système).
    Set(Option<String>),
}

impl Lang {
    fn is(&self, code: Option<&str>) -> bool {
        matches!(self, Lang::Set(l) if l.as_deref() == code)
    }
}

pub struct App {
    /// `colors.plm` régénéré par matugen : couleurs rechargées à chaud.
    colors: theme::Watch,
    state: State,
    pal: Palette,
    /// Commandes vers le démon ; `None` après la fermeture.
    tx: Option<UnboundedSender<Cmd>>,
    events: Receiver<Link>,
    target: Target,
    /// Colonne de gauche : recherche et liste.
    list: ConvList,
    /// Saisie du message du fil ouvert.
    composer: Composer,
    /// Fenêtre au premier plan à l'image précédente.
    was_focused: bool,
    /// Vue Paramètres ouverte à la place du fil.
    prefs: Option<crate::prefs::View>,
    /// Langue appliquée à la GUI ; survit aux reconnexions.
    lang: Lang,
    /// Emojis en couleur (police Noto Color Emoji).
    emojis: Emojis,
    /// Taille du texte appliquée (zoom) ; `None` : aucun réglage reçu.
    text_size: Option<TextSize>,
}

impl App {
    pub fn new(pal: Palette, tx: UnboundedSender<Cmd>, events: Receiver<Link>) -> Self {
        Self { colors: theme::Watch::new(), state: State::default(), pal, tx: Some(tx), events, target: Target::None, list: ConvList::default(), composer: Composer::default(), was_focused: false, prefs: None, lang: Lang::Unset, emojis: Emojis::default(), text_size: None }
    }

    /// Validation locale (retour immédiat), puis envoi au démon qui revalide.
    fn submit(&mut self, cmd: Cmd) -> bool {
        if let Cmd::Send { to, text } = &cmd
            && let Err(e) = validate_send(to, text)
        {
            self.state.error = Some(e);
            return false;
        }
        match &self.tx {
            Some(tx) if tx.send(cmd).is_ok() => true,
            _ => {
                self.state.error = Some(t!("connection to the daemon closed").into());
                false
            }
        }
    }

    fn on_link(&mut self, l: Link) {
        match l {
            // L'instantané du démon suit : on repart d'un état vide.
            Link::Up => self.state = State::default(),
            Link::Down(msg) => {
                // Conversations gardées à l'écran ; liens et appel remis à zéro.
                let mut st = State::default();
                st.convs = std::mem::take(&mut self.state.convs);
                // Plus de résultat à attendre : les envois en cours passent en échec (renvoi possible).
                st.expire_pending(true);
                st.error = Some(msg);
                self.state = st;
            }
            // Les notifications sont l'affaire du démon : effets ignorés.
            Link::Event(o) => {
                drop(self.state.apply(*o));
                // Langue changée par le démon : textes de la GUI rechargés aussitôt.
                if let Some(p) = &self.state.settings
                    && !self.lang.is(p.settings.lang.as_deref())
                {
                    bluez_message::i18n::set_lang(p.settings.lang.as_deref());
                    self.lang = Lang::Set(p.settings.lang.clone());
                }
            }
        }
    }

    fn send_draft(&mut self) {
        let to = match &self.target {
            Target::Conv(k) => match self.state.conv(k) {
                Some(c) => c.number.clone(),
                None => return,
            },
            // Numéro du répertoire (« +33 6 12… ») : séparateurs retirés pour l'envoi.
            Target::Contact { number, .. } => number.chars().filter(|c| !" -.()".contains(*c)).collect(),
            Target::None => return,
        };
        let text = self.composer.draft.trim().to_owned();
        if text.is_empty() {
            return;
        }
        if self.send(&to, &text) {
            self.composer.draft.clear();
            self.state.error = None;
        }
    }

    /// Envoi : la bulle paraît aussitôt, en attente du résultat du transfert.
    fn send(&mut self, to: &str, text: &str) -> bool {
        if !self.submit(Cmd::Send { to: to.to_owned(), text: text.to_owned() }) {
            return false;
        }
        self.state.pending(to, text);
        true
    }

    /// En-tête (`MenuHeader`) ; à droite, les points d'état des liens.
    fn header(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal;
        let hfp = match (self.state.hfp_up, self.state.slc) {
            (true, true) => w::Link::On,
            (true, false) => w::Link::Pending,
            _ => w::Link::Off,
        };
        let map = if self.state.map_up { w::Link::On } else { w::Link::Off };
        let rect = w::header(ui, &pal, "󰍡", "MESSAGES");
        right_of(ui, rect.shrink2(egui::vec2(size::EDGE, 0.0)), |ui| {
            ui.spacing_mut().item_spacing.x = 16.0;
            w::status_dot(ui, &pal, "HFP", hfp);
            w::status_dot(ui, &pal, "MAP", map);
        });
    }

    /// Colonne de gauche : conversations, et Paramètres en pied (filet puis ligne).
    fn side(&mut self, ui: &mut egui::Ui) {
        let full = ui.max_rect();
        let foot_h = 1.0 + w::GAP * 2.0 + size::ROW_H;
        let (top, foot) = full.split_top_bottom_at_y(full.bottom() - foot_h);
        let pal = self.pal;
        let keys = self.prefs.is_none();
        let acts = ui.scope_builder(egui::UiBuilder::new().max_rect(top), |ui| self.list.show(ui, &pal, &mut self.emojis, &self.state, &self.target, keys)).inner;
        for a in acts {
            match a {
                ListAction::Delete(key) => self.delete_conv(key),
                ListAction::Open(t) => self.open(t),
            }
        }
        let open = self.prefs.is_some();
        let clicked = ui
            .scope_builder(egui::UiBuilder::new().max_rect(foot), |ui| {
                w::rule(ui, &pal);
                ui.add_space(w::GAP * 2.0);
                let r = w::Row { icon: "󰒓", label: t!("Settings"), sub: None, hint: "Ctrl+,", badge: 0, selected: open, strong: open };
                w::row(ui, &pal, &r).clicked()
            })
            .inner;
        if clicked {
            self.toggle_prefs(ui.ctx());
        }
    }

    /// Ouvre ou ferme les Paramètres ; ouverts, aucun champ ne garde le focus (flèches pour la vue).
    fn toggle_prefs(&mut self, ctx: &egui::Context) {
        if self.prefs.take().is_none() {
            self.prefs = Some(crate::prefs::View::default());
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        } else {
            self.focus_target();
        }
    }

    /// Curseur dans la recherche, ou dans le message si un fil est ouvert.
    fn focus_target(&mut self) {
        let list = self.target == Target::None;
        (self.list.focus, self.composer.focus) = (list, !list);
    }

    /// Information du démon (reconnexion, purge) : texte en `fg`, comme le bandeau d'erreur.
    fn info_banner(&mut self, ui: &mut egui::Ui) {
        if let Some(m) = &self.state.info
            && w::banner(ui, &self.pal, "󰋽", m, self.pal.fg)
        {
            self.state.info = None;
        }
    }

    /// Erreur (démon arrêté, liaison, envoi refusé) : texte en danger, comme `wifi_error`, et sa croix.
    fn error_banner(&mut self, ui: &mut egui::Ui) {
        if let Some(e) = &self.state.error
            && w::banner(ui, &self.pal, "󰀦", e, self.pal.danger)
        {
            self.state.error = None;
        }
    }

    /// Bandeau d'appel : carte d'appel, boutons 120 × 26 à 10 du bord.
    fn call_banner(&mut self, ui: &mut egui::Ui) {
        let c = &self.state.call;
        let what = match c.phase {
            Phase::Incoming => t!("Incoming call"),
            Phase::Outgoing => t!("Outgoing call"),
            Phase::Active => t!("In call"),
            Phase::Idle => return,
        };
        let who = match (&c.name, c.number.as_deref()) {
            (Some(n), Some(num)) => format!("{n}  ·  {num}"),
            (None, Some(num)) if !num.is_empty() => num.to_owned(),
            _ => t!("hidden number").to_owned(),
        };
        let incoming = c.phase == Phase::Incoming;
        let pal = self.pal;
        let rect = w::call_card(ui, &pal, "󰏲", pal.accent, &who, what, pal.muted());
        let (mut answer, mut hangup) = (false, false);
        right_of(ui, rect.shrink2(egui::vec2(10.0, 0.0)), |ui| {
            hangup = w::button(ui, &pal, t!("HANG UP"), w::Tone::Danger).clicked();
            if incoming {
                answer = w::button(ui, &pal, t!("ANSWER"), w::Tone::Accent).clicked();
            }
        });
        if hangup {
            self.submit(Cmd::Hangup);
        }
        if answer {
            self.submit(Cmd::Answer);
        }
    }

    /// Supprime une conversation (localement puis côté démon) ; ouverte, le fil se vide.
    fn delete_conv(&mut self, key: String) {
        if matches!(&self.target, Target::Conv(k) if *k == key) {
            self.target = Target::None;
        }
        self.state.delete(&key, None);
        self.submit(Cmd::Delete { key, item: None });
    }

    /// Ouvre une cible choisie dans la liste : ferme les Paramètres, focus sur la saisie.
    fn open(&mut self, t: Target) {
        self.prefs = None;
        self.list.opened(&t);
        self.composer.focus = true;
        self.target = t;
    }

    fn compose(&mut self, ui: &mut egui::Ui) {
        match self.composer.show(ui, &self.pal, &mut self.emojis) {
            Some(ComposerAction::Send) => self.send_draft(),
            Some(ComposerAction::Leave) => self.list.focus = true,
            None => {}
        }
    }

    fn thread(&mut self, ui: &mut egui::Ui) {
        let pal = self.pal;
        // Contact choisi dont la conversation existe désormais (SMS envoyé, appel journalisé) : on y passe.
        if let Target::Contact { number, .. } = &self.target
            && self.state.conv(&number_key(number)).is_some()
        {
            self.target = Target::Conv(number_key(number));
        }
        let key = match &self.target {
            Target::Conv(key) => key.clone(),
            Target::Contact { name, number } => {
                if thread::contact(ui, &pal, name, number) {
                    let number = number.clone();
                    self.dial(&number);
                }
                return;
            }
            Target::None => return thread::placeholder(ui, &pal, t!("Select a conversation or search for a contact")),
        };
        // Lu seulement si la fenêtre est au premier plan (sinon le message n'a pas été vu).
        if ui.ctx().input(|i| i.focused) && let Some(until) = self.state.mark_read(&key) {
            self.submit(Cmd::MarkRead { key: key.clone(), until });
        }
        let Some(c) = self.state.conv(&key) else {
            w::hint(ui, &pal, t!("Sending…"));
            return;
        };
        let acts = thread::conv(ui, &pal, &mut self.emojis, c);
        let number = c.number.clone();
        self.apply(&key, &number, acts);
    }

    /// Actions demandées depuis les bulles du fil `key` (correspondant `number`).
    fn apply(&mut self, key: &str, number: &str, acts: Vec<ThreadAction>) {
        for a in acts {
            match a {
                ThreadAction::Resend(text) => drop(self.send(number, &text)),
                ThreadAction::React(emoji, text) => {
                    if let Some(t) = tapback::format(emoji, &text) {
                        self.send(number, &t);
                    }
                }
                ThreadAction::Delete(item) => {
                    self.state.delete(key, Some(&item));
                    if self.state.conv(key).is_none() {
                        self.target = Target::None;
                    }
                    self.submit(Cmd::Delete { key: key.to_owned(), item: Some(item) });
                }
                ThreadAction::Dial => self.dial(number),
            }
        }
    }

    /// Appel depuis l'iPhone : HFP prêt et aucun appel en cours ; la popin s'ouvre à la numérotation.
    fn dial(&mut self, number: &str) {
        let err = if !(self.state.hfp_up && self.state.slc) {
            Some(t!("cannot call: phone not connected (HFP)").to_owned())
        } else if !self.state.call.is_idle() {
            Some(t!("cannot call: a call is already in progress").to_owned())
        } else {
            dial_number(number).err()
        };
        match err {
            Some(e) => self.state.error = Some(e),
            None => drop(self.submit(Cmd::Dial { number: number.to_owned() })),
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(l) = self.events.try_recv() {
            self.on_link(l);
        }
        if self.state.expire_pending(false) {
            // Animation de la barre d'envoi.
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
        if let Some(p) = self.colors.poll(ctx) {
            self.pal = p;
        }
        // Zoom posé au changement seulement : Ctrl+/- d'egui reste utilisable entre deux réglages.
        if let Some(z) = self.state.settings.as_ref().map(|p| p.settings.text_size)
            && self.text_size != Some(z)
        {
            self.text_size = Some(z);
            ctx.set_zoom_factor(z.zoom());
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let pal = self.pal;
        // Retour au premier plan : curseur dans la recherche, ou dans le message si un fil est ouvert.
        let focused = ui.ctx().input(|i| i.focused);
        if focused && !self.was_focused && self.prefs.is_none() {
            self.focus_target();
        }
        self.was_focused = focused;
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::F)) {
            self.prefs = None;
            self.list.focus = true;
        }
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::Comma)) {
            self.toggle_prefs(ui.ctx());
        }
        let side = |l: i8, r: i8, t: i8, b: i8| Frame::new().fill(pal.bg).inner_margin(Margin { left: l, right: r, top: t, bottom: b });
        let p = w::PAD;
        Panel::top("head").frame(side(p, p, 2, 2)).show(ui, |ui| self.header(ui));
        if self.state.error.is_some() || self.state.info.is_some() || !self.state.call.is_idle() {
            Panel::top("alerts").show_separator_line(false).frame(side(p, p, w::TOP, 0)).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = w::GAP;
                self.call_banner(ui);
                self.error_banner(ui);
                self.info_banner(ui);
            });
        }
        Panel::left("convs").resizable(false).exact_size(340.0).frame(side(p, p, w::TOP, w::TOP)).show(ui, |ui| self.side(ui));
        if self.target != Target::None && self.prefs.is_none() {
            Panel::bottom("compose").frame(side(p, p, w::TOP, w::TOP)).show(ui, |ui| self.compose(ui));
        }
        CentralPanel::default_margins().frame(side(p, p, 0, 4)).show(ui, |ui| match self.prefs.as_mut() {
            Some(v) => {
                let links = (self.state.map_up, self.state.hfp_up && self.state.slc);
                let (cmds, close) = v.show(ui, &pal, self.state.settings.as_ref(), links);
                for c in cmds {
                    self.submit(c);
                }
                if close {
                    self.toggle_prefs(ui.ctx());
                }
            }
            None => self.thread(ui),
        });
    }

    fn on_exit(&mut self) {
        // Ferme la connexion ; le démon continue.
        self.tx = None;
    }
}
