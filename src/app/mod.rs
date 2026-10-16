//! Logic shared by the `bluez-messaged` daemon and the GUI: state (conversations, call) and notifications.

#[cfg(feature = "notify")]
pub mod notify;
pub mod state;
pub mod tapback;
