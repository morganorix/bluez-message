//! `bluez-message`: iPhone SMS and calls in an egui window. Client of the `bluez-messaged` daemon
//! (engine and notifications): closing the window interrupts nothing.

mod composer;
mod convs;
mod prefs;
mod setup;
mod thread;
mod ui;

use std::fs::{File, TryLockError};
use std::process::ExitCode;

use bluez_message::gui;
use clap::Parser;
use eframe::egui;

#[derive(Parser)]
#[command(about = "iPhone SMS and calls: client window of the bluez-messaged daemon")]
struct Args {}

fn main() -> ExitCode {
    bluez_message::log::init();
    Args::parse();
    // Single window: a second launch exits without doing anything.
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

/// Instance lock `bluez-message-gui.lock` next to the daemon socket, released by the system on exit (even
/// on crash). `WouldBlock`: a window is already open. Without `XDG_RUNTIME_DIR`: no lock.
fn instance_lock() -> Result<Option<File>, TryLockError> {
    let Ok(sock) = bluez_message::config::socket_path() else { return Ok(None) };
    let f = bluez_message::ipc::lock_file(&sock.with_file_name("bluez-message-gui.lock")).map_err(TryLockError::Error)?;
    f.try_lock()?;
    Ok(Some(f))
}
