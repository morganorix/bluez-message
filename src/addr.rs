//! Adresse Bluetooth validée (`AA:BB:CC:DD:EE:FF`, majuscules).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Adresse Bluetooth : six octets hexadécimaux séparés par `:`, normalisée en majuscules (forme de BlueZ).
/// Sérialisée en chaîne ; une adresse invalide est rejetée dès la lecture.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BtAddr(String);

impl BtAddr {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Nœud D-Bus de l'appareil (`dev_AA_BB_…`).
    #[must_use]
    pub fn node(&self) -> String {
        format!("dev_{}", self.0.replace(':', "_"))
    }

    /// Chemin D-Bus complet de l'appareil sur l'adaptateur.
    #[must_use]
    pub fn path(&self) -> String {
        format!("{}/{}", crate::bluez::ADAPTER, self.node())
    }
}

impl FromStr for BtAddr {
    type Err = Error;

    fn from_str(a: &str) -> Result<Self, Error> {
        let ok = a.len() == 17
            && a.split(':').count() == 6
            && a.split(':').all(|o| o.len() == 2 && o.bytes().all(|b| b.is_ascii_hexdigit()));
        if ok { Ok(Self(a.to_ascii_uppercase())) } else { Err(Error::InvalidAddress(a.to_owned())) }
    }
}

impl TryFrom<String> for BtAddr {
    type Error = Error;

    fn try_from(s: String) -> Result<Self, Error> {
        s.parse()
    }
}

impl From<BtAddr> for String {
    fn from(a: BtAddr) -> Self {
        a.0
    }
}

impl AsRef<str> for BtAddr {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        assert!("AA:BB:CC:DD:00:70".parse::<BtAddr>().is_ok());
        assert!("AA:BB:CC:DD:00".parse::<BtAddr>().is_err());
        assert!("zz:A9:B7:AC:00:70".parse::<BtAddr>().is_err());
        assert!("AA:BB:CC:DD:0070".parse::<BtAddr>().is_err());
    }

    #[test]
    fn normalized() {
        let a: BtAddr = "aa:bb:cc:dd:00:7f".parse().unwrap();
        assert_eq!(a.as_str(), "AA:BB:CC:DD:00:7F");
        assert_eq!(a.node(), "dev_AA_BB_CC_DD_00_7F");
        assert_eq!(a.path(), "/org/bluez/hci0/dev_AA_BB_CC_DD_00_7F");
    }

    #[test]
    fn serde_round_trip() {
        let a: BtAddr = serde_json::from_str(r#""aa:bb:cc:dd:00:70""#).unwrap();
        assert_eq!(serde_json::to_string(&a).unwrap(), r#""AA:BB:CC:DD:00:70""#);
        assert!(serde_json::from_str::<BtAddr>(r#""nope""#).is_err());
    }
}
