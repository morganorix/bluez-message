//! État de la liaison Bluetooth de l'iPhone (`org.bluez.Device1`) et relance de la connexion :
//! après une reconnexion spontanée, l'iPhone reste parfois relié sans ses profils (`ServicesResolved` faux).

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, proxy};

use crate::addr::BtAddr;
use crate::engine::Device;
use crate::error::Result;

/// Adaptateur Bluetooth utilisé.
pub(crate) const ADAPTER: &str = "/org/bluez/hci0";

/// Propriété D-Bus textuelle d'un dictionnaire `a{sv}`.
pub(crate) fn prop_str<'a>(p: &'a HashMap<String, OwnedValue>, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| <&str>::try_from(v).ok())
}

/// Propriété D-Bus booléenne ; absente : faux.
pub(crate) fn prop_bool(p: &HashMap<String, OwnedValue>, k: &str) -> bool {
    p.get(k).and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}

#[proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
trait Device {
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn services_resolved(&self) -> zbus::Result<bool>;
}

/// Liaison de l'appareil vue par BlueZ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub connected: bool,
    pub resolved: bool,
}

/// Appareil BlueZ de l'adresse `addr`, sur l'adaptateur `hci0`.
pub struct Phone {
    dev: DeviceProxy<'static>,
}

impl Phone {
    /// Proxy `Device1` de l'appareil `addr` sur le bus système.
    ///
    /// # Errors
    /// Bus système injoignable ou chemin D-Bus refusé.
    pub async fn new(addr: &BtAddr) -> Result<Self> {
        let conn = Connection::system().await?;
        let path = OwnedObjectPath::try_from(addr.path()).map_err(zbus::Error::from)?;
        // Pas de cache : les propriétés sont relues à chaque constat.
        let dev = DeviceProxy::builder(&conn).path(path)?.cache_properties(zbus::proxy::CacheProperties::No).build().await?;
        Ok(Self { dev })
    }

    /// État courant de la liaison (relié, profils résolus).
    ///
    /// # Errors
    /// Lecture des propriétés D-Bus impossible.
    pub async fn link(&self) -> Result<Link> {
        Ok(Link { connected: self.dev.connected().await?, resolved: self.dev.services_resolved().await? })
    }

    /// Connecte les profils de l'appareil (sans effet sur ceux déjà connectés).
    ///
    /// # Errors
    /// Refus ou échec de BlueZ.
    pub async fn connect(&self) -> Result<()> {
        Ok(self.dev.connect().await?)
    }

    /// Déconnecte l'appareil.
    ///
    /// # Errors
    /// Refus ou échec de BlueZ.
    pub async fn disconnect(&self) -> Result<()> {
        Ok(self.dev.disconnect().await?)
    }
}

impl Phone {
    /// Relance complète de la liaison (`Disconnect`, 2 s, `Connect`), bornée à `limit`.
    ///
    /// # Errors
    /// Message traduit : échec de `Disconnect`/`Connect` ou délai dépassé.
    pub async fn reconnect(&self, limit: std::time::Duration) -> std::result::Result<(), String> {
        let r = tokio::time::timeout(limit, async {
            // Déjà déconnecté : `Disconnect` échoue sans gravité.
            let _ = self.disconnect().await;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            self.connect().await
        })
        .await;
        match r {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err(crate::t!("timed out").to_owned()),
        }
    }
}

/// HFP Audio Gateway, MAP Message Server.
const PHONE_UUIDS: [&str; 2] = ["0000111f-0000-1000-8000-00805f9b34fb", "00001132-0000-1000-8000-00805f9b34fb"];

/// Appareils appairés sur `hci0`, triés par nom ; 3 s au plus (la boucle du démon l'attend).
///
/// # Errors
/// BlueZ injoignable ou délai de 3 s dépassé.
pub async fn paired() -> Result<Vec<Device>> {
    tokio::time::timeout(std::time::Duration::from_secs(3), list_paired())
        .await
        .map_err(|_| crate::error::Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "BlueZ")))?
}

async fn list_paired() -> Result<Vec<Device>> {
    let conn = Connection::system().await?;
    let om = zbus::fdo::ObjectManagerProxy::builder(&conn).destination("org.bluez")?.path("/")?.build().await?;
    let objs = om.get_managed_objects().await?;
    let mut out: Vec<Device> = objs
        .iter()
        .filter(|(p, _)| p.as_str().strip_prefix(ADAPTER).is_some_and(|r| r.starts_with("/dev_")))
        .filter_map(|(_, ifs)| {
            let d = ifs.get("org.bluez.Device1")?;
            let get_s = |k: &str| prop_str(d, k).map(str::to_owned);
            // Téléphones seulement : passerelle mains libres (HFP AG) ou serveur de messages (MAP MSE).
            let uuids = d.get("UUIDs").and_then(|v| <Vec<String>>::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
            if !prop_bool(d, "Paired") || !uuids.iter().any(|u| PHONE_UUIDS.iter().any(|p| u.eq_ignore_ascii_case(p))) {
                return None;
            }
            let raw = prop_str(d, "Address")?;
            let Ok(addr) = raw.parse::<BtAddr>() else {
                tracing::debug!(address = raw, "appareil ignoré : adresse illisible");
                return None;
            };
            let name = get_s("Alias").or_else(|| get_s("Name")).unwrap_or_else(|| addr.to_string());
            Some(Device { addr, name, connected: prop_bool(d, "Connected") })
        })
        .collect();
    out.sort_by_key(|d| d.name.to_lowercase());
    Ok(out)
}
