//! `bluez-msg-call` : petite fenêtre de l'appel en cours (durée, Muet, Attente, Raccrocher, double appel),
//! dessinée avec les composants de la GUI (`bluez-message/widgets.rs`).
//! Lancée par `bluez-messaged` au décroché, placée sous la barre par une règle Hyprland (classe `bluez-msg-call`),
//! elle se ferme d'elle-même à la fin de l'appel.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use bluez_message::app::state::{Call, Phase, State};
use bluez_message::engine::Cmd;
use bluez_message::gui::theme::{self, Palette, font, size};
use bluez_message::gui::widgets::{self, Tone};
use bluez_message::gui::{self, Client};
use bluez_message::ipc::Link;
use bluez_message::t;
use clap::Parser;
use eframe::egui::{self, Align, Align2, Layout, Margin, UiBuilder, vec2};
use tokio::sync::mpsc as tokio_mpsc;

/// En-tête 44 + carte 48 + boutons 26, marges de la GUI.
const SIZE: [f32; 2] = [420.0, 150.0];
/// Lancée hors appel (ou instantané sans appel) : fermeture après ce délai.
const NO_CALL: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(about = "Ongoing call window (client of the bluez-messaged daemon)")]
struct Args {
    /// Show a fake call, without the daemon (rendering test).
    #[arg(long)]
    demo: bool,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let args = Args::parse();
    let viewport = egui::ViewportBuilder::default()
        .with_title(t!("Call"))
        .with_app_id("bluez-msg-call")
        .with_inner_size(SIZE)
        .with_resizable(false)
        .with_decorations(false)
        .with_always_on_top();
    // Démo : pas de démon, les boutons sont sans effet.
    gui::run("bluez-msg-call", viewport, !args.demo, move |c| {
        let mut popin = Popin::new(c);
        if args.demo {
            popin.state.call = demo_call();
            popin.seen = true;
        }
        popin
    })
}

fn demo_call() -> Call {
    let mut c = Call::default();
    c.phase = Phase::Active;
    c.number = Some("+33 6 12 34 56 78".into());
    c.name = Some("Bob Dupont".into());
    c.since = Instant::now().checked_sub(Duration::from_secs(83));
    c
}

struct Popin {
    pal: Palette,
    /// `colors.plm` régénéré par matugen : couleurs rechargées à chaud.
    colors: theme::Watch,
    state: State,
    tx: tokio_mpsc::UnboundedSender<Cmd>,
    events: std::sync::mpsc::Receiver<Link>,
    started: Instant,
    /// Un appel a été vu : son retour à `Idle` ferme la fenêtre.
    seen: bool,
}

fn masked(n: Option<&str>) -> &str {
    match n {
        Some(n) if !n.is_empty() => n,
        _ => t!("Hidden number"),
    }
}

fn duration(since: Option<Instant>) -> String {
    let s = since.map_or(0, |t| t.elapsed().as_secs());
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

impl Popin {
    fn new(c: Client) -> Self {
        Self { pal: c.palette, colors: theme::Watch::new(), state: State::default(), tx: c.tx, events: c.events, started: Instant::now(), seen: false }
    }

    /// `false` : la fenêtre doit se fermer.
    fn pump(&mut self) -> bool {
        while let Ok(l) = self.events.try_recv() {
            match l {
                Link::Up => self.state = State::default(),
                // Démon coupé : la fenêtre attend sa reconnexion (instantané avec l'appel) au plus `NO_CALL`.
                Link::Down(_) => {
                    self.state = State::default();
                    self.seen = false;
                    self.started = Instant::now();
                }
                Link::Event(o) => drop(self.state.apply(*o)),
            }
        }
        let idle = self.state.call.is_idle();
        self.seen |= !idle;
        !(idle && (self.seen || self.started.elapsed() > NO_CALL))
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    /// En-tête de la GUI (icône, titre 15 gras en capitales) ; durée à droite, accent.
    fn header(&self, ui: &mut egui::Ui) {
        let (c, pal) = (&self.state.call, &self.pal);
        let title = match c.phase {
            Phase::Outgoing => t!("OUTGOING CALL"),
            Phase::Incoming => t!("INCOMING CALL"),
            _ if c.hold => t!("ON HOLD"),
            _ => t!("IN CALL"),
        };
        let rect = widgets::header(ui, pal, "󰏲", title);
        if c.phase == Phase::Active {
            let at = rect.right_center() - vec2(size::EDGE, 0.0);
            ui.painter().text(at, Align2::RIGHT_CENTER, duration(c.since), egui::FontId::monospace(font::LABEL), pal.accent);
        }
    }

    /// Carte du bandeau d'appel de la GUI ; sous-ligne : double appel, micro coupé ou numéro.
    fn card(&self, ui: &mut egui::Ui) {
        let (c, pal) = (&self.state.call, &self.pal);
        let (glyph, tint) = if c.muted { ("󰍭", pal.danger) } else { ("󰏲", pal.accent) };
        let number = masked(c.number.as_deref());
        let title = c.name.as_deref().unwrap_or(number);
        let (sub, color) = match (&c.waiting, &c.held) {
            (Some(w), _) => (t!("{0} is calling", masked(Some(w))), pal.accent),
            (None, Some(h)) => (t!("On hold: {0}", masked(Some(h))), pal.accent),
            _ if c.muted => (t!("Microphone muted").to_owned(), pal.danger),
            _ if c.name.is_some() => (number.to_owned(), pal.muted()),
            _ => (String::new(), pal.muted()),
        };
        widgets::call_card(ui, pal, glyph, tint, title, &sub, color);
    }

    /// Boutons de la GUI (120 × 26) : actions à gauche, RACCROCHER à droite. Les actions d'attente
    /// n'apparaissent qu'en double appel ; MUET passe en rouge « MICRO COUPÉ » quand le démon l'a confirmé.
    fn buttons(&self, ui: &mut egui::Ui) {
        let (c, pal) = (&self.state.call, &self.pal);
        let mut cmd = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let mut b = |ui: &mut egui::Ui, label: &str, tone: Tone, what: Cmd| {
                if widgets::button(ui, pal, label, tone).clicked() {
                    cmd = Some(what);
                }
            };
            if c.waiting.is_some() {
                b(ui, t!("ANSWER"), Tone::Accent, Cmd::HoldAnswer);
                b(ui, t!("DECLINE"), Tone::Neutral, Cmd::Reject);
            } else {
                if c.phase == Phase::Active || c.phase == Phase::Outgoing {
                    let mute = if c.muted { (t!("MIC OFF"), Tone::Danger) } else { (t!("MUTE"), Tone::Neutral) };
                    b(ui, mute.0, mute.1, Cmd::Mute);
                }
                if c.hold || c.held.is_some() {
                    b(ui, t!("SWAP"), Tone::Accent, Cmd::Swap);
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| b(ui, t!("HANG UP"), Tone::Danger, Cmd::Hangup));
        });
        if let Some(cmd) = cmd {
            self.send(cmd);
        }
    }
}

impl eframe::App for Popin {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !self.pump() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if let Some(p) = self.colors.poll(&ctx) {
            self.pal = p;
        }
        // Panneau de la GUI : fond `bg`, marges latérales 14 (bandeau d'appel), bas 12.
        let frame = egui::Frame::new().fill(self.pal.bg).inner_margin(Margin { left: 14, right: 14, top: 0, bottom: 12 });
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = widgets::GAP;
            self.header(ui);
            self.card(ui);
            ui.add_space(4.0);
            let r = ui.available_rect_before_wrap();
            ui.scope_builder(UiBuilder::new().max_rect(r), |ui| self.buttons(ui));
        });
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}
