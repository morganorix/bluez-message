//! Réglages de l'app (`~/.config/bluez-message/settings.json`), écrits par le démon seul ; la GUI les reçoit
//! par [`crate::engine::Out::Settings`] et les modifie par [`crate::engine::Cmd::Settings`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::t;

/// Durées de conservation proposées (jours) ; `None` : tout garder.
pub const RETENTIONS: [u32; 3] = [30, 90, 365];

/// Taille de l'interface des fenêtres (zoom egui : texte et espacements ensemble).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSize {
    #[default]
    Normal,
    Large,
    ExtraLarge,
}

impl TextSize {
    /// Dans l'ordre d'affichage.
    pub const ALL: [Self; 3] = [Self::Normal, Self::Large, Self::ExtraLarge];

    /// Facteur de zoom de l'interface.
    #[must_use]
    pub const fn zoom(self) -> f32 {
        match self {
            Self::Normal => 1.0,
            Self::Large => 1.15,
            Self::ExtraLarge => 1.3,
        }
    }

    /// Taille suivante, en boucle.
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
    /// Code de langue (`fr`, `pt_BR`…) ; `None` : celle du système.
    pub lang: Option<String>,
    pub notify_sms: bool,
    pub notify_calls: bool,
    /// Notification de SMS sans le texte.
    pub hide_preview: bool,
    /// SMS et appels plus anciens effacés de l'historique local ; `None` : tout garder.
    pub retention_days: Option<u32>,
    pub text_size: TextSize,
    /// iPhone non relié : le démon relance la connexion Bluetooth régulièrement.
    pub auto_connect: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { lang: None, notify_sms: true, notify_calls: true, hide_preview: false, retention_days: None, text_size: TextSize::Normal, auto_connect: true }
    }
}

/// Code de langue utilisable comme nom de fichier : lettres, chiffres, `_`, `-` (pas de traversée).
#[must_use]
pub fn valid_lang(code: &str) -> bool {
    !code.is_empty() && code.len() <= 16 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl Settings {
    /// Contrôle des valeurs reçues d'un client.
    ///
    /// # Errors
    /// Message traduit : langue ou rétention invalide.
    pub fn validate(&self) -> Result<(), String> {
        if self.lang.as_deref().is_some_and(|l| !valid_lang(l)) {
            return Err(t!("invalid language code").to_owned());
        }
        if self.retention_days.is_some_and(|d| !RETENTIONS.contains(&d)) {
            return Err(t!("invalid retention").to_owned());
        }
        Ok(())
    }

    fn path() -> Option<PathBuf> {
        crate::config::config_dir().map(|d| d.join("bluez-message/settings.json"))
    }

    /// Réglages enregistrés ; absents ou invalides : défauts (avertissement au journal).
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

    /// Écriture atomique (fichier temporaire 0600 puis renommage).
    ///
    /// # Errors
    /// `HOME` absent ou écriture impossible.
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
        let ok = Settings { lang: Some("pt_BR".into()), retention_days: Some(90), ..Settings::default() };
        assert!(ok.validate().is_ok());
    }
}
