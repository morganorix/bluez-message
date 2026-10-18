//! Setup wizard (right panel, like Settings): adapter, device (paired, search and pairing, or typed
//! address), iPhone settings, end. Opened while no device is configured, or from Settings > Devices.
//! The adapter and pairing belong to the daemon: each step sends commands, the display follows its events.

use bluez_message::addr::BtAddr;
use bluez_message::app::state::State;
use bluez_message::engine::Cmd;
use bluez_message::gui::icons::Icon;
use bluez_message::gui::theme::{Palette, font, size};
use bluez_message::gui::widgets as w;
use bluez_message::t;
use eframe::egui::{self, ScrollArea};

const STEPS: u8 = 4;

#[derive(Default)]
pub struct Wizard {
    /// 0-based step.
    step: u8,
    /// Address typed by hand.
    manual: String,
    /// Typed address rejected.
    bad: bool,
    /// Device chosen at step 2 (`None`: the configured one).
    chosen: Option<BtAddr>,
    /// Pairing requested from here, awaiting its result.
    asked: Option<BtAddr>,
}

impl Wizard {
    /// Rendering; returns the commands to send to the daemon and `true` if the wizard must close.
    pub fn show(&mut self, ui: &mut egui::Ui, pal: &Palette, st: &State) -> (Vec<Cmd>, bool) {
        let device = st.settings.as_ref().and_then(|p| p.device.as_ref());
        let rect = w::header_with(ui, pal, Icon::Bluetooth, t!("SETUP"), 60.0);
        // Without a configured device, there is nothing to go back to.
        let mut close = device.is_some() && w::header_button(ui, pal, rect, Icon::Close).on_hover_text(t!("Close")).clicked();
        w::rule(ui, pal);
        ui.add_space(12.0);
        let mut cmds = vec![];
        let title = match self.step {
            0 => t!("Bluetooth adapter"),
            1 => t!("Device"),
            2 => t!("iPhone settings"),
            _ => t!("Done"),
        };
        ui.label(w::text(format!("{} · {title}", t!("Step {0}/{1}", self.step + 1, STEPS)), font::SMALL, pal.muted()));
        ui.add_space(w::GAP * 2.0);
        let nav_h = size::BUTTON_H + w::GAP * 4.0;
        let ready = ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height((ui.available_height() - nav_h).max(0.0))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = w::ROW_GAP;
                match self.step {
                    0 => adapter(ui, pal, st, &mut cmds),
                    1 => self.device(ui, pal, st, device, &mut cmds),
                    2 => iphone(ui, pal, st),
                    // Until the restarted daemon reports the device, closing would be refused.
                    _ if device.is_none() => {
                        w::hint(ui, pal, t!("Waiting for the daemon…"));
                        false
                    }
                    _ => {
                        w::hint(ui, pal, t!("Messages and calls of the iPhone now go through this PC."));
                        true
                    }
                }
            })
            .inner;
        ui.add_space(w::GAP * 2.0);
        let (back, next) = ui
            .horizontal(|ui| {
                let back = self.step > 0 && w::button(ui, pal, t!("BACK"), w::Tone::Neutral).clicked();
                let label = if self.step + 1 == STEPS { t!("FINISH") } else { t!("NEXT") };
                let next = ui.add_enabled_ui(ready, |ui| w::button(ui, pal, label, w::Tone::Accent)).inner.clicked();
                (back, next)
            })
            .inner;
        // Closing goes through `App::close_setup`, which sends `leave` itself.
        if back || next {
            cmds.extend(self.leave(st));
        }
        if back {
            self.step -= 1;
        } else if next {
            if self.step == 1
                && let Some(a) = &self.chosen
                && device != Some(a)
            {
                // The daemon stores the device and restarts: the wizard waits for it at step 3. The
                // choice is kept (Back shows it); sending it again for the same device is a no-op.
                cmds.push(Cmd::SetDevice { addr: a.clone() });
            }
            if self.step + 1 == STEPS {
                close = true;
            } else {
                self.step += 1;
            }
        }
        (cmds, close)
    }

    /// Commands to send when leaving the current step (or the wizard): the search and a pending
    /// confirmation only serve step 2. Stopping an idle search is harmless, so it is not checked
    /// (a search just asked may not be reported yet).
    pub fn leave(&self, st: &State) -> Vec<Cmd> {
        let mut cmds = vec![];
        if self.step == 1 {
            cmds.push(Cmd::Discover { on: false });
            if st.pairing.confirm.is_some() {
                cmds.push(Cmd::Confirm { ok: false });
            }
        }
        cmds
    }

    /// Step 2: paired devices, search and pairing, typed address. Ready once a device is chosen.
    fn device(&mut self, ui: &mut egui::Ui, pal: &Palette, st: &State, device: Option<&BtAddr>, cmds: &mut Vec<Cmd>) -> bool {
        let p = &st.pairing;
        // Pairing requested from here and successful: that device is chosen.
        if let (Some(a), Some((b, Ok(())))) = (&self.asked, &p.paired)
            && a == b
        {
            self.chosen = self.asked.take();
        }
        let chosen = self.chosen.clone().or_else(|| device.cloned());
        section(ui, pal, t!("Paired devices"));
        let devices = st.settings.as_ref().map_or(&[][..], |s| &s.devices);
        // Chosen but in no list yet (just paired, typed by hand): shown so the choice is visible.
        let listed = |a: &BtAddr| devices.iter().any(|d| d.addr == *a) || p.found.iter().any(|n| n.addr == *a && !n.paired);
        let extra = chosen.as_ref().filter(|a| !listed(a));
        if devices.is_empty() && extra.is_none() {
            w::hint(ui, pal, t!("No paired device"));
        }
        if let Some(a) = extra {
            let name = p.found.iter().find(|n| n.addr == *a).map_or(a.as_str(), |n| n.name.as_str());
            w::row(ui, pal, &row(name, "", true));
        }
        for d in devices {
            let hint = if d.connected { t!("connected") } else { "" };
            if w::row(ui, pal, &row(&d.name, hint, chosen.as_ref() == Some(&d.addr))).clicked() {
                self.chosen = Some(d.addr.clone());
            }
        }

        section(ui, pal, t!("Nearby devices"));
        let searching = p.adapter.is_some_and(|a| a.discovering);
        ui.horizontal(|ui| {
            let label = if searching { t!("STOP") } else { t!("SEARCH") };
            if w::button(ui, pal, label, w::Tone::Accent).clicked() {
                cmds.push(Cmd::Discover { on: !searching });
            }
            if searching {
                w::hint(ui, pal, t!("Searching… Make the iPhone visible: Settings > Bluetooth."));
            }
        });
        if let Some((addr, passkey)) = &p.confirm {
            ui.add_space(w::GAP);
            let name = p.found.iter().find(|n| n.addr == *addr).map_or(addr.as_str(), |n| n.name.as_str());
            ui.label(w::text(t!("Does {0} show {1}?", name, format!("{passkey:06}")), font::LABEL, pal.fg));
            ui.horizontal(|ui| {
                if w::button(ui, pal, t!("YES"), w::Tone::Accent).clicked() {
                    cmds.push(Cmd::Confirm { ok: true });
                }
                if w::button(ui, pal, t!("NO"), w::Tone::Danger).clicked() {
                    cmds.push(Cmd::Confirm { ok: false });
                }
            });
        } else if let (Some(a), Some((b, Err(e)))) = (&self.asked, &p.paired)
            && a == b
        {
            ui.label(w::text(t!("Pairing failed: {0}", e), font::BODY, pal.danger));
        }
        // Not paired yet, phones first.
        let mut near: Vec<_> = p.found.iter().filter(|n| !n.paired).collect();
        near.sort_by_key(|n| !n.phone);
        if searching && near.is_empty() {
            w::hint(ui, pal, t!("No device found yet"));
        }
        for n in near {
            let pending = self.asked.as_ref() == Some(&n.addr) && p.paired.as_ref().is_none_or(|(b, _)| *b != n.addr);
            let hint = if pending { t!("pairing…") } else { t!("pair") };
            if w::row(ui, pal, &row(&n.name, hint, false)).clicked() && !pending {
                self.asked = Some(n.addr.clone());
                cmds.push(Cmd::Pair { addr: n.addr.clone() });
            }
        }

        section(ui, pal, t!("Address typed by hand"));
        ui.spacing_mut().interact_size.y = size::FIELD_H;
        ui.horizontal(|ui| {
            let wid = ui.available_width() - size::BUTTON_W - ui.spacing().item_spacing.x;
            let r = ui.add(w::field(&mut self.manual, "00:11:22:33:44:55").desired_width(wid.max(100.0)));
            if r.changed() {
                self.bad = false;
            }
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if w::button(ui, pal, t!("USE"), w::Tone::Neutral).clicked() || enter {
                match self.manual.trim().parse::<BtAddr>() {
                    Ok(a) => self.chosen = Some(a),
                    Err(_) => self.bad = true,
                }
            }
        });
        if self.bad {
            ui.label(w::text(t!("Invalid address"), font::BODY, pal.danger));
        }
        self.chosen.is_some() || device.is_some()
    }
}

/// Step 1: adapter present and powered.
fn adapter(ui: &mut egui::Ui, pal: &Palette, st: &State, cmds: &mut Vec<Cmd>) -> bool {
    let Some(a) = st.pairing.adapter else {
        w::hint(ui, pal, t!("Waiting for the daemon…"));
        return false;
    };
    let (icon, label) = match (a.present, a.powered) {
        (false, _) => (Icon::BluetoothOff, t!("No Bluetooth adapter found")),
        (true, false) => (Icon::BluetoothOff, t!("Adapter off")),
        (true, true) => (Icon::Bluetooth, t!("Adapter on")),
    };
    w::row(ui, pal, &w::Row { icon: Some(icon), label, sub: None, hint: "", badge: 0, selected: false, strong: a.powered });
    if a.present && !a.powered {
        ui.add_space(w::GAP);
        if w::button(ui, pal, t!("TURN ON"), w::Tone::Accent).clicked() {
            cmds.push(Cmd::Powered { on: true });
        }
    }
    a.powered
}

/// Step 3: what to allow on the iPhone, with the matching links.
fn iphone(ui: &mut egui::Ui, pal: &Palette, st: &State) -> bool {
    w::hint(ui, pal, t!("On the iPhone: Settings > Bluetooth > (i) next to this PC, turn on Show Notifications and Sync Contacts."));
    ui.add_space(w::GAP * 2.0);
    let map = if st.map_up { w::Link::On } else { w::Link::Off };
    let pbap = match st.pbap {
        Some(true) => w::Link::On,
        Some(false) => w::Link::Pending,
        None => w::Link::Off,
    };
    for (icon, link, label) in [(Icon::MessageText, map, t!("Messages (MAP)")), (Icon::Contacts, pbap, t!("Contacts (PBAP)"))] {
        ui.horizontal(|ui| {
            w::status_dot(ui, pal, icon, link);
            ui.label(w::text(label, font::BODY, pal.fg));
        });
    }
    true
}

fn section(ui: &mut egui::Ui, pal: &Palette, title: &str) {
    ui.add_space(w::GAP * 2.0);
    ui.label(w::text(title, font::SMALL, pal.muted()));
    ui.add_space(w::GAP);
}

fn row<'a>(label: &'a str, hint: &'a str, selected: bool) -> w::Row<'a> {
    w::Row { icon: Some(Icon::PhoneBluetooth), label, sub: None, hint, badge: 0, selected, strong: selected }
}
