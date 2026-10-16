//! State of the iPhone's Bluetooth link (`org.bluez.Device1`) and connection restart:
//! after a spontaneous reconnection, the iPhone sometimes stays linked without its profiles (`ServicesResolved` false).

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, proxy};

use crate::addr::BtAddr;
use crate::engine::Device;
use crate::error::Result;

/// Bluetooth adapter used.
pub(crate) const ADAPTER: &str = "/org/bluez/hci0";

/// Textual D-Bus property of an `a{sv}` dictionary.
pub(crate) fn prop_str<'a>(p: &'a HashMap<String, OwnedValue>, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| <&str>::try_from(v).ok())
}

/// Boolean D-Bus property; absent: false.
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

/// Device link as seen by BlueZ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub connected: bool,
    pub resolved: bool,
}

/// BlueZ device of address `addr`, on the `hci0` adapter.
pub struct Phone {
    dev: DeviceProxy<'static>,
}

impl Phone {
    /// `Device1` proxy of device `addr` on the system bus.
    ///
    /// # Errors
    /// System bus unreachable or D-Bus path refused.
    pub async fn new(addr: &BtAddr) -> Result<Self> {
        let conn = Connection::system().await?;
        let path = OwnedObjectPath::try_from(addr.path()).map_err(zbus::Error::from)?;
        // No cache: properties are reread at each check.
        let dev = DeviceProxy::builder(&conn).path(path)?.cache_properties(zbus::proxy::CacheProperties::No).build().await?;
        Ok(Self { dev })
    }

    /// Current link state (linked, profiles resolved).
    ///
    /// # Errors
    /// D-Bus properties cannot be read.
    pub async fn link(&self) -> Result<Link> {
        Ok(Link { connected: self.dev.connected().await?, resolved: self.dev.services_resolved().await? })
    }

    /// Connects the device's profiles (no effect on those already connected).
    ///
    /// # Errors
    /// Refusal or failure from BlueZ.
    pub async fn connect(&self) -> Result<()> {
        Ok(self.dev.connect().await?)
    }

    /// Changes of `Connected` or `ServicesResolved` signalled by BlueZ (`PropertiesChanged`);
    /// the stream stays silent if the device disappears.
    ///
    /// # Errors
    /// Signal subscription refused.
    pub async fn link_changes(&self) -> Result<impl futures_util::Stream<Item = ()> + Unpin + use<>> {
        let props = zbus::fdo::PropertiesProxy::builder(self.dev.inner().connection())
            .destination("org.bluez")?
            .path(self.dev.inner().path().clone())?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        let changes = props.receive_properties_changed().await?;
        Ok(futures_util::StreamExt::filter_map(changes, |sig| {
            let link = sig.args().is_ok_and(|a| {
                a.interface_name() == "org.bluez.Device1" && ["Connected", "ServicesResolved"].iter().any(|k| a.changed_properties().contains_key(k))
            });
            std::future::ready(link.then_some(()))
        }))
    }

    /// Disconnects the device.
    ///
    /// # Errors
    /// Refusal or failure from BlueZ.
    pub async fn disconnect(&self) -> Result<()> {
        Ok(self.dev.disconnect().await?)
    }
}

impl Phone {
    /// Full restart of the link (`Disconnect`, 2 s, `Connect`), bounded by `limit`.
    ///
    /// # Errors
    /// Translated message: `Disconnect`/`Connect` failed or timeout exceeded.
    pub async fn reconnect(&self, limit: std::time::Duration) -> std::result::Result<(), String> {
        let r = tokio::time::timeout(limit, async {
            // Already disconnected: `Disconnect` fails harmlessly.
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

/// Devices paired on `hci0`, sorted by name; 3 s at most (the daemon loop waits for it).
///
/// # Errors
/// BlueZ unreachable or 3 s timeout exceeded.
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
            // Phones only: hands-free gateway (HFP AG) or message server (MAP MSE).
            let uuids = d.get("UUIDs").and_then(|v| <Vec<String>>::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
            if !prop_bool(d, "Paired") || !uuids.iter().any(|u| PHONE_UUIDS.iter().any(|p| u.eq_ignore_ascii_case(p))) {
                return None;
            }
            let raw = prop_str(d, "Address")?;
            let Ok(addr) = raw.parse::<BtAddr>() else {
                tracing::debug!(address = raw, "device ignored: unreadable address");
                return None;
            };
            let name = get_s("Alias").or_else(|| get_s("Name")).unwrap_or_else(|| addr.to_string());
            Some(Device { addr, name, connected: prop_bool(d, "Connected") })
        })
        .collect();
    out.sort_by_key(|d| d.name.to_lowercase());
    Ok(out)
}
