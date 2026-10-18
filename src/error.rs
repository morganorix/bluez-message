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
    /// Client input rejected before any action.
    Invalid(Field),
    /// Client command that is not valid JSON or not a known command.
    BadCommand(serde_json::Error),
    /// Settings edited from an older revision: applying them would undo a change made elsewhere.
    StaleSettings,
    /// `XDG_RUNTIME_DIR` missing or relative: no socket path.
    NoRuntimeDir,
    /// A bounded queue is full: the command is dropped, not delayed.
    Busy(Queue),
    /// Command only the `bluez-messaged` daemon handles, sent to an engine.
    DaemonOnly,
    /// Operation bounded in time that did not finish.
    Timeout,
    /// Microphone mute requested outside a call.
    MicNoCall,
    /// `wpctl` did not report a usable default source.
    MicNoSource,
}

/// Client field rejected by [`Error::Invalid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// Empty text or longer than [`crate::engine::MAX_TEXT`].
    Text,
    ReadMarker,
    Search,
    HistoryRequest,
    Deletion,
    Conversation,
    Language,
    Retention,
    Country,
    QuickReplies,
}

/// Queue named by [`Error::Busy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queue {
    Send,
    Hfp,
    Setup,
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
            Self::Invalid(Field::Text) => t!("empty text or longer than {0} characters", crate::engine::MAX_TEXT),
            Self::Invalid(Field::ReadMarker) => t!("invalid read marker").to_owned(),
            Self::Invalid(Field::Search) => t!("invalid search").to_owned(),
            Self::Invalid(Field::HistoryRequest) => t!("invalid history request").to_owned(),
            Self::Invalid(Field::Deletion) => t!("invalid deletion").to_owned(),
            Self::Invalid(Field::Conversation) => t!("invalid conversation").to_owned(),
            Self::Invalid(Field::Language) => t!("invalid language code").to_owned(),
            Self::Invalid(Field::Retention) => t!("invalid retention").to_owned(),
            Self::Invalid(Field::Country) => t!("unknown country").to_owned(),
            Self::Invalid(Field::QuickReplies) => t!("invalid quick replies").to_owned(),
            // Kind and position only: serde quotes the offending value (number, address).
            Self::BadCommand(e) => t!("unreadable command: {0}", format!("{:?} {}:{}", e.classify(), e.line(), e.column())),
            Self::StaleSettings => t!("settings changed elsewhere, try again").to_owned(),
            Self::NoRuntimeDir => t!("XDG_RUNTIME_DIR missing or relative").to_owned(),
            Self::Busy(Queue::Send) => t!("send queue full").to_owned(),
            Self::Busy(Queue::Hfp) => t!("HFP busy").to_owned(),
            Self::Busy(Queue::Setup) => t!("setup queue full").to_owned(),
            Self::DaemonOnly => t!("command handled by the bluez-messaged daemon").to_owned(),
            Self::Timeout => t!("timed out").to_owned(),
            Self::MicNoCall => t!("microphone: no call in progress").to_owned(),
            Self::MicNoSource => t!("microphone: default source not found").to_owned(),
        };
        f.write_str(&s)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn bad_command_hides_value() {
        let e = serde_json::from_str::<u8>("\"+33600000001\"").unwrap_err();
        assert!(!Error::BadCommand(e).to_string().contains("336"));
    }

    #[test]
    fn invalid_input_masked() {
        let n = Error::InvalidNumber("+33600000001x".into()).to_string();
        assert!(!n.contains("33600000") && n.ends_with("****001x"), "{n}");
        let a = Error::InvalidAddress("AA:BB:CC:DD:EE:FG".into()).to_string();
        assert!(!a.contains("AA:BB") && a.ends_with("****E:FG"), "{a}");
    }
}
