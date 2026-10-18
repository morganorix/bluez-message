//! State of the iPhone's Bluetooth link (`org.bluez.Device1`) and connection restart:
//! after a spontaneous reconnection, the iPhone sometimes stays linked without its profiles (`ServicesResolved` false).

use std::collections::HashMap;

use std::future::Future;
use std::time::Duration;

use zbus::fdo::ManagedObjects;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, proxy};

use crate::addr::BtAddr;
use crate::engine::Device;
use crate::error::{Error, Result};

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

/// Bounds a BlueZ call: past `limit`, `Error::Io(TimedOut)`.
///
/// # Errors
/// Error of the call, or timeout exceeded.
pub(crate) async fn bounded<T, E: Into<Error>>(limit: Duration, call: impl Future<Output = std::result::Result<T, E>>) -> Result<T> {
    tokio::time::timeout(limit, call)
        .await
        .map_err(|_| Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "BlueZ")))?
        .map_err(Into::into)
}

/// D-Bus error named `name` (`org.bluez.Error.…`) returned by a method.
pub(crate) fn is_error(e: &Error, name: &str) -> bool {
    matches!(e, Error::Dbus(zbus::Error::MethodError(n, _, _)) if n.as_str() == name)
}

#[proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
pub(crate) trait Device {
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    fn pair(&self) -> zbus::Result<()>;
    fn cancel_pairing(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn services_resolved(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_trusted(&self, v: bool) -> zbus::Result<()>;
}

/// `Device1` proxy of `addr` on `conn`, without property cache (reread at each check).
///
/// # Errors
/// D-Bus path refused.
pub(crate) async fn device(conn: &Connection, addr: &BtAddr) -> Result<DeviceProxy<'static>> {
    let path = OwnedObjectPath::try_from(addr.path()).map_err(zbus::Error::from)?;
    Ok(DeviceProxy::builder(conn).path(path)?.cache_properties(zbus::proxy::CacheProperties::No).build().await?)
}

#[proxy(interface = "org.bluez.Battery1", default_service = "org.bluez")]
trait Battery {
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<u8>;
}

/// Timeout of a battery level read.
const BATTERY_TIMEOUT: Duration = Duration::from_secs(3);

/// Device link as seen by BlueZ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub connected: bool,
    pub resolved: bool,
}

/// Interface and properties followed by [`Phone::changes`].
type Watched = (&'static str, &'static [&'static str]);

/// Link properties of the device.
const LINK_PROPS: Watched = ("org.bluez.Device1", &["Connected", "ServicesResolved"]);

/// BlueZ device of address `addr`, on the `hci0` adapter.
pub struct Phone {
    dev: DeviceProxy<'static>,
    battery: BatteryProxy<'static>,
}

impl Phone {
    /// `Device1` proxy of device `addr` on the system bus.
    ///
    /// # Errors
    /// System bus unreachable or D-Bus path refused.
    pub async fn new(addr: &BtAddr) -> Result<Self> {
        let conn = Connection::system().await?;
        let dev = device(&conn, addr).await?;
        let battery = BatteryProxy::builder(&conn)
            .path(dev.inner().path().clone())?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        Ok(Self { dev, battery })
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

    /// Battery level reported by the phone (0..=100); `None` if BlueZ does not expose it
    /// (`Battery1` absent: link down or HFP not mounted) or the value is out of range.
    pub async fn battery(&self) -> Option<u8> {
        match bounded(BATTERY_TIMEOUT, self.battery.percentage()).await {
            Ok(p) => (p <= 100).then_some(p),
            Err(e) => {
                tracing::debug!("battery level: {e}");
                None
            }
        }
    }

    /// Changes of `Connected` or `ServicesResolved` signalled by BlueZ (`PropertiesChanged`);
    /// the stream stays silent if the device disappears.
    ///
    /// # Errors
    /// Signal subscription refused.
    pub async fn link_changes(&self) -> Result<impl futures_util::Stream<Item = ()> + Unpin + use<>> {
        self.changes(&[LINK_PROPS]).await
    }

    /// Changes of the battery level, or of the link (which makes `Battery1` appear or disappear).
    ///
    /// # Errors
    /// Signal subscription refused.
    pub async fn battery_changes(&self) -> Result<impl futures_util::Stream<Item = ()> + Unpin + use<>> {
        self.changes(&[LINK_PROPS, ("org.bluez.Battery1", &["Percentage"])]).await
    }

    /// `PropertiesChanged` signals of the device touching one of the `watched` (interface, properties).
    async fn changes(&self, watched: &'static [Watched]) -> Result<impl futures_util::Stream<Item = ()> + Unpin + use<>> {
        let props = zbus::fdo::PropertiesProxy::builder(self.dev.inner().connection())
            .destination("org.bluez")?
            .path(self.dev.inner().path().clone())?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        let changes = props.receive_properties_changed().await?;
        Ok(futures_util::StreamExt::filter_map(changes, move |sig| {
            let hit = sig.args().is_ok_and(|a| {
                watched
                    .iter()
                    .any(|(iface, keys)| a.interface_name() == *iface && keys.iter().any(|k| a.changed_properties().contains_key(k)))
            });
            std::future::ready(hit.then_some(()))
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

/// Major device class "phone" (bits 8-12 of `Class`).
const CLASS_PHONE: u32 = 0x0200;

/// Devices paired on `hci0`, sorted by name; 3 s at most (the daemon loop waits for it).
///
/// # Errors
/// BlueZ unreachable or 3 s timeout exceeded.
pub async fn paired() -> Result<Vec<Device>> {
    bounded(Duration::from_secs(3), list_paired()).await
}

async fn list_paired() -> Result<Vec<Device>> {
    let conn = Connection::system().await?;
    let objs = managed(&conn).await?;
    let mut out: Vec<Device> = devices(&objs)
        // Phones only: hands-free gateway (HFP AG) or message server (MAP MSE).
        .filter(|(_, d)| prop_bool(d, "Paired") && phone_profiles(d))
        .map(|(addr, d)| Device { name: name(d, &addr), connected: prop_bool(d, "Connected"), addr })
        .collect();
    out.sort_by_key(|d| d.name.to_lowercase());
    Ok(out)
}

/// Objects exported by BlueZ (`GetManagedObjects`).
///
/// # Errors
/// BlueZ unreachable.
pub(crate) async fn managed(conn: &Connection) -> Result<ManagedObjects> {
    let om = zbus::fdo::ObjectManagerProxy::builder(conn).destination("org.bluez")?.path("/")?.build().await?;
    Ok(om.get_managed_objects().await?)
}

/// `Device1` properties of the devices of `hci0` whose address is valid.
pub(crate) fn devices(objs: &ManagedObjects) -> impl Iterator<Item = (BtAddr, &HashMap<String, OwnedValue>)> {
    objs.iter()
        .filter(|(p, _)| p.as_str().strip_prefix(ADAPTER).is_some_and(|r| r.starts_with("/dev_")))
        .filter_map(|(_, ifs)| {
            let d = ifs.get("org.bluez.Device1")?;
            let raw = prop_str(d, "Address")?;
            let Ok(addr) = raw.parse::<BtAddr>() else {
                tracing::debug!("device ignored: unreadable address");
                return None;
            };
            Some((addr, d))
        })
}

/// Name shown for a device: `Alias`, otherwise `Name`, otherwise the address.
pub(crate) fn name(d: &HashMap<String, OwnedValue>, addr: &BtAddr) -> String {
    prop_str(d, "Alias").or_else(|| prop_str(d, "Name")).map_or_else(|| addr.to_string(), str::to_owned)
}

/// Device announcing the phone profiles (HFP AG or MAP MSE).
fn phone_profiles(d: &HashMap<String, OwnedValue>) -> bool {
    let uuids = d.get("UUIDs").and_then(|v| <Vec<String>>::try_from(v.try_clone().ok()?).ok()).unwrap_or_default();
    uuids.iter().any(|u| PHONE_UUIDS.iter().any(|p| u.eq_ignore_ascii_case(p)))
}

/// Phone by its profiles, its class or its icon (not yet paired: profiles often unknown).
pub(crate) fn is_phone(d: &HashMap<String, OwnedValue>) -> bool {
    let class = d.get("Class").and_then(|v| u32::try_from(v).ok()).is_some_and(|c| c & 0x1F00 == CLASS_PHONE);
    class || prop_str(d, "Icon").is_some_and(|i| i.starts_with("phone")) || phone_profiles(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn props(kv: &[(&str, Value<'_>)]) -> HashMap<String, OwnedValue> {
        kv.iter().map(|(k, v)| ((*k).to_owned(), OwnedValue::try_from(v.try_clone().unwrap()).unwrap())).collect()
    }

    #[test]
    fn phone_detection() {
        assert!(is_phone(&props(&[("Class", Value::U32(0x5a_020c))])));
        assert!(is_phone(&props(&[("Icon", Value::from("phone"))])));
        assert!(!is_phone(&props(&[("Class", Value::U32(0x24_0404)), ("Icon", Value::from("audio-headset"))])));
        assert!(!is_phone(&props(&[])));
    }

    #[test]
    fn display_name() {
        let addr: BtAddr = "00:11:22:33:44:55".parse().unwrap();
        assert_eq!(name(&props(&[("Alias", Value::from("A")), ("Name", Value::from("N"))]), &addr), "A");
        assert_eq!(name(&props(&[("Name", Value::from("N"))]), &addr), "N");
        assert_eq!(name(&props(&[]), &addr), addr.to_string());
    }
}
