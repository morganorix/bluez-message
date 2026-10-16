//! `bluez-message` : SMS et appels de l'iPhone dans une fenêtre egui. Client du démon `bluez-messaged`
//! (moteur et notifications) : fermer la fenêtre n'interrompt rien.

mod composer;
mod convs;
mod prefs;
mod thread;
mod ui;

use std::fs::{File, OpenOptions, TryLockError};
use std::process::ExitCode;

use bluez_message::gui;
use clap::Parser;
use eframe::egui;

#[derive(Parser)]
#[command(about = "iPhone SMS and calls: client window of the bluez-messaged daemon")]
struct Args {}

fn main() -> ExitCode {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    Args::parse();
    // Une seule fenêtre : un second lancement se retire sans rien faire.
    let _lock = match instance_lock() {
        Ok(f) => f,
        Err(TryLockError::WouldBlock) => {
            eprintln!("bluez-message: already open");
            return ExitCode::SUCCESS;
        }
        Err(TryLockError::Error(e)) => {
            tracing::warn!("instance lock: {e}");
            None
        }
    };
    let viewport = egui::ViewportBuilder::default()
        .with_title("bmessage")
        .with_app_id("bluez-message")
        .with_inner_size([900.0, 640.0])
        .with_min_inner_size([640.0, 420.0])
        .with_active(true);
    gui::run("bluez-message", viewport, true, |c| ui::App::new(c.palette, c.tx, c.events))
}

/// Verrou d'instance `bluez-message-gui.lock` près du socket du démon, libéré par le système à la sortie (même
/// sur plantage). `WouldBlock` : une fenêtre est déjà ouverte. Sans `XDG_RUNTIME_DIR` : pas de verrou.
fn instance_lock() -> Result<Option<File>, TryLockError> {
    let Ok(sock) = bluez_message::config::socket_path() else { return Ok(None) };
    let f = OpenOptions::new().create(true).truncate(false).write(true).open(sock.with_file_name("bluez-message-gui.lock")).map_err(TryLockError::Error)?;
    f.try_lock()?;
    Ok(Some(f))
}
