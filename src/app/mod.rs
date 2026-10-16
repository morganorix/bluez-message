//! Logique commune au démon `bluez-messaged` et à la GUI : état (conversations, appel) et notifications.

#[cfg(feature = "notify")]
pub mod notify;
pub mod state;
pub mod tapback;
