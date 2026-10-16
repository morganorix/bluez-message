use std::fmt;

use crate::t;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    InvalidNumber(String),
    InvalidAddress(String),
    InvalidFolder(String),
    TransferFailed,
    TransferTimeout,
    Dbus(#[from] zbus::Error),
    Fdo(#[from] zbus::fdo::Error),
    Io(#[from] std::io::Error),
    Hfp(String),
}

/// Messages traduits (`thiserror` n'accepte que des littéraux) ; détails système laissés tels quels.
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::InvalidNumber(n) => t!("invalid number: {0}", format!("{n:?}")),
            Self::InvalidAddress(a) => t!("invalid Bluetooth address: {0}", format!("{a:?}")),
            Self::InvalidFolder(d) => t!("invalid MAP folder: {0}", format!("{d:?}")),
            Self::TransferFailed => t!("OBEX transfer failed").to_owned(),
            Self::TransferTimeout => t!("OBEX transfer timed out").to_owned(),
            Self::Dbus(e) => format!("D-Bus: {e}"),
            Self::Fdo(e) => format!("D-Bus: {e}"),
            Self::Io(e) => t!("I/O: {0}", e),
            Self::Hfp(e) => format!("HFP: {e}"),
        };
        f.write_str(&s)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
