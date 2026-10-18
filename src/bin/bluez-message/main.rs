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

use bluez_message::engine::Cmd;
use bluez_message::gui;
use clap::Parser;
use eframe::egui;

#[derive(Parser)]
#[command(about = "iPhone SMS and calls: client window of the bluez-messaged daemon")]
struct Args {
    /// Shows the SMS conversation with this number (passed on to the window already open).
    #[arg(long, value_name = "NUMBER", value_parser = open_number)]
    open: Option<String>,
}

/// Number set by the daemon in the environment ([`bluez_message::ipc::OPEN_ENV`]); an invalid or
/// inherited value is ignored (never printed: it is a number).
fn env_open() -> Option<String> {
    let raw = std::env::var(bluez_message::ipc::OPEN_ENV).ok()?;
    let n = open_number(&raw);
    if n.is_err() {
        tracing::warn!("{}: invalid number ignored", bluez_message::ipc::OPEN_ENV);
    }
    n.ok()
}

/// Same form as the daemon check: separators removed, `+` then 3 to 20 digits.
fn open_number(raw: &str) -> Result<String, String> {
    let n = bluez_message::phone::strip_separators(raw);
    bluez_message::bmsg::validate_number(&n).map_err(|e| e.to_string())?;
    Ok(n)
}

fn main() -> ExitCode {
    bluez_message::log::init();
    let mut args = Args::parse();
    args.open = args.open.or_else(env_open);
    // Single window: a second launch brings the first forward and exits.
    let _lock = match instance_lock() {
        Ok(f) => f,
        Err(TryLockError::WouldBlock) => {
            // The open window comes forward (on `--open` if given); without the daemon, it stays as it is.
            return match (forward_open(args.open.clone()), args.open) {
                (Ok(()), _) => ExitCode::SUCCESS,
                (Err(e), Some(_)) => {
                    eprintln!("bluez-message: {e}");
                    ExitCode::FAILURE
                }
                (Err(_), None) => {
                    eprintln!("bluez-message: already open");
                    ExitCode::SUCCESS
                }
            };
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
    gui::run("bluez-message", viewport, true, move |c| ui::App::new(c.palette, c.tx, c.events, args.open))
}

/// Instance lock `bluez-message-gui.lock` next to the daemon socket, released by the system on exit (even
/// on crash). `WouldBlock`: a window is already open. Without `XDG_RUNTIME_DIR`: no lock.
fn instance_lock() -> Result<Option<File>, TryLockError> {
    let Ok(path) = bluez_message::config::gui_lock_path() else { return Ok(None) };
    let f = bluez_message::ipc::lock_file(&path).map_err(TryLockError::Error)?;
    f.try_lock()?;
    Ok(Some(f))
}

/// Window already open: the daemon passes the request on to it ([`Cmd::Open`]). `quit` then closes the
/// connection once `open` is queued; the snapshot is drained until then, or the daemon would stop at a failed write
/// before reading the command.
fn forward_open(number: Option<String>) -> std::io::Result<()> {
    use std::io::Write;
    let sock = bluez_message::config::socket_path().map_err(std::io::Error::other)?;
    let mut s = std::os::unix::net::UnixStream::connect(sock)?;
    let wait = Some(std::time::Duration::from_secs(2));
    s.set_write_timeout(wait)?;
    s.set_read_timeout(wait)?;
    let mut lines = Vec::new();
    for cmd in [Cmd::Open { number }, Cmd::Quit] {
        serde_json::to_writer(&mut lines, &cmd).map_err(std::io::Error::other)?;
        lines.push(b'\n');
    }
    s.write_all(&lines)?;
    match std::io::copy(&mut s, &mut std::io::sink()) {
        // Already delivered: a daemon slow to close the connection is no failure.
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Ok(()),
        r => r.map(drop),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_number_normalises_and_rejects() {
        assert_eq!(open_number("+33 6 00-00-00-00").unwrap(), "+33600000000");
        assert!(open_number("abc").is_err());
        assert!(Args::try_parse_from(["bluez-message", "--open", "12"]).is_err());
    }
}
