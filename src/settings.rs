//! App settings (`~/.config/bluez-message/settings.json`), written by the daemon only; the GUI receives them
//! through [`crate::engine::Out::Settings`] and changes them through [`crate::engine::Cmd::Settings`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::t;

/// Offered retention periods (days); `None`: keep everything.
pub const RETENTIONS: [u32; 3] = [30, 90, 365];
/// Bounds of [`Settings::quick_replies`]: number of texts, characters per text (one SMS).
pub const QUICK_REPLIES_MAX: usize = 5;
pub const QUICK_REPLY_LEN: usize = 160;

/// Window interface size (egui zoom: text and spacing together).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSize {
    #[default]
    Normal,
    Large,
    ExtraLarge,
}

impl TextSize {
    /// In display order.
    pub const ALL: [Self; 3] = [Self::Normal, Self::Large, Self::ExtraLarge];

    /// Interface zoom factor.
    #[must_use]
    pub const fn zoom(self) -> f32 {
        match self {
            Self::Normal => 1.0,
            Self::Large => 1.15,
            Self::ExtraLarge => 1.3,
        }
    }

    /// Next size, looping.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Normal => Self::Large,
            Self::Large => Self::ExtraLarge,
            Self::ExtraLarge => Self::Normal,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => t!("Normal"),
            Self::Large => t!("Large"),
            Self::ExtraLarge => t!("Extra large"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Language code (`fr`, `pt_BR`…); `None`: the system's.
    pub lang: Option<String>,
    pub notify_sms: bool,
    pub notify_calls: bool,
    /// SMS notification without the text.
    pub hide_preview: bool,
    /// SMS and calls older than this are erased from the local history; `None`: keep everything.
    pub retention_days: Option<u32>,
    pub text_size: TextSize,
    /// iPhone not linked: the daemon regularly retries the Bluetooth connection.
    pub auto_connect: bool,
    /// Home country for the numbering plan (`fr`…, see [`crate::numbering`]); `None`: from the locale.
    pub country: Option<String>,
    /// Texts offered when declining a call; the first one is sent by "Decline + SMS".
    pub quick_replies: Vec<String>,
    /// Preview card under a web link (title, image): the GUI contacts the linked site.
    pub link_previews: bool,
}

/// The default quick replies are in the active language (`t!`): written to `settings.json` on the first save,
/// they do not follow a later language change.
impl Default for Settings {
    fn default() -> Self {
        Self { lang: None, notify_sms: true, notify_calls: true, hide_preview: false, retention_days: None, text_size: TextSize::Normal, auto_connect: true, country: None, quick_replies: vec![t!("Can't talk now, I'll call you back.").to_owned(), t!("I'm in a meeting.").to_owned()], link_previews: false }
    }
}

/// Quick reply as the settings keep it: trimmed, control characters removed, at most [`QUICK_REPLY_LEN`] characters.
#[must_use]
pub fn clean_reply(text: &str) -> String {
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    text.trim().chars().take(QUICK_REPLY_LEN).collect::<String>().trim_end().to_owned()
}

/// Language code usable as a file name: letters, digits, `_`, `-` (no traversal).
#[must_use]
pub fn valid_lang(code: &str) -> bool {
    !code.is_empty() && code.len() <= 16 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl Settings {
    /// Check of the values received from a client.
    ///
    /// # Errors
    /// Translated message: invalid language, retention, country or quick replies.
    pub fn validate(&self) -> Result<(), String> {
        if self.lang.as_deref().is_some_and(|l| !valid_lang(l)) {
            return Err(t!("invalid language code").to_owned());
        }
        if self.retention_days.is_some_and(|d| !RETENTIONS.contains(&d)) {
            return Err(t!("invalid retention").to_owned());
        }
        if self.country.as_deref().is_some_and(|c| crate::numbering::parse_code(c).is_none()) {
            return Err(t!("unknown country").to_owned());
        }
        if self.quick_replies.len() > QUICK_REPLIES_MAX || self.quick_replies.iter().any(|r| r.is_empty() || clean_reply(r) != *r) {
            return Err(t!("invalid quick replies").to_owned());
        }
        Ok(())
    }

    fn path() -> Option<PathBuf> {
        crate::config::config_dir().map(|d| d.join("bluez-message/settings.json"))
    }

    /// Saved settings; absent or invalid: defaults (warning in the log).
    pub fn load() -> Self {
        let Some(p) = Self::path() else { return Self::default() };
        let raw = match std::fs::read_to_string(&p) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!(path = %p.display(), "settings unreadable: {e}");
                return Self::default();
            }
        };
        match serde_json::from_str::<Self>(&raw).map_err(|e| e.to_string()).and_then(|s| s.validate().map(|()| s)) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %p.display(), "settings ignored: {e}");
                Self::default()
            }
        }
    }

    /// Atomic write (temporary file 0600 then rename).
    ///
    /// # Errors
    /// `HOME` missing or write impossible.
    pub fn save(&self) -> std::io::Result<()> {
        let p = Self::path().ok_or_else(|| std::io::Error::other("HOME not set"))?;
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::config::write_private(&p, json.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_partial_json() {
        let s: Settings = serde_json::from_str(r#"{"notify_sms": false}"#).unwrap();
        assert_eq!(s, Settings { notify_sms: false, ..Settings::default() });
        assert!(s.validate().is_ok());
        let s: Settings = serde_json::from_str(r#"{"text_size": "extra_large"}"#).unwrap();
        assert_eq!(s.text_size, TextSize::ExtraLarge);
        assert!(serde_json::from_str::<Settings>(r#"{"text_size": "huge"}"#).is_err());
    }

    #[test]
    fn validation() {
        let bad_lang = Settings { lang: Some("../x".into()), ..Settings::default() };
        assert!(bad_lang.validate().is_err());
        let bad_days = Settings { retention_days: Some(7), ..Settings::default() };
        assert!(bad_days.validate().is_err());
        let bad_country = Settings { country: Some("xx".into()), ..Settings::default() };
        assert!(bad_country.validate().is_err());
        let ok = Settings { lang: Some("pt_BR".into()), retention_days: Some(90), country: Some("fr".into()), ..Settings::default() };
        assert!(ok.validate().is_ok());
        for bad in [vec![String::new()], vec![" x".into()], vec!["a\nb".into()], vec!["é".repeat(161)], vec!["x".into(); 6]] {
            assert!(Settings { quick_replies: bad, ..Settings::default() }.validate().is_err());
        }
        let ok = Settings { quick_replies: vec!["é".repeat(160); 5], ..Settings::default() };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn quick_replies() {
        assert_eq!(clean_reply("  On\u{7}my way\n "), "Onmy way");
        assert_eq!(clean_reply(&"é".repeat(200)).chars().count(), QUICK_REPLY_LEN);
        let cut = format!("{} x", "é".repeat(159));
        assert_eq!(clean_reply(&cut), clean_reply(&clean_reply(&cut)), "idempotent at the cut");
        // Older settings file: default texts.
        let s: Settings = serde_json::from_str(r#"{"notify_sms": true}"#).unwrap();
        assert_eq!(s.quick_replies.len(), 2);
        let s: Settings = serde_json::from_str(r#"{"quick_replies": []}"#).unwrap();
        assert!(s.quick_replies.is_empty() && s.validate().is_ok());
    }
}
