use std::fmt;

use crate::t;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    InvalidNumber(String),
    InvalidAddress(String),
    InvalidFolder(String),
    /// `HOME` missing: no configuration or data directory.
    NoHome,
    TransferFailed,
    TransferTimeout,
    Dbus(#[from] zbus::Error),
    Fdo(#[from] zbus::fdo::Error),
    Io(#[from] std::io::Error),
    Db(#[from] rusqlite::Error),
    Hfp(String),
    /// `user_version` of a history written by a newer version (or corrupted).
    DbVersion(i64),
    /// A pairing is already running (one at a time).
    PairingBusy,
    /// `Confirm` received while no passkey is waiting.
    NoConfirmation,
}

/// Translated messages (`thiserror` only accepts literals); system details left as is.
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::InvalidNumber(n) => t!("invalid number: {0}", crate::log::mask(n)),
            Self::InvalidAddress(a) => t!("invalid Bluetooth address: {0}", crate::log::mask(a)),
            Self::InvalidFolder(d) => t!("invalid MAP folder: {0}", format!("{d:?}")),
            Self::NoHome => t!("HOME not set").to_owned(),
            Self::TransferFailed => t!("OBEX transfer failed").to_owned(),
            Self::TransferTimeout => t!("OBEX transfer timed out").to_owned(),
            Self::Dbus(e) => format!("D-Bus: {e}"),
            Self::Fdo(e) => format!("D-Bus: {e}"),
            Self::Io(e) => t!("I/O: {0}", e),
            Self::Db(e) => format!("SQLite: {e}"),
            Self::Hfp(e) => format!("HFP: {e}"),
            Self::DbVersion(v) => t!("history database version {0} is not supported", v),
            Self::PairingBusy => t!("pairing already in progress").to_owned(),
            Self::NoConfirmation => t!("no pairing waiting for confirmation").to_owned(),
        };
        f.write_str(&s)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn invalid_input_masked() {
        let n = Error::InvalidNumber("+33600000001x".into()).to_string();
        assert!(!n.contains("33600000") && n.ends_with("****001x"), "{n}");
        let a = Error::InvalidAddress("AA:BB:CC:DD:EE:FG".into()).to_string();
        assert!(!a.contains("AA:BB") && a.ends_with("****E:FG"), "{a}");
    }
}
